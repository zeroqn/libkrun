use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::Path;

use kvm_bindings::__IncompleteArrayField;
use nix::fcntl;
use nix::fcntl::OFlag;
use nix::sys::stat::Mode;
use thiserror::Error;
use vm_memory::Address;
use vm_memory::GuestAddress;
use vm_memory::GuestMemoryBackend;
use vm_memory::GuestMemoryMmap;
use vm_memory::GuestMemoryRegion;
use vmm_sys_util::fam::{self, FamStruct, FamStructWrapper};
use vmm_sys_util::generate_fam_struct_impl;

#[derive(Error, Debug)]
pub enum UdmabufError {
    #[error("system call returned {0}")]
    NixError(nix::Error),

    #[error("could not create ioctl struct: {0}")]
    StructError(fam::Error),

    #[error("page size unavailable")]
    NoPageSize,

    #[error("could not find memory region")]
    RegionNotFound,

    #[error("memory region not backed by memfd")]
    RegionNotFileBacked,

    #[error("provided address and length are out of bounds for a region")]
    OutOfBounds,

    #[error("starting address or length not page aligned")]
    NotPageAligned,

    #[error(
        "udmabuf request needs {items} page runs but the driver accepts at most {limit} \
         (fragmentation, not size: the guest hands one run per page)"
    )]
    TooFragmented { items: usize, limit: usize },
}

pub type Result<T> = std::result::Result<T, UdmabufError>;

const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

/// The driver's `list_limit` module parameter, 1024 by default: a create request
/// naming more runs than this is rejected with `EINVAL`.
const UDMABUF_CREATE_LIST_LIMIT: usize = 1024;

/// One `udmabuf_create_item`: a contiguous run of pages in one memfd.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
struct UdmabufSegment {
    memfd: i32,
    offset: u64,
    size: u64,
}

/// Merge runs that continue each other in the same memfd.
///
/// The guest names one dma-buf entry per page, so an 8 MiB `wl_shm` pool arrives
/// as 2025 runs - over the driver's `list_limit` of 1024, and the ioctl then
/// fails with a bare `EINVAL` *after* the guest has switched to its zero-copy
/// path. Adjacent runs describe the same pages in the same order, so they are
/// one item; non-adjacent or cross-memfd runs stay separate, and the order is
/// preserved because it is the blob's page order.
fn coalesce_segments(segments: Vec<UdmabufSegment>) -> Vec<UdmabufSegment> {
    let mut merged: Vec<UdmabufSegment> = Vec::with_capacity(segments.len());
    for segment in segments {
        match merged.last_mut() {
            Some(previous)
                if previous.memfd == segment.memfd
                    && previous.offset + previous.size == segment.offset =>
            {
                previous.size += segment.size;
            }
            _ => merged.push(segment),
        }
    }
    merged
}

#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
struct UdmabufCreateItem {
    memfd: i32,
    __pad: u32,
    offset: u64,
    size: u64,
}

#[repr(C)]
#[derive(Debug, Default)]
struct UdmabufCreateList {
    flags: u32,
    count: u32,
    list: __IncompleteArrayField<UdmabufCreateItem>,
}

generate_fam_struct_impl!(
    UdmabufCreateList,
    UdmabufCreateItem,
    list,
    u32,
    count,
    65535
);

type UdmabufCreateListWrapper = FamStructWrapper<UdmabufCreateList>;

nix::ioctl_write_ptr!(create_list, b'u', 0x43, UdmabufCreateList);

/// A convenience wrapper for the Linux kernel's udmabuf driver.
///
/// `new()` is the feature probe: cang only asks for the zero-copy SHM path when
/// this succeeds, and the caller (`GpuDevice`) keeps the driver for the device,
/// so the probe and the run cannot disagree.
pub struct UdmabufDriver {
    driver_fd: OwnedFd,
    page_size: usize,
}

impl UdmabufDriver {
    pub fn new() -> Result<UdmabufDriver> {
        const UDMABUF_PATH: &str = "/dev/udmabuf";
        let path = Path::new(UDMABUF_PATH);
        let driver_fd =
            fcntl::open(path, OFlag::O_RDONLY, Mode::empty()).map_err(UdmabufError::NixError)?;

        // SAFETY: _SC_PAGESIZE takes no pointer argument and cannot fail
        // except by returning a non-positive value, which is checked.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        if page_size <= 0 {
            return Err(UdmabufError::NoPageSize);
        }
        let page_size = page_size as usize;

        Ok(UdmabufDriver {
            driver_fd,
            page_size,
        })
    }

    /// Create a udmabuf over the guest memory the `iovecs` name.
    ///
    /// Every page must live in a region vm-memory built with a `FileOffset`,
    /// i.e. one of the guest RAM regions `create_guest_memory` file-backs when
    /// the zero-copy gate is on.
    pub fn create_udmabuf(
        &self,
        mem: &GuestMemoryMmap,
        iovecs: &[(GuestAddress, usize)],
    ) -> Result<OwnedFd> {
        let mut segments: Vec<UdmabufSegment> = Vec::with_capacity(iovecs.len());
        for &(addr, len) in iovecs.iter() {
            let region = mem.find_region(addr).ok_or(UdmabufError::RegionNotFound)?;

            let Some(file_offset) = region.file_offset() else {
                return Err(UdmabufError::RegionNotFileBacked);
            };

            let map_offset = addr
                .checked_sub(region.start_addr().0)
                .ok_or(UdmabufError::OutOfBounds)?;

            if map_offset.0 as usize + len > region.len() as usize {
                return Err(UdmabufError::OutOfBounds);
            }

            let offset = file_offset.start() + map_offset.0;

            if offset as usize % self.page_size != 0 || len % self.page_size != 0 {
                return Err(UdmabufError::NotPageAligned);
            }

            segments.push(UdmabufSegment {
                memfd: file_offset.file().as_raw_fd(),
                offset,
                size: len as u64,
            });
        }

        let segments = coalesce_segments(segments);
        if segments.len() > UDMABUF_CREATE_LIST_LIMIT {
            return Err(UdmabufError::TooFragmented {
                items: segments.len(),
                limit: UDMABUF_CREATE_LIST_LIMIT,
            });
        }

        let mut list = UdmabufCreateListWrapper::from_header(UdmabufCreateList {
            flags: UDMABUF_FLAGS_CLOEXEC,
            ..Default::default()
        })
        .map_err(UdmabufError::StructError)?;
        for segment in segments {
            list.push(UdmabufCreateItem {
                memfd: segment.memfd,
                __pad: 0,
                offset: segment.offset,
                size: segment.size,
            })
            .map_err(UdmabufError::StructError)?;
        }

        // SAFETY: `list` is a correctly sized FamStructWrapper over the
        // UDMABUF_CREATE_LIST ioctl argument, and the descriptors it names are
        // live for the duration of the call.
        let fd = unsafe { create_list(self.driver_fd.as_raw_fd(), list.as_mut_fam_struct_ptr()) }
            .map_err(UdmabufError::NixError)?;

        // SAFETY: a successful UDMABUF_CREATE returns a fresh file descriptor
        // owned by us.
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page_runs(count: usize, memfd: i32) -> Vec<UdmabufSegment> {
        (0..count)
            .map(|index| UdmabufSegment {
                memfd,
                offset: (index * 4096) as u64,
                size: 4096,
            })
            .collect()
    }

    #[test]
    fn coalescing_makes_one_item_of_a_contiguous_pool() {
        // An 8 MiB wl_shm pool arrives as one run per page, which is what the
        // driver's list_limit rejects.
        let merged = coalesce_segments(page_runs(2025, 7));
        assert_eq!(
            merged,
            [UdmabufSegment {
                memfd: 7,
                offset: 0,
                size: 2025 * 4096
            }]
        );
    }

    #[test]
    fn coalescing_keeps_gaps_and_other_memfds_separate() {
        let segments = vec![
            UdmabufSegment {
                memfd: 7,
                offset: 0,
                size: 8192,
            },
            // Gap: a new item.
            UdmabufSegment {
                memfd: 7,
                offset: 16384,
                size: 4096,
            },
            // Same file offsets, different memfd: a new item.
            UdmabufSegment {
                memfd: 9,
                offset: 20480,
                size: 4096,
            },
            // Continues the second memfd's run.
            UdmabufSegment {
                memfd: 9,
                offset: 24576,
                size: 4096,
            },
        ];

        assert_eq!(
            coalesce_segments(segments),
            [
                UdmabufSegment {
                    memfd: 7,
                    offset: 0,
                    size: 8192
                },
                UdmabufSegment {
                    memfd: 7,
                    offset: 16384,
                    size: 4096
                },
                UdmabufSegment {
                    memfd: 9,
                    offset: 20480,
                    size: 8192
                },
            ]
        );
    }

    #[test]
    fn coalescing_reports_fragmentation_over_the_driver_limit() {
        // One page in, one page out: 2048 single-page runs cannot be expressed.
        let mut segments = Vec::new();
        for index in 0..2048u64 {
            segments.push(UdmabufSegment {
                memfd: 7,
                offset: index * 8192,
                size: 4096,
            });
        }
        let merged = coalesce_segments(segments);
        assert_eq!(merged.len(), 2048);
        assert!(merged.len() > UDMABUF_CREATE_LIST_LIMIT);
    }
}
