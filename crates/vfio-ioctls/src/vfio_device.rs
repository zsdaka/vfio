// Copyright © 2019 Intel Corporation
// Copyright (C) 2019 Alibaba Cloud Computing. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0 OR BSD-3-Clause

use std::collections::HashMap;
use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::mem::{self, ManuallyDrop};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::prelude::FileExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use byteorder::{ByteOrder, LittleEndian};
#[cfg(feature = "kvm")]
use kvm_bindings::{
    kvm_device_attr, KVM_DEV_VFIO_GROUP, KVM_DEV_VFIO_GROUP_ADD, KVM_DEV_VFIO_GROUP_DEL,
};
#[cfg(feature = "kvm")]
use kvm_ioctls::DeviceFd;
use log::{debug, error, warn};
#[cfg(all(feature = "mshv", not(feature = "kvm")))]
use mshv_bindings::{
    mshv_device_attr, MSHV_DEV_VFIO_GROUP, MSHV_DEV_VFIO_GROUP_ADD, MSHV_DEV_VFIO_GROUP_DEL,
};
#[cfg(all(feature = "mshv", not(feature = "kvm")))]
use mshv_ioctls::DeviceFd;
use vfio_bindings::bindings::vfio::*;
use vm_memory::{Address, GuestMemory, GuestMemoryRegion, MemoryRegionAddress};
use vmm_sys_util::errno::Error as SysError;
use vmm_sys_util::eventfd::EventFd;
use vmm_sys_util::ioctl::*;

use crate::fam::vec_with_array_field;
use crate::vfio_ioctls::*;
use crate::{Result, VfioError};

#[repr(C)]
#[derive(Debug, Default)]
// A VFIO region structure with an incomplete array for region
// capabilities information.
//
// When the VFIO_DEVICE_GET_REGION_INFO ioctl returns with
// VFIO_REGION_INFO_FLAG_CAPS flag set, it also provides the size of the region
// capabilities information. This is a kernel hint for us to fetch this
// information by calling the same ioctl, but with the argument size set to
// the region plus the capabilities information array length. The kernel will
// then fill our vfio_region_info_with_cap structure with both the region info
// and its capabilities.
struct vfio_region_info_with_cap {
    region_info: vfio_region_info,
    cap_info: __IncompleteArrayField<u8>,
}

/// A safe wrapper over a VFIO container object.
///
/// A VFIO container represents an IOMMU domain, or a set of IO virtual address translation tables.
/// On its own, the container provides little functionality, with all but a couple version and
/// extension query interfaces locked away. The user needs to add a group into the container for
/// the next level of functionality. After some groups are associated with a container, the user
/// can query and set the IOMMU backend, and then build IOVA mapping to access memory.
///
/// Multiple VFIO groups may be associated with the same VFIO container to share the underline
/// address translation mapping tables.
pub struct VfioContainer {
    pub(crate) container: File,
    pub(crate) device_fd: Arc<DeviceFd>,
    pub(crate) groups: Mutex<HashMap<u32, Arc<VfioGroup>>>,
}

impl VfioContainer {
    /// Create a container wrapper object.
    ///
    /// # Arguments
    /// * `device_fd`: file handle of the VFIO device.
    pub fn new(device_fd: Arc<DeviceFd>) -> Result<Self> {
        let container = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/vfio/vfio")
            .map_err(VfioError::OpenContainer)?;

        let container = VfioContainer {
            container,
            device_fd,
            groups: Mutex::new(HashMap::new()),
        };
        container.check_api_version()?;
        container.check_extension(VFIO_TYPE1v2_IOMMU)?;

        Ok(container)
    }

    fn check_api_version(&self) -> Result<()> {
        // Safe as file is vfio container fd and ioctl is defined by kernel.
        let version = unsafe { ioctl(self, VFIO_GET_API_VERSION()) };
        if version as u32 != VFIO_API_VERSION {
            return Err(VfioError::VfioApiVersion);
        }
        Ok(())
    }

    fn check_extension(&self, val: u32) -> Result<()> {
        if val != VFIO_TYPE1_IOMMU && val != VFIO_TYPE1v2_IOMMU {
            return Err(VfioError::VfioInvalidType);
        }

        // Safe as file is vfio container and make sure val is valid.
        let ret = unsafe { ioctl_with_val(self, VFIO_CHECK_EXTENSION(), val.into()) };
        if ret != 1 {
            return Err(VfioError::VfioExtension);
        }

        Ok(())
    }

    fn set_iommu(&self, val: u32) -> Result<()> {
        if val != VFIO_TYPE1_IOMMU && val != VFIO_TYPE1v2_IOMMU {
            return Err(VfioError::VfioInvalidType);
        }

        // Safe as file is vfio container and make sure val is valid.
        let ret = unsafe { ioctl_with_val(self, VFIO_SET_IOMMU(), val.into()) };
        if ret < 0 {
            return Err(VfioError::ContainerSetIOMMU);
        }

        Ok(())
    }

    fn device_add_group(&self, group: &VfioGroup) -> Result<()> {
        let group_fd_ptr = &group.as_raw_fd() as *const i32;

        #[cfg(feature = "kvm")]
        let dev_attr = kvm_device_attr {
            flags: 0,
            group: KVM_DEV_VFIO_GROUP,
            attr: u64::from(KVM_DEV_VFIO_GROUP_ADD),
            addr: group_fd_ptr as u64,
        };

        #[cfg(all(feature = "mshv", not(feature = "kvm")))]
        let dev_attr = mshv_device_attr {
            flags: 0,
            group: MSHV_DEV_VFIO_GROUP,
            attr: u64::from(MSHV_DEV_VFIO_GROUP_ADD),
            addr: group_fd_ptr as u64,
        };

        self.device_fd
            .set_device_attr(&dev_attr)
            .map_err(VfioError::SetDeviceAttr)
    }

    fn device_del_group(&self, group: &VfioGroup) -> Result<()> {
        let group_fd_ptr = &group.as_raw_fd() as *const i32;
        #[cfg(feature = "kvm")]
        let dev_attr = kvm_device_attr {
            flags: 0,
            group: KVM_DEV_VFIO_GROUP,
            attr: u64::from(KVM_DEV_VFIO_GROUP_DEL),
            addr: group_fd_ptr as u64,
        };

        #[cfg(all(feature = "mshv", not(feature = "kvm")))]
        let dev_attr = mshv_device_attr {
            flags: 0,
            group: MSHV_DEV_VFIO_GROUP,
            attr: u64::from(MSHV_DEV_VFIO_GROUP_DEL),
            addr: group_fd_ptr as u64,
        };

        self.device_fd
            .set_device_attr(&dev_attr)
            .map_err(VfioError::SetDeviceAttr)
    }

    fn get_group(&self, group_id: u32) -> Result<Arc<VfioGroup>> {
        // Safe because there's no legal way to break the lock.
        let mut hash = self.groups.lock().unwrap();
        if let Some(entry) = hash.get(&group_id) {
            return Ok(entry.clone());
        }

        let group = Arc::new(VfioGroup::new(group_id)?);

        // Bind the new group object to the container.
        // Safe as we are the owner of group and container_raw_fd which are valid value,
        // and we verify the ret value
        let container_raw_fd = self.as_raw_fd();
        let ret = unsafe { ioctl_with_ref(&*group, VFIO_GROUP_SET_CONTAINER(), &container_raw_fd) };
        if ret < 0 {
            return Err(VfioError::GroupSetContainer);
        }

        // Initialize the IOMMU backend driver after binding the first group object.
        if hash.len() == 0 {
            if let Err(e) = self.set_iommu(VFIO_TYPE1v2_IOMMU) {
                let _ = unsafe {
                    ioctl_with_ref(&*group, VFIO_GROUP_UNSET_CONTAINER(), &self.as_raw_fd())
                };
                return Err(e);
            }
        }

        // Add the new group object to the hypervisor driver.
        if let Err(e) = self.device_add_group(&group) {
            let _ =
                unsafe { ioctl_with_ref(&*group, VFIO_GROUP_UNSET_CONTAINER(), &self.as_raw_fd()) };
            return Err(e);
        }

        hash.insert(group_id, group.clone());

        Ok(group)
    }

    fn put_group(&self, group: Arc<VfioGroup>) {
        // Safe because there's no legal way to break the lock.
        let mut hash = self.groups.lock().unwrap();

        // Clean up the group when the last user releases reference to the group, three reference
        // count for:
        // - one reference held by the last device object
        // - one reference cloned in VfioDevice.drop() and passed into here
        // - one reference held by the groups hashmap
        if Arc::strong_count(&group) == 3 {
            match self.device_del_group(&group) {
                Ok(_) => {}
                Err(e) => {
                    error!("Could not delete VFIO group: {:?}", e);
                    return;
                }
            }
            // Safe as we are the owner of self and container_raw_fd which are valid value.
            let ret =
                unsafe { ioctl_with_ref(&*group, VFIO_GROUP_UNSET_CONTAINER(), &self.as_raw_fd()) };
            if ret < 0 {
                error!("Could not unbind VFIO group: {:?}", group.id());
                return;
            }
            hash.remove(&group.id());
        }
    }

    /// Map a region of guest memory regions into the vfio container's iommu table.
    ///
    /// # Parameters
    /// * iova: IO virtual address to mapping the memory.
    /// * size: size of the memory region.
    /// * user_addr: host virtual address for the guest memory region to map.
    pub fn vfio_dma_map(&self, iova: u64, size: u64, user_addr: u64) -> Result<()> {
        let dma_map = vfio_iommu_type1_dma_map {
            argsz: mem::size_of::<vfio_iommu_type1_dma_map>() as u32,
            flags: VFIO_DMA_MAP_FLAG_READ | VFIO_DMA_MAP_FLAG_WRITE,
            vaddr: user_addr,
            iova,
            size,
        };

        // Safe as file is vfio container, dma_map is constructed by us, and
        // we check the return value
        let ret = unsafe { ioctl_with_ref(self, VFIO_IOMMU_MAP_DMA(), &dma_map) };
        if ret != 0 {
            return Err(VfioError::IommuDmaMap);
        }

        Ok(())
    }

    /// Unmap a region of guest memory regions into the vfio container's iommu table.
    ///
    /// # Parameters
    /// * iova: IO virtual address to mapping the memory.
    /// * size: size of the memory region.
    pub fn vfio_dma_unmap(&self, iova: u64, size: u64) -> Result<()> {
        let mut dma_unmap = vfio_iommu_type1_dma_unmap {
            argsz: mem::size_of::<vfio_iommu_type1_dma_unmap>() as u32,
            flags: 0,
            iova,
            size,
        };

        // Safe as file is vfio container, dma_unmap is constructed by us, and
        // we check the return value
        let ret = unsafe { ioctl_with_mut_ref(self, VFIO_IOMMU_UNMAP_DMA(), &mut dma_unmap) };
        if ret != 0 || dma_unmap.size != size {
            return Err(VfioError::IommuDmaUnmap);
        }

        Ok(())
    }

    /// Add all guest memory regions into the vfio container's iommu table.
    ///
    /// # Parameters
    /// * mem: pinned guest memory which could be accessed by devices binding to the container.
    pub fn vfio_map_guest_memory<M: GuestMemory>(&self, mem: &M) -> Result<()> {
        mem.iter().try_for_each(|region| {
            let host_addr = region
                .get_host_address(MemoryRegionAddress(0))
                .map_err(|_| VfioError::IommuDmaMap)?;
            self.vfio_dma_map(
                region.start_addr().raw_value(),
                region.len() as u64,
                host_addr as u64,
            )
        })
    }

    /// Remove all guest memory regions from the vfio container's iommu table.
    ///
    /// The vfio kernel driver and device hardware couldn't access this guest memory after
    /// returning from the function.
    ///
    /// # Parameters
    /// * mem: pinned guest memory which could be accessed by devices binding to the container.
    pub fn vfio_unmap_guest_memory<M: GuestMemory>(&self, mem: &M) -> Result<()> {
        mem.iter().try_for_each(|region| {
            self.vfio_dma_unmap(region.start_addr().raw_value(), region.len() as u64)
        })
    }
}

impl AsRawFd for VfioContainer {
    fn as_raw_fd(&self) -> RawFd {
        self.container.as_raw_fd()
    }
}

/// A safe wrapper over a VFIO container object.
///
/// The Linux VFIO frameworks supports multiple devices per group, and multiple groups per
/// container. But current implementation assumes there's only one device per group to simplify
/// implementation. With such an assumption, the `VfioGroup` becomes an internal implementation
/// details.
pub struct VfioGroup {
    pub(crate) id: u32,
    pub(crate) group: File,
}

impl VfioGroup {
    /// Create a new VfioGroup object.
    ///
    /// # Parameters
    /// * `id`: ID(index) of the VFIO group file.
    fn new(id: u32) -> Result<Self> {
        let group_path = Path::new("/dev/vfio").join(id.to_string());
        let group = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&group_path)
            .map_err(|e| VfioError::OpenGroup(e, id.to_string()))?;

        let mut group_status = vfio_group_status {
            argsz: mem::size_of::<vfio_group_status>() as u32,
            flags: 0,
        };
        // Safe as we are the owner of group and group_status which are valid value.
        let ret = unsafe { ioctl_with_mut_ref(&group, VFIO_GROUP_GET_STATUS(), &mut group_status) };
        if ret < 0 {
            return Err(VfioError::GetGroupStatus);
        }

        if group_status.flags != VFIO_GROUP_FLAGS_VIABLE {
            return Err(VfioError::GroupViable);
        }

        Ok(VfioGroup { id, group })
    }

    fn id(&self) -> u32 {
        self.id
    }

    fn get_device(&self, name: &Path) -> Result<VfioDeviceInfo> {
        let uuid_osstr = name.file_name().ok_or(VfioError::InvalidPath)?;
        let uuid_str = uuid_osstr.to_str().ok_or(VfioError::InvalidPath)?;
        let path: CString = CString::new(uuid_str.as_bytes()).expect("CString::new() failed");
        let path_ptr = path.as_ptr();

        // Safe as we are the owner of self and path_ptr which are valid value.
        let fd = unsafe { ioctl_with_ptr(self, VFIO_GROUP_GET_DEVICE_FD(), path_ptr) };
        if fd < 0 {
            return Err(VfioError::GroupGetDeviceFD);
        }

        // Safe as fd is valid FD
        let device = unsafe { File::from_raw_fd(fd) };

        let mut dev_info = vfio_device_info {
            argsz: mem::size_of::<vfio_device_info>() as u32,
            flags: 0,
            num_regions: 0,
            num_irqs: 0,
        };
        // Safe as we are the owner of dev and dev_info which are valid value,
        // and we verify the return value.
        let ret = unsafe { ioctl_with_mut_ref(&device, VFIO_DEVICE_GET_INFO(), &mut dev_info) };
        if ret < 0
            || (dev_info.flags & VFIO_DEVICE_FLAGS_PCI) == 0
            || dev_info.num_regions < VFIO_PCI_CONFIG_REGION_INDEX + 1
            || dev_info.num_irqs < VFIO_PCI_MSIX_IRQ_INDEX + 1
        {
            return Err(VfioError::VfioDeviceGetInfo);
        }

        Ok(VfioDeviceInfo {
            device,
            flags: dev_info.flags,
            num_regions: dev_info.num_regions,
            num_irqs: dev_info.num_irqs,
        })
    }
}

impl AsRawFd for VfioGroup {
    fn as_raw_fd(&self) -> RawFd {
        self.group.as_raw_fd()
    }
}

/// Represent one area of the sparse mmap
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VfioRegionSparseMmapArea {
    /// Offset of mmap'able area within region
    pub offset: u64,
    /// Size of mmap'able area
    pub size: u64,
}

/// List of sparse mmap areas
#[derive(Clone, Debug, PartialEq)]
pub struct VfioRegionInfoCapSparseMmap {
    /// List of areas
    pub areas: Vec<VfioRegionSparseMmapArea>,
}

/// Represent a specific device by providing type and subtype
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VfioRegionInfoCapType {
    /// Device type
    pub type_: u32,
    /// Device subtype
    pub subtype: u32,
}

/// Carry NVLink SSA TGT information
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VfioRegionInfoCapNvlink2Ssatgt {
    /// TGT value
    pub tgt: u64,
}

/// Carry NVLink link speed information
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VfioRegionInfoCapNvlink2Lnkspd {
    /// Link speed value
    pub link_speed: u32,
}

/// List of capabilities that can be related to a region.
#[derive(Clone, Debug, PartialEq)]
pub enum VfioRegionInfoCap {
    /// Sparse memory mapping type
    SparseMmap(VfioRegionInfoCapSparseMmap),
    /// Capability holding type and subtype
    Type(VfioRegionInfoCapType),
    /// Indicate if the region is mmap'able with the presence of MSI-X region
    MsixMappable,
    /// NVLink SSA TGT
    Nvlink2Ssatgt(VfioRegionInfoCapNvlink2Ssatgt),
    /// NVLink Link Speed
    Nvlink2Lnkspd(VfioRegionInfoCapNvlink2Lnkspd),
}

/// Information abour VFIO MMIO region.
#[derive(Clone, Debug)]
pub struct VfioRegion {
    pub(crate) flags: u32,
    pub(crate) size: u64,
    pub(crate) offset: u64,
    pub(crate) caps: Vec<VfioRegionInfoCap>,
}

/// Information abour VFIO interrupts.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct VfioIrq {
    /// Flags for irq.
    pub flags: u32,
    /// Staring index.
    pub index: u32,
    /// Number interrupts.
    pub count: u32,
}

struct VfioDeviceInfo {
    device: File,
    flags: u32,
    num_regions: u32,
    num_irqs: u32,
}

impl VfioDeviceInfo {
    fn get_irqs(&self) -> Result<HashMap<u32, VfioIrq>> {
        let mut irqs: HashMap<u32, VfioIrq> = HashMap::new();

        for index in 0..self.num_irqs {
            let mut irq_info = vfio_irq_info {
                argsz: mem::size_of::<vfio_irq_info>() as u32,
                flags: 0,
                index,
                count: 0,
            };

            let ret = unsafe {
                ioctl_with_mut_ref(&self.device, VFIO_DEVICE_GET_IRQ_INFO(), &mut irq_info)
            };
            if ret < 0 {
                warn!("Could not get VFIO IRQ info for index {:}", index);
                continue;
            }

            let irq = VfioIrq {
                flags: irq_info.flags,
                index,
                count: irq_info.count,
            };

            debug!("IRQ #{}", index);
            debug!("\tflag 0x{:x}", irq.flags);
            debug!("\tindex {}", irq.index);
            debug!("\tcount {}", irq.count);

            irqs.insert(index, irq);
        }

        Ok(irqs)
    }

    fn get_region_map(
        &self,
        region: &mut VfioRegion,
        region_info: &vfio_region_info,
    ) -> Result<()> {
        let region_info_size: u32 = mem::size_of::<vfio_region_info>() as u32;

        if region_info.flags & VFIO_REGION_INFO_FLAG_CAPS == 0
            || region_info.argsz <= region_info_size
        {
            // There is not capabilities information for that region, we can
            // just return.
            return Ok(());
        }

        // There is a capability information for that region, we have to call
        // VFIO_DEVICE_GET_REGION_INFO with a vfio_region_with_cap structure
        // and the hinted size.
        let cap_len: usize = (region_info.argsz - region_info_size) as usize;
        let mut region_with_cap = vec_with_array_field::<vfio_region_info_with_cap, u8>(cap_len);
        region_with_cap[0].region_info.argsz = region_info.argsz;
        region_with_cap[0].region_info.flags = 0;
        region_with_cap[0].region_info.index = region_info.index;
        region_with_cap[0].region_info.cap_offset = 0;
        region_with_cap[0].region_info.size = 0;
        region_with_cap[0].region_info.offset = 0;
        // Safe as we are the owner of dev and region_info which are valid value,
        // and we verify the return value.
        let ret = unsafe {
            ioctl_with_mut_ref(
                &self.device,
                VFIO_DEVICE_GET_REGION_INFO(),
                &mut (region_with_cap[0].region_info),
            )
        };
        if ret < 0 {
            return Err(VfioError::VfioDeviceGetRegionInfo(SysError::new(ret)));
        }

        // region_with_cap[0] may contain different types of structure
        // depending on the capability type, but all of them begin with
        // vfio_info_cap_header in order to identify the capability type,
        // version and if there's another capability after this one.
        // It is safe to convert region_with_cap[0] with an offset of
        // cap_offset into vfio_info_cap_header pointer and access its
        // elements, as long as cap_offset is greater than region_info_size.
        if region_with_cap[0].region_info.cap_offset >= region_info_size {
            let mut next_cap_offset = region_with_cap[0].region_info.cap_offset;
            let info_ptr = &region_with_cap[0] as *const vfio_region_info_with_cap as *const u8;

            while next_cap_offset >= region_info_size {
                let cap_header = unsafe {
                    *(info_ptr.offset(next_cap_offset as isize) as *const vfio_info_cap_header)
                };

                match u32::from(cap_header.id) {
                    VFIO_REGION_INFO_CAP_SPARSE_MMAP => {
                        let sparse_mmap = unsafe {
                            info_ptr.offset(next_cap_offset as isize)
                                as *const vfio_region_info_cap_sparse_mmap
                        };
                        let nr_areas = unsafe { (*sparse_mmap).nr_areas };
                        let areas = unsafe { (*sparse_mmap).areas.as_slice(nr_areas as usize) };

                        let cap = VfioRegionInfoCapSparseMmap {
                            areas: areas
                                .iter()
                                .map(|a| VfioRegionSparseMmapArea {
                                    offset: a.offset,
                                    size: a.size,
                                })
                                .collect(),
                        };
                        region.caps.push(VfioRegionInfoCap::SparseMmap(cap));
                    }
                    VFIO_REGION_INFO_CAP_TYPE => {
                        let type_ = unsafe {
                            *(info_ptr.offset(next_cap_offset as isize)
                                as *const vfio_region_info_cap_type)
                        };
                        let cap = VfioRegionInfoCapType {
                            type_: type_.type_,
                            subtype: type_.subtype,
                        };
                        region.caps.push(VfioRegionInfoCap::Type(cap));
                    }
                    VFIO_REGION_INFO_CAP_MSIX_MAPPABLE => {
                        region.caps.push(VfioRegionInfoCap::MsixMappable);
                    }
                    VFIO_REGION_INFO_CAP_NVLINK2_SSATGT => {
                        let nvlink2_ssatgt = unsafe {
                            *(info_ptr.offset(next_cap_offset as isize)
                                as *const vfio_region_info_cap_nvlink2_ssatgt)
                        };
                        let cap = VfioRegionInfoCapNvlink2Ssatgt {
                            tgt: nvlink2_ssatgt.tgt,
                        };
                        region.caps.push(VfioRegionInfoCap::Nvlink2Ssatgt(cap));
                    }
                    VFIO_REGION_INFO_CAP_NVLINK2_LNKSPD => {
                        let nvlink2_lnkspd = unsafe {
                            *(info_ptr.offset(next_cap_offset as isize)
                                as *const vfio_region_info_cap_nvlink2_lnkspd)
                        };
                        let cap = VfioRegionInfoCapNvlink2Lnkspd {
                            link_speed: nvlink2_lnkspd.link_speed,
                        };
                        region.caps.push(VfioRegionInfoCap::Nvlink2Lnkspd(cap));
                    }
                    _ => {}
                }

                next_cap_offset = cap_header.next;
            }
        }

        Ok(())
    }

    fn get_regions(&self) -> Result<Vec<VfioRegion>> {
        let mut regions: Vec<VfioRegion> = Vec::new();

        for i in VFIO_PCI_BAR0_REGION_INDEX..self.num_regions {
            let argsz: u32 = mem::size_of::<vfio_region_info>() as u32;

            let mut reg_info = vfio_region_info {
                argsz,
                flags: 0,
                index: i,
                cap_offset: 0,
                size: 0,
                offset: 0,
            };
            // Safe as we are the owner of dev and reg_info which are valid value,
            // and we verify the return value.
            let ret = unsafe {
                ioctl_with_mut_ref(&self.device, VFIO_DEVICE_GET_REGION_INFO(), &mut reg_info)
            };
            if ret < 0 {
                error!("Could not get region #{} info", i);
                continue;
            }

            let mut region = VfioRegion {
                flags: reg_info.flags,
                size: reg_info.size,
                offset: reg_info.offset,
                caps: Vec::new(),
            };

            if let Err(e) = self.get_region_map(&mut region, &reg_info) {
                error!("Could not get region #{} map {}", i, e);
                continue;
            }

            debug!("Region #{}", i);
            debug!("\tflag 0x{:x}", region.flags);
            debug!("\tsize 0x{:x}", region.size);
            debug!("\toffset 0x{:x}", region.offset);

            regions.push(region);
        }

        Ok(regions)
    }
}

/// Vfio device to access underline hardware devices.
///
/// The VFIO device API includes ioctls for describing the device, the I/O regions and their
/// read/write/mmap offsets on the device descriptor, as well as mechanisms for describing and
/// registering interrupt notifications.
pub struct VfioDevice {
    pub(crate) device: ManuallyDrop<File>,
    pub(crate) flags: u32,
    pub(crate) regions: Vec<VfioRegion>,
    pub(crate) irqs: HashMap<u32, VfioIrq>,
    pub(crate) group: Arc<VfioGroup>,
    pub(crate) container: Arc<VfioContainer>,
}

impl VfioDevice {
    /// Create a new vfio device, then guest read/write on this device could be transferred into kernel vfio.
    ///
    /// # Parameters
    /// * `sysfspath`: specify the vfio device path in sys file system.
    /// * `container`: the new VFIO device object will bind to this container object.
    pub fn new(sysfspath: &Path, container: Arc<VfioContainer>) -> Result<Self> {
        let uuid_path: PathBuf = [sysfspath, Path::new("iommu_group")].iter().collect();
        let group_path = uuid_path.read_link().map_err(|_| VfioError::InvalidPath)?;
        let group_osstr = group_path.file_name().ok_or(VfioError::InvalidPath)?;
        let group_str = group_osstr.to_str().ok_or(VfioError::InvalidPath)?;
        let group_id = group_str
            .parse::<u32>()
            .map_err(|_| VfioError::InvalidPath)?;

        let group = container.get_group(group_id)?;
        let device_info = group.get_device(sysfspath)?;
        let regions = device_info.get_regions()?;
        let irqs = device_info.get_irqs()?;

        Ok(VfioDevice {
            device: ManuallyDrop::new(device_info.device),
            flags: device_info.flags,
            regions,
            irqs,
            group,
            container,
        })
    }

    /// VFIO device reset only if the device supports being reset.
    pub fn reset(&self) {
        if self.flags & VFIO_DEVICE_FLAGS_RESET != 0 {
            unsafe { ioctl(self, VFIO_DEVICE_RESET()) };
        }
    }

    /// Get information about VFIO IRQs.
    ///
    /// # Arguments
    /// * `irq_index` - The type (INTX, MSI or MSI-X) of interrupts to enable.
    pub fn get_irq_info(&self, irq_index: u32) -> Option<&VfioIrq> {
        self.irqs.get(&irq_index)
    }

    /// Trigger a VFIO device IRQ from userspace.
    ///
    /// Once a signaling mechanism is set, DATA_BOOL or DATA_NONE can be used with ACTION_TRIGGER
    /// to perform kernel level interrupt loopback testing from userspace (ie. simulate hardware
    /// triggering).
    ///
    /// # Arguments
    /// * `irq_index` - The type (INTX, MSI or MSI-X) of interrupts to enable.
    /// * `vector` - The sub-index into the interrupt group of `irq_index`.
    pub fn trigger_irq(&self, irq_index: u32, vector: u32) -> Result<()> {
        let irq = self
            .irqs
            .get(&irq_index)
            .ok_or(VfioError::VfioDeviceSetIrq)?;
        if irq.count < vector {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        let irq_set = vfio_irq_set {
            argsz: mem::size_of::<vfio_irq_set>() as u32,
            flags: VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER,
            index: irq_index,
            start: vector,
            count: 1,
            ..Default::default()
        };

        // Safe as we are the owner of self and irq_set which are valid value
        let ret = unsafe { ioctl_with_ref(self, VFIO_DEVICE_SET_IRQS(), &irq_set) };
        if ret < 0 {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        Ok(())
    }

    /// Enables a VFIO device IRQs.
    /// This maps a vector of EventFds to all VFIO managed interrupts. In other words, this
    /// tells VFIO which EventFd to write into whenever one of the device interrupt vector
    /// is triggered.
    ///
    /// # Arguments
    /// * `irq_index` - The type (INTX, MSI or MSI-X) of interrupts to enable.
    /// * `event_fds` - The EventFds vector that matches all the supported VFIO interrupts.
    pub fn enable_irq(&self, irq_index: u32, event_fds: Vec<&EventFd>) -> Result<()> {
        let irq = self
            .irqs
            .get(&irq_index)
            .ok_or(VfioError::VfioDeviceSetIrq)?;
        if irq.count == 0 || (irq.count as usize) < event_fds.len() {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        let mut irq_set = vec_with_array_field::<vfio_irq_set, u32>(event_fds.len());
        irq_set[0].argsz = mem::size_of::<vfio_irq_set>() as u32
            + (event_fds.len() * mem::size_of::<u32>()) as u32;
        irq_set[0].flags = VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER;
        irq_set[0].index = irq_index;
        irq_set[0].start = 0;
        irq_set[0].count = event_fds.len() as u32;

        {
            // irq_set.data could be none, bool or fd according to flags, so irq_set.data
            // is u8 default, here irq_set.data is a vector of fds as u32, so 4 default u8
            // are combined together as u32 for each fd.
            // It is safe as enough space is reserved through
            // vec_with_array_field(u32)<event_fds.len()>.
            let fds = unsafe {
                irq_set[0]
                    .data
                    .as_mut_slice(event_fds.len() * mem::size_of::<u32>())
            };
            for (index, event_fd) in event_fds.iter().enumerate() {
                let fds_offset = index * mem::size_of::<u32>();
                let fd = &mut fds[fds_offset..fds_offset + mem::size_of::<u32>()];
                LittleEndian::write_u32(fd, event_fd.as_raw_fd() as u32);
            }
        }

        // Safe as we are the owner of self and irq_set which are valid value
        let ret = unsafe { ioctl_with_ref(self, VFIO_DEVICE_SET_IRQS(), &irq_set[0]) };
        if ret < 0 {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        Ok(())
    }

    /// Disables a VFIO device IRQs
    ///
    /// # Arguments
    /// * `irq_index` - The type (INTX, MSI or MSI-X) of interrupts to disable.
    pub fn disable_irq(&self, irq_index: u32) -> Result<()> {
        let irq = self
            .irqs
            .get(&irq_index)
            .ok_or(VfioError::VfioDeviceSetIrq)?;
        if irq.count == 0 {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        // Individual subindex interrupts can be disabled using the -1 value for DATA_EVENTFD or
        // the index can be disabled as a whole with: flags = (DATA_NONE|ACTION_TRIGGER), count = 0.
        let mut irq_set = vec_with_array_field::<vfio_irq_set, u32>(0);
        irq_set[0].argsz = mem::size_of::<vfio_irq_set>() as u32;
        irq_set[0].flags = VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_TRIGGER;
        irq_set[0].index = irq_index;
        irq_set[0].start = 0;
        irq_set[0].count = 0;

        // Safe as we are the owner of self and irq_set which are valid value
        let ret = unsafe { ioctl_with_ref(self, VFIO_DEVICE_SET_IRQS(), &irq_set[0]) };
        if ret < 0 {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        Ok(())
    }

    /// Unmask IRQ
    ///
    /// # Arguments
    /// * `irq_index` - The type (INTX, MSI or MSI-X) of interrupts to unmask.
    pub fn unmask_irq(&self, irq_index: u32) -> Result<()> {
        let irq = self
            .irqs
            .get(&irq_index)
            .ok_or(VfioError::VfioDeviceSetIrq)?;
        if irq.count == 0 {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        let mut irq_set = vec_with_array_field::<vfio_irq_set, u32>(0);
        irq_set[0].argsz = mem::size_of::<vfio_irq_set>() as u32;
        irq_set[0].flags = VFIO_IRQ_SET_DATA_NONE | VFIO_IRQ_SET_ACTION_UNMASK;
        irq_set[0].index = irq_index;
        irq_set[0].start = 0;
        irq_set[0].count = 1;

        // Safe as we are the owner of self and irq_set which are valid value
        let ret = unsafe { ioctl_with_ref(self, VFIO_DEVICE_SET_IRQS(), &irq_set[0]) };
        if ret < 0 {
            return Err(VfioError::VfioDeviceSetIrq);
        }

        Ok(())
    }

    /// Wrapper to enable MSI IRQs.
    pub fn enable_msi(&self, fds: Vec<&EventFd>) -> Result<()> {
        self.enable_irq(VFIO_PCI_MSI_IRQ_INDEX, fds)
    }

    /// Wrapper to disable MSI IRQs.
    pub fn disable_msi(&self) -> Result<()> {
        self.disable_irq(VFIO_PCI_MSI_IRQ_INDEX)
    }

    /// Wrapper to enable MSI-X IRQs.
    pub fn enable_msix(&self, fds: Vec<&EventFd>) -> Result<()> {
        self.enable_irq(VFIO_PCI_MSIX_IRQ_INDEX, fds)
    }

    /// Wrapper to disable MSI-X IRQs.
    pub fn disable_msix(&self) -> Result<()> {
        self.disable_irq(VFIO_PCI_MSIX_IRQ_INDEX)
    }

    /// Get a region's flag.
    ///
    /// # Arguments
    /// * `index` - The index of memory region.
    pub fn get_region_flags(&self, index: u32) -> u32 {
        match self.regions.get(index as usize) {
            Some(v) => v.flags,
            None => 0,
        }
    }

    /// Get a region's offset.
    ///
    /// # Arguments
    /// * `index` - The index of memory region.
    pub fn get_region_offset(&self, index: u32) -> u64 {
        match self.regions.get(index as usize) {
            Some(v) => v.offset,
            None => 0,
        }
    }

    /// Get a region's size.
    ///
    /// # Arguments
    /// * `index` - The index of memory region.
    pub fn get_region_size(&self, index: u32) -> u64 {
        match self.regions.get(index as usize) {
            Some(v) => v.size,
            None => {
                warn!("get_region_size with invalid index: {}", index);
                0
            }
        }
    }

    /// Get region's list of capabilities
    ///
    /// # Arguments
    /// * `index` - The index of memory region.
    pub fn get_region_caps(&self, index: u32) -> Vec<VfioRegionInfoCap> {
        match self.regions.get(index as usize) {
            Some(v) => v.caps.clone(),
            None => {
                warn!("get_region_caps with invalid index: {}", index);
                Vec::new()
            }
        }
    }

    /// Read region's data from VFIO device into buf
    ///
    /// # Arguments
    /// * `index`: region num
    /// * `buf`: data destination and buf length is read size
    /// * `addr`: offset in the region
    pub fn region_read(&self, index: u32, buf: &mut [u8], addr: u64) {
        let region: &VfioRegion;
        match self.regions.get(index as usize) {
            Some(v) => region = v,
            None => {
                warn!("region read with invalid index: {}", index);
                return;
            }
        }

        let size = buf.len() as u64;
        if size > region.size || addr + size > region.size {
            warn!(
                "region read with invalid parameter, add: {}, size: {}",
                addr, size
            );
            return;
        }

        if let Err(e) = self.device.read_exact_at(buf, region.offset + addr) {
            warn!(
                "Failed to read region in index: {}, addr: {}, error: {}",
                index, addr, e
            );
        }
    }

    /// Write the data from buf into a vfio device region
    ///
    /// # Arguments
    /// * `index`: region num
    /// * `buf`: data src and buf length is write size
    /// * `addr`: offset in the region
    pub fn region_write(&self, index: u32, buf: &[u8], addr: u64) {
        let stub: &VfioRegion;
        match self.regions.get(index as usize) {
            Some(v) => stub = v,
            None => {
                warn!("region write with invalid index: {}", index);
                return;
            }
        }

        let size = buf.len() as u64;
        if size > stub.size
            || addr + size > stub.size
            || (stub.flags & VFIO_REGION_INFO_FLAG_WRITE) == 0
        {
            warn!(
                "region write with invalid parameter, add: {}, size: {}",
                addr, size
            );
            return;
        }

        if let Err(e) = self.device.write_all_at(buf, stub.offset + addr) {
            warn!(
                "Failed to write region in index: {}, addr: {}, error: {}",
                index, addr, e
            );
        }
    }

    /// Return the maximum numner of interrupts a VFIO device can request.
    pub fn max_interrupts(&self) -> u32 {
        let mut max_interrupts = 0;
        let irq_indexes = vec![
            VFIO_PCI_INTX_IRQ_INDEX,
            VFIO_PCI_MSI_IRQ_INDEX,
            VFIO_PCI_MSIX_IRQ_INDEX,
        ];

        for index in irq_indexes {
            if let Some(irq_info) = self.irqs.get(&index) {
                if irq_info.count > max_interrupts {
                    max_interrupts = irq_info.count;
                }
            }
        }

        max_interrupts
    }
}

impl AsRawFd for VfioDevice {
    fn as_raw_fd(&self) -> RawFd {
        self.device.as_raw_fd()
    }
}

impl Drop for VfioDevice {
    fn drop(&mut self) {
        unsafe {
            ManuallyDrop::drop(&mut self.device);
        }
        self.container.put_group(self.group.clone());
    }
}
