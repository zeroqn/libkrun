// Copyright 2019 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;

use std::convert::TryInto;
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use vm_memory::ByteValued;

use super::super::linux_errno::linux_error;
use super::bindings;
use super::descriptor_utils::{Reader, Writer};
use super::filesystem::{
    Context, DirEntry, Entry, Extensions, FileSystem, GetxattrReply, ListxattrReply, SecContext,
    ZeroCopyReader, ZeroCopyWriter,
};
use super::fs_utils::einval;
use super::fuse::*;
use super::{FsError as Error, Result};
use crate::virtio::VirtioShmRegion;

#[cfg(windows)]
use windows_sys::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};

const MAX_BUFFER_SIZE: u32 = 1 << 20;
const BUFFER_HEADER_SIZE: u32 = 0x1000;
const DIRENT_PADDING: [u8; 8] = [0; 8];

struct ZCReader<'a>(Reader<'a>);

impl ZeroCopyReader for ZCReader<'_> {
    fn read_to(&mut self, f: &File, count: usize, off: u64) -> io::Result<usize> {
        self.0.read_to_at(f, count, off)
    }
}

impl io::Read for ZCReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

struct ZCWriter<'a>(Writer<'a>);

impl ZeroCopyWriter for ZCWriter<'_> {
    fn write_from(&mut self, f: &File, count: usize, off: u64) -> io::Result<usize> {
        self.0.write_from_at(f, count, off)
    }
}

impl io::Write for ZCWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// How many opcode slots the per-device histogram keeps.
///
/// FUSE's opcode space is dense: the ones the kernel sends for this connection
/// top out at `REMOVE_MAPPING` (49), so 64 covers every one of them with room to
/// spare and one `u64` per slot is a few hundred bytes.
pub const OPCODE_SLOTS: usize = 64;

pub struct Server<F: FileSystem + Sync> {
    fs: F,
    options: AtomicU64,
    /// Whether `open`/`opendir` answer `-ENOSYS` (zero-message open).
    ///
    /// Held here as well as in the file system because `FUSE_ATOMIC_O_TRUNC` has
    /// to leave the `supported` set together with it: truncate-on-open rides the
    /// `FUSE_OPEN` request when that flag is negotiated (`fs/fuse/file.c:34-35`,
    /// `:257-295`), and a zero-message-open client sends no `FUSE_OPEN` - it
    /// would open the file, keep the old contents, and never send the `SETATTR`
    /// that would have truncated it, because the kernel believes the open did.
    zero_message_open: AtomicBool,
    /// Requests served per opcode.
    ///
    /// One relaxed increment per request, and the only way to learn the opcode
    /// mix of a workload: the pinned guest kernel has neither ftrace nor debugfs
    /// (`config-libkrunfw_x86_64-kvm-lto`), so the fuse tracepoints cannot be
    /// used from inside the guest.
    opcodes: [AtomicU64; OPCODE_SLOTS],
    /// Requests served, i.e. the sum of `opcodes`.
    served: AtomicU64,
    /// `FUSE_LOOKUP` outcomes, split the way the timeout question needs them.
    ///
    /// A positive lookup is the server being asked for a name the guest had
    /// cached and revalidated or lost; a negative one is the guest probing for a
    /// name that does not exist. The two have opposite fixes - one is churn the
    /// timeout cannot help, the other is exactly what `entry_timeout` governs -
    /// and the opcode histogram cannot tell them apart.
    lookup_positive: AtomicU64,
    lookup_negative: AtomicU64,
    lookup_error: AtomicU64,
    /// Whether the histogram is reported while the device runs as well as at
    /// teardown (off unless the caller asked: a launch that does not want to be
    /// measured should not carry the lines).
    stats: AtomicBool,
}

/// Request counts at which a running device reports its histogram.
///
/// The teardown report is the complete one, but it depends on the device being
/// torn down in an orderly way - a killed VM may never get there. These give a
/// progressively better answer in the log while the workload runs, at a cost of
/// one comparison per request. A cang rustc-build arm serves ~1-3 M requests, so
/// the interesting lines are the 1 M and 3 M ones.
const OPCODE_MILESTONES: [u64; 9] = [
    5_000,
    20_000,
    100_000,
    300_000,
    1_000_000,
    3_000_000,
    10_000_000,
    30_000_000,
    100_000_000,
];

impl<F: FileSystem + Sync> Server<F> {
    pub fn new(fs: F) -> Server<F> {
        Server {
            fs,
            options: AtomicU64::new(FsOptions::empty().bits()),
            zero_message_open: AtomicBool::new(false),
            opcodes: std::array::from_fn(|_| AtomicU64::new(0)),
            served: AtomicU64::new(0),
            lookup_positive: AtomicU64::new(0),
            lookup_negative: AtomicU64::new(0),
            lookup_error: AtomicU64::new(0),
            stats: AtomicBool::new(false),
        }
    }

    /// Whether to report the histogram at request milestones as well as when the
    /// device stops.
    pub fn set_opcode_stats(&self, enabled: bool) {
        self.stats.store(enabled, Ordering::Relaxed);
    }

    /// Serve `FUSE_OPEN`/`FUSE_OPENDIR` with `-ENOSYS`, and keep the negotiated
    /// flags consistent with that (see the field's comment).
    pub fn set_zero_message_open(&self, enabled: bool) {
        self.zero_message_open.store(enabled, Ordering::Relaxed);
    }

    fn count_opcode(&self, opcode: u32) {
        let slot = opcode as usize;
        if slot < OPCODE_SLOTS {
            self.opcodes[slot].fetch_add(1, Ordering::Relaxed);
        }
        let served = self.served.fetch_add(1, Ordering::Relaxed) + 1;
        if self.stats.load(Ordering::Relaxed) && OPCODE_MILESTONES.contains(&served) {
            self.log_opcode_histogram("milestone");
        }
    }

    /// Log the opcode histogram: one header line, then one line per opcode, most
    /// frequent first.
    pub fn log_opcode_histogram(&self, tag: &str) {
        let histogram = self.opcode_histogram();
        let total: u64 = histogram.iter().map(|(_, count)| count).sum();
        warn!("fs: opcode histogram [{tag}]: {total} requests served");
        for (opcode, count) in histogram {
            let share = if total == 0 {
                0.0
            } else {
                (count as f64) * 100.0 / (total as f64)
            };
            match opcode_name(opcode) {
                Some(name) => warn!("fs:   {name:<20} {count:>12} {share:>7.2}%"),
                None => warn!("fs:   opcode {opcode:<13} {count:>12} {share:>7.2}%"),
            }
        }

        let positive = self.lookup_positive.load(Ordering::Relaxed);
        let negative = self.lookup_negative.load(Ordering::Relaxed);
        let error = self.lookup_error.load(Ordering::Relaxed);
        let lookups = positive + negative + error;
        if lookups != 0 {
            let miss = (negative + error) as f64 * 100.0 / lookups as f64;
            warn!(
                "fs:   lookup {positive} positive, {negative} negative, {error} error \
                 ({miss:.2}% of lookups found nothing; {:.2}% of requests)",
                lookups as f64 * 100.0 / (total as f64).max(1.0)
            );
        }
    }

    /// The opcode histogram, most frequent first.
    pub fn opcode_histogram(&self) -> Vec<(u32, u64)> {
        let mut counts: Vec<(u32, u64)> = self
            .opcodes
            .iter()
            .enumerate()
            .map(|(opcode, count)| (opcode as u32, count.load(Ordering::Relaxed)))
            .filter(|(_, count)| *count != 0)
            .collect();
        counts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        counts
    }

    #[allow(clippy::cognitive_complexity)]
    pub fn handle_message(
        &self,
        mut r: Reader,
        w: Writer,
        allow_idmap: bool,
        shm_region: &Option<VirtioShmRegion>,
        exit_code: &Arc<AtomicI32>,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
    ) -> Result<usize> {
        let in_header: InHeader = r.read_obj().map_err(Error::DecodeMessage)?;

        if in_header.len > (MAX_BUFFER_SIZE + BUFFER_HEADER_SIZE) {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                in_header.unique,
                w,
            );
        }
        debug!("opcode: {}", in_header.opcode);
        self.count_opcode(in_header.opcode);
        match in_header.opcode {
            x if x == Opcode::Lookup as u32 => self.lookup(in_header, r, w),
            x if x == Opcode::Forget as u32 => self.forget(in_header, r), // No reply.
            x if x == Opcode::Getattr as u32 => self.getattr(in_header, r, w),
            x if x == Opcode::Setattr as u32 => self.setattr(in_header, r, w),
            x if x == Opcode::Readlink as u32 => self.readlink(in_header, w),
            x if x == Opcode::Symlink as u32 => self.symlink(in_header, r, w),
            x if x == Opcode::Mknod as u32 => self.mknod(in_header, r, w),
            x if x == Opcode::Mkdir as u32 => self.mkdir(in_header, r, w),
            x if x == Opcode::Unlink as u32 => self.unlink(in_header, r, w),
            x if x == Opcode::Rmdir as u32 => self.rmdir(in_header, r, w),
            x if x == Opcode::Rename as u32 => self.rename(in_header, r, w),
            x if x == Opcode::Link as u32 => self.link(in_header, r, w),
            x if x == Opcode::Open as u32 => self.open(in_header, r, w),
            x if x == Opcode::Read as u32 => self.read(in_header, r, w),
            x if x == Opcode::Write as u32 => self.write(in_header, r, w),
            x if x == Opcode::Statfs as u32 => self.statfs(in_header, w),
            x if x == Opcode::Release as u32 => self.release(in_header, r, w),
            x if x == Opcode::Fsync as u32 => self.fsync(in_header, r, w),
            x if x == Opcode::Setxattr as u32 => self.setxattr(in_header, r, w),
            x if x == Opcode::Getxattr as u32 => self.getxattr(in_header, r, w),
            x if x == Opcode::Listxattr as u32 => self.listxattr(in_header, r, w),
            x if x == Opcode::Removexattr as u32 => self.removexattr(in_header, r, w),
            x if x == Opcode::Flush as u32 => self.flush(in_header, r, w),
            x if x == Opcode::Init as u32 => self.init(in_header, r, w, allow_idmap),
            x if x == Opcode::Opendir as u32 => self.opendir(in_header, r, w),
            x if x == Opcode::Readdir as u32 => self.readdir(in_header, r, w),
            x if x == Opcode::Releasedir as u32 => self.releasedir(in_header, r, w),
            x if x == Opcode::Fsyncdir as u32 => self.fsyncdir(in_header, r, w),
            x if x == Opcode::Getlk as u32 => self.getlk(in_header, r, w),
            x if x == Opcode::Setlk as u32 => self.setlk(in_header, r, w),
            x if x == Opcode::Setlkw as u32 => self.setlkw(in_header, r, w),
            x if x == Opcode::Access as u32 => self.access(in_header, r, w),
            x if x == Opcode::Create as u32 => self.create(in_header, r, w),
            x if x == Opcode::Interrupt as u32 => self.interrupt(in_header),
            x if x == Opcode::Bmap as u32 => self.bmap(in_header, r, w),
            x if x == Opcode::Destroy as u32 => self.destroy(),
            x if x == Opcode::Ioctl as u32 => self.ioctl(in_header, r, w, exit_code),
            x if x == Opcode::Poll as u32 => self.poll(in_header, r, w),
            x if x == Opcode::NotifyReply as u32 => self.notify_reply(in_header, r, w),
            x if x == Opcode::BatchForget as u32 => self.batch_forget(in_header, r, w),
            x if x == Opcode::Fallocate as u32 => self.fallocate(in_header, r, w),
            x if x == Opcode::Readdirplus as u32 => self.readdirplus(in_header, r, w),
            x if x == Opcode::Rename2 as u32 => self.rename2(in_header, r, w),
            x if x == Opcode::Lseek as u32 => self.lseek(in_header, r, w),
            x if x == Opcode::CopyFileRange as u32 => self.copyfilerange(in_header, r, w),
            x if (x == Opcode::SetupMapping as u32) && shm_region.is_some() => {
                let shm = shm_region.as_ref().unwrap();
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                let shm_base_addr = shm.host_addr;
                #[cfg(target_os = "macos")]
                let shm_base_addr = shm.guest_addr;
                self.setupmapping(
                    in_header,
                    r,
                    w,
                    shm_base_addr,
                    shm.size as u64,
                    #[cfg(target_os = "macos")]
                    map_sender,
                )
            }
            x if (x == Opcode::RemoveMapping as u32) && shm_region.is_some() => {
                let shm = shm_region.as_ref().unwrap();
                #[cfg(any(target_os = "linux", target_os = "windows"))]
                let shm_base_addr = shm.host_addr;
                #[cfg(target_os = "macos")]
                let shm_base_addr = shm.guest_addr;
                self.removemapping(
                    in_header,
                    r,
                    w,
                    shm_base_addr,
                    shm.size as u64,
                    #[cfg(target_os = "macos")]
                    map_sender,
                )
            }
            _ => reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOSYS)),
                in_header.unique,
                w,
            ),
        }
    }

    fn lookup(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let namelen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .ok_or(Error::InvalidHeaderLength)?;

        let mut buf = vec![0u8; namelen];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;

        let name = bytes_to_cstr(buf.as_ref())?;

        match self
            .fs
            .lookup(Context::from(in_header), in_header.nodeid.into(), name)
        {
            Ok(entry) => {
                self.lookup_positive.fetch_add(1, Ordering::Relaxed);
                let out = EntryOut::from(entry);

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => {
                if e.raw_os_error() == Some(libc::ENOENT) {
                    self.lookup_negative.fetch_add(1, Ordering::Relaxed);
                } else {
                    self.lookup_error.fetch_add(1, Ordering::Relaxed);
                }
                reply_error(e, in_header.unique, w)
            }
        }
    }

    fn forget(&self, in_header: InHeader, mut r: Reader) -> Result<usize> {
        let ForgetIn { nlookup } = r.read_obj().map_err(Error::DecodeMessage)?;

        self.fs
            .forget(Context::from(in_header), in_header.nodeid.into(), nlookup);

        // There is no reply for forget messages.
        Ok(0)
    }

    fn getattr(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let GetattrIn { flags, fh, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        // `fh == 0` means "no handle", not handle number zero: a zero-message
        // open leaves `ff->fh` at 0 (`fs/fuse/file.c:154-156`) and the kernel
        // still sets `FUSE_GETATTR_FH` for a file it has open (`fs/fuse/dir.c:1430-1435`).
        let handle = if (flags & GETATTR_FH) != 0 && fh != 0 {
            Some(fh.into())
        } else {
            None
        };

        match self
            .fs
            .getattr(Context::from(in_header), in_header.nodeid.into(), handle)
        {
            Ok((st, timeout)) => {
                let out = AttrOut {
                    attr_valid: timeout.as_secs(),
                    attr_valid_nsec: timeout.subsec_nanos(),
                    dummy: 0,
                    attr: st.into(),
                };
                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn setattr(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let setattr_in: SetattrIn = r.read_obj().map_err(Error::DecodeMessage)?;

        // As in `getattr`: with zero-message opens `FATTR_FH` can carry 0
        // (`fs/fuse/dir.c:2210-2214`), and resolving that as a handle would fail
        // every attribute change the guest makes through an open file -
        // truncation included. No handle means the file system applies it to the
        // inode's `O_PATH` descriptor, which is exactly what a zero-message-open
        // request wants.
        let handle = if setattr_in.valid & FATTR_FH != 0 && setattr_in.fh != 0 {
            Some(setattr_in.fh.into())
        } else {
            None
        };

        let valid = SetattrValid::from_bits_truncate(setattr_in.valid);

        let st: bindings::stat64 = setattr_in.into();

        match self.fs.setattr(
            Context::from(in_header),
            in_header.nodeid.into(),
            st,
            handle,
            valid,
        ) {
            Ok((st, timeout)) => {
                let out = AttrOut {
                    attr_valid: timeout.as_secs(),
                    attr_valid_nsec: timeout.subsec_nanos(),
                    dummy: 0,
                    attr: st.into(),
                };
                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn readlink(&self, in_header: InHeader, w: Writer) -> Result<usize> {
        match self
            .fs
            .readlink(Context::from(in_header), in_header.nodeid.into())
        {
            Ok(linkname) => {
                // We need to disambiguate the option type here even though it is `None`.
                reply_ok(None::<u8>, Some(&linkname), in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn symlink(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        // Unfortunately the name and linkname are encoded one after another and
        // separated by a nul character.
        let len = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .ok_or(Error::InvalidHeaderLength)?;
        let mut buf = vec![0; len];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;

        let mut components = buf.split_inclusive(|c| *c == b'\0');

        let name = components.next().ok_or(Error::MissingParameter)?;
        let linkname = components.next().ok_or(Error::MissingParameter)?;

        let options = FsOptions::from_bits_truncate(self.options.load(Ordering::Relaxed));

        let extensions = get_extensions(options, name.len() + linkname.len(), buf.as_slice())?;

        match self.fs.symlink(
            Context::from(in_header),
            bytes_to_cstr(linkname)?,
            in_header.nodeid.into(),
            bytes_to_cstr(name)?,
            extensions,
        ) {
            Ok(entry) => {
                let out = EntryOut::from(entry);

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn mknod(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let MknodIn {
            mode, rdev, umask, ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        let remaining_len = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .and_then(|l| l.checked_sub(size_of::<MknodIn>()))
            .ok_or(Error::InvalidHeaderLength)?;
        let mut buf = vec![0; remaining_len];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;
        let mut components = buf.split_inclusive(|c| *c == b'\0');
        let name = components.next().ok_or(Error::MissingParameter)?;

        let options = FsOptions::from_bits_truncate(self.options.load(Ordering::Relaxed));

        let extensions = get_extensions(options, name.len(), buf.as_slice())?;

        match self.fs.mknod(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(name)?,
            mode,
            rdev,
            umask,
            extensions,
        ) {
            Ok(entry) => {
                let out = EntryOut::from(entry);

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn mkdir(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let MkdirIn { mode, umask } = r.read_obj().map_err(Error::DecodeMessage)?;

        let remaining_len = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .and_then(|l| l.checked_sub(size_of::<MkdirIn>()))
            .ok_or(Error::InvalidHeaderLength)?;
        let mut buf = vec![0; remaining_len];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;
        let mut components = buf.split_inclusive(|c| *c == b'\0');
        let name = components.next().ok_or(Error::MissingParameter)?;

        let options = FsOptions::from_bits_truncate(self.options.load(Ordering::Relaxed));

        let extensions = get_extensions(options, name.len(), buf.as_slice())?;

        match self.fs.mkdir(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(name)?,
            mode,
            umask,
            extensions,
        ) {
            Ok(entry) => {
                let out = EntryOut::from(entry);

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn unlink(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let namelen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .ok_or(Error::InvalidHeaderLength)?;
        let mut name = vec![0; namelen];

        r.read_exact(&mut name).map_err(Error::DecodeMessage)?;

        match self.fs.unlink(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(&name)?,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn rmdir(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let namelen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .ok_or(Error::InvalidHeaderLength)?;
        let mut name = vec![0; namelen];

        r.read_exact(&mut name).map_err(Error::DecodeMessage)?;

        match self.fs.rmdir(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(&name)?,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn do_rename(
        &self,
        in_header: InHeader,
        msg_size: usize,
        newdir: u64,
        flags: u32,
        mut r: Reader,
        w: Writer,
    ) -> Result<usize> {
        let buflen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .and_then(|l| l.checked_sub(msg_size))
            .ok_or(Error::InvalidHeaderLength)?;
        let mut buf = vec![0; buflen];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;

        // We want to include the '\0' byte in the first slice.
        let split_pos = buf
            .iter()
            .position(|c| *c == b'\0')
            .map(|p| p + 1)
            .ok_or(Error::MissingParameter)?;

        let (oldname, newname) = buf.split_at(split_pos);

        match self.fs.rename(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(oldname)?,
            newdir.into(),
            bytes_to_cstr(newname)?,
            flags,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn rename(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let RenameIn { newdir } = r.read_obj().map_err(Error::DecodeMessage)?;

        self.do_rename(in_header, size_of::<RenameIn>(), newdir, 0, r, w)
    }

    fn rename2(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let Rename2In { newdir, flags, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        #[cfg(target_os = "linux")]
        let flags = flags & (libc::RENAME_EXCHANGE | libc::RENAME_NOREPLACE);

        self.do_rename(in_header, size_of::<Rename2In>(), newdir, flags, r, w)
    }

    fn link(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let LinkIn { oldnodeid } = r.read_obj().map_err(Error::DecodeMessage)?;

        let namelen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .and_then(|l| l.checked_sub(size_of::<LinkIn>()))
            .ok_or(Error::InvalidHeaderLength)?;
        let mut name = vec![0; namelen];

        r.read_exact(&mut name).map_err(Error::DecodeMessage)?;

        match self.fs.link(
            Context::from(in_header),
            oldnodeid.into(),
            in_header.nodeid.into(),
            bytes_to_cstr(&name)?,
        ) {
            Ok(entry) => {
                let out = EntryOut::from(entry);

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn open(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let OpenIn {
            flags, open_flags, ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        let kill_priv = open_flags & OPEN_KILL_SUIDGID != 0;

        match self.fs.open(
            Context::from(in_header),
            in_header.nodeid.into(),
            kill_priv,
            flags,
        ) {
            Ok((handle, opts)) => {
                let out = OpenOut {
                    fh: handle.map(Into::into).unwrap_or(0),
                    open_flags: opts.bits(),
                    ..Default::default()
                };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn read(&self, in_header: InHeader, mut r: Reader, mut w: Writer) -> Result<usize> {
        let ReadIn {
            fh,
            offset,
            size,
            read_flags,
            lock_owner,
            flags,
            ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                in_header.unique,
                w,
            );
        }

        let owner = if read_flags & READ_LOCKOWNER != 0 {
            Some(lock_owner)
        } else {
            None
        };

        // Split the writer into 2 pieces: one for the `OutHeader` and the rest for the data.
        let data_writer = ZCWriter(w.split_at(size_of::<OutHeader>()).unwrap());

        match self.fs.read(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh.into(),
            data_writer,
            size,
            offset,
            owner,
            flags,
        ) {
            Ok(count) => {
                // Don't use `reply_ok` because we need to set a custom size length for the
                // header.
                let out = OutHeader {
                    len: (size_of::<OutHeader>() + count) as u32,
                    error: 0,
                    unique: in_header.unique,
                };

                w.write_all(out.as_slice()).map_err(Error::EncodeMessage)?;
                Ok(out.len as usize)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn write(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let WriteIn {
            fh,
            offset,
            size,
            write_flags,
            lock_owner,
            flags,
            ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                in_header.unique,
                w,
            );
        }

        let owner = if write_flags & WRITE_LOCKOWNER != 0 {
            Some(lock_owner)
        } else {
            None
        };

        let delayed_write = write_flags & WRITE_CACHE != 0;
        let kill_priv = write_flags & WRITE_KILL_PRIV != 0;

        let data_reader = ZCReader(r);

        match self.fs.write(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh.into(),
            data_reader,
            size,
            offset,
            owner,
            delayed_write,
            kill_priv,
            flags,
        ) {
            Ok(count) => {
                let out = WriteOut {
                    size: count as u32,
                    ..Default::default()
                };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn statfs(&self, in_header: InHeader, w: Writer) -> Result<usize> {
        match self
            .fs
            .statfs(Context::from(in_header), in_header.nodeid.into())
        {
            Ok(st) => reply_ok(Some(Kstatfs::from(st)), None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn release(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let ReleaseIn {
            fh,
            flags,
            release_flags,
            lock_owner,
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        let flush = release_flags & RELEASE_FLUSH != 0;
        let flock_release = release_flags & RELEASE_FLOCK_UNLOCK != 0;
        let lock_owner = if flush || flock_release {
            Some(lock_owner)
        } else {
            None
        };

        match self.fs.release(
            Context::from(in_header),
            in_header.nodeid.into(),
            flags,
            fh.into(),
            flush,
            flock_release,
            lock_owner,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn fsync(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let FsyncIn {
            fh, fsync_flags, ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;
        let datasync = fsync_flags & 0x1 != 0;

        match self.fs.fsync(
            Context::from(in_header),
            in_header.nodeid.into(),
            datasync,
            fh.into(),
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn setxattr(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let SetxattrIn { size, flags } = r.read_obj().map_err(Error::DecodeMessage)?;

        // The name and value and encoded one after another and separated by a '\0' character.
        let len = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .and_then(|l| l.checked_sub(size_of::<SetxattrIn>()))
            .ok_or(Error::InvalidHeaderLength)?;
        let mut buf = vec![0; len];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;

        // We want to include the '\0' byte in the first slice.
        let split_pos = buf
            .iter()
            .position(|c| *c == b'\0')
            .map(|p| p + 1)
            .ok_or(Error::MissingParameter)?;

        let (name, value) = buf.split_at(split_pos);

        if size != value.len() as u32 {
            return Err(Error::InvalidXattrSize((size, value.len())));
        }

        match self.fs.setxattr(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(name)?,
            value,
            flags,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn getxattr(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let GetxattrIn { size, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        let namelen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .and_then(|l| l.checked_sub(size_of::<GetxattrIn>()))
            .ok_or(Error::InvalidHeaderLength)?;
        let mut name = vec![0; namelen];

        r.read_exact(&mut name).map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                in_header.unique,
                w,
            );
        }

        match self.fs.getxattr(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(&name)?,
            size,
        ) {
            Ok(GetxattrReply::Value(val)) => reply_ok(None::<u8>, Some(&val), in_header.unique, w),
            Ok(GetxattrReply::Count(count)) => {
                let out = GetxattrOut {
                    size: count,
                    ..Default::default()
                };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn listxattr(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let GetxattrIn { size, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                in_header.unique,
                w,
            );
        }

        match self
            .fs
            .listxattr(Context::from(in_header), in_header.nodeid.into(), size)
        {
            Ok(ListxattrReply::Names(val)) => reply_ok(None::<u8>, Some(&val), in_header.unique, w),
            Ok(ListxattrReply::Count(count)) => {
                let out = GetxattrOut {
                    size: count,
                    ..Default::default()
                };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn removexattr(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let namelen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .ok_or(Error::InvalidHeaderLength)?;

        let mut buf = vec![0; namelen];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;

        let name = bytes_to_cstr(&buf)?;

        match self
            .fs
            .removexattr(Context::from(in_header), in_header.nodeid.into(), name)
        {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn flush(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let FlushIn { fh, lock_owner, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.flush(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh.into(),
            lock_owner,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn init(
        &self,
        in_header: InHeader,
        mut r: Reader,
        w: Writer,
        allow_idmap: bool,
    ) -> Result<usize> {
        let InitInCompat {
            major,
            minor,
            max_readahead,
            flags,
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        let options = FsOptions::from_bits_truncate(flags as u64);

        let InitInExt { flags2, .. } = if options.contains(FsOptions::INIT_EXT) {
            r.read_obj().map_err(Error::DecodeMessage)?
        } else {
            InitInExt::default()
        };

        if major < KERNEL_VERSION {
            error!("Unsupported fuse protocol version: {major}.{minor}");
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::EPROTO)),
                in_header.unique,
                w,
            );
        }

        if major > KERNEL_VERSION {
            // Wait for the kernel to reply back with a 7.X version.
            let out = InitOut {
                major: KERNEL_VERSION,
                minor: KERNEL_MINOR_VERSION,
                ..Default::default()
            };

            return reply_ok(Some(out), None, in_header.unique, w);
        }

        if minor < KERNEL_MINOR_VERSION {
            error!("Unsupported fuse protocol minor version: {major}.{minor}");
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::EPROTO)),
                in_header.unique,
                w,
            );
        }

        // These fuse features are supported by this server by default.
        let mut supported = FsOptions::ASYNC_READ
            | FsOptions::PARALLEL_DIROPS
            | FsOptions::BIG_WRITES
            | FsOptions::AUTO_INVAL_DATA
            | FsOptions::ASYNC_DIO
            | FsOptions::HAS_IOCTL_DIR
            | FsOptions::MAX_PAGES
            | FsOptions::ZERO_MESSAGE_OPEN
            | FsOptions::ZERO_MESSAGE_OPENDIR
            | FsOptions::SUBMOUNTS
            | FsOptions::HANDLE_KILLPRIV_V2
            | FsOptions::INIT_EXT;

        if allow_idmap {
            supported |= FsOptions::ALLOW_IDMAP;
        }

        // Truncate-on-open rides the `FUSE_OPEN` request when this is negotiated:
        // the kernel masks `O_TRUNC` into the open arguments, keeps its own size
        // and page cache in step, and sends no `SETATTR` for it
        // (`fs/fuse/file.c:34-35`, `:257-295`). A zero-message-open client sends
        // no `FUSE_OPEN` at all, so negotiating the flag would make `O_TRUNC`
        // open the file and leave its contents in place. Without it the VFS
        // truncates through `SETATTR` size 0 after the open, which arrives with
        // `fh == 0` and is served from the inode's descriptor: one round trip
        // instead of the `OPEN` + `SETATTR` pair, and correct.
        if !self.zero_message_open.load(Ordering::Relaxed) {
            supported |= FsOptions::ATOMIC_O_TRUNC;
        }

        if cfg!(target_os = "macos") {
            supported |= FsOptions::SECURITY_CTX;
        }

        let flags_64 = ((flags2 as u64) << 32) | (flags as u64);
        let capable = FsOptions::from_bits_truncate(flags_64);

        let page_size: u32 = {
            #[cfg(unix)]
            unsafe {
                libc::sysconf(libc::_SC_PAGESIZE).try_into().unwrap()
            }
            #[cfg(windows)]
            unsafe {
                let mut info: SYSTEM_INFO = std::mem::zeroed();
                GetSystemInfo(&mut info);
                info.dwPageSize
            }
        };
        let max_pages = ((MAX_BUFFER_SIZE - 1) / page_size) + 1;

        match self.fs.init(capable) {
            Ok(want) => {
                let enabled = (capable & (want | supported)).bits();
                self.options.store(enabled, Ordering::Relaxed);

                let out = InitOut {
                    major: KERNEL_VERSION,
                    minor: KERNEL_MINOR_VERSION,
                    max_readahead,
                    flags: enabled as u32,
                    max_background: u16::MAX,
                    congestion_threshold: (u16::MAX / 4) * 3,
                    max_write: MAX_BUFFER_SIZE,
                    time_gran: 1, // nanoseconds
                    max_pages: max_pages.try_into().unwrap(),
                    map_alignment: 0,
                    flags2: (enabled >> 32) as u32,
                    ..Default::default()
                };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn opendir(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let OpenIn { flags, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self
            .fs
            .opendir(Context::from(in_header), in_header.nodeid.into(), flags)
        {
            Ok((handle, opts)) => {
                let out = OpenOut {
                    fh: handle.map(Into::into).unwrap_or(0),
                    open_flags: opts.bits(),
                    ..Default::default()
                };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn do_readdir(
        &self,
        in_header: InHeader,
        mut r: Reader,
        mut w: Writer,
        plus: bool,
    ) -> Result<usize> {
        let ReadIn {
            fh, offset, size, ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        if size > MAX_BUFFER_SIZE {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                in_header.unique,
                w,
            );
        }

        let available_bytes = w.available_bytes();
        if available_bytes < size as usize {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                in_header.unique,
                w,
            );
        }

        // Skip over enough bytes for the header.
        let mut cursor = w.split_at(size_of::<OutHeader>()).unwrap();

        let res = if plus {
            self.fs.readdirplus(
                Context::from(in_header),
                in_header.nodeid.into(),
                fh.into(),
                size,
                offset,
                |d, e| add_dirent(&mut cursor, size, d, Some(e)),
            )
        } else {
            self.fs.readdir(
                Context::from(in_header),
                in_header.nodeid.into(),
                fh.into(),
                size,
                offset,
                |d| add_dirent(&mut cursor, size, d, None),
            )
        };

        if let Err(e) = res {
            reply_error(e, in_header.unique, w)
        } else {
            // Don't use `reply_ok` because we need to set a custom size length for the
            // header.
            let out = OutHeader {
                len: (size_of::<OutHeader>() + cursor.bytes_written()) as u32,
                error: 0,
                unique: in_header.unique,
            };

            w.write_all(out.as_slice()).map_err(Error::EncodeMessage)?;
            Ok(out.len as usize)
        }
    }

    fn readdir(&self, in_header: InHeader, r: Reader, w: Writer) -> Result<usize> {
        self.do_readdir(in_header, r, w, false)
    }

    fn readdirplus(&self, in_header: InHeader, r: Reader, w: Writer) -> Result<usize> {
        self.do_readdir(in_header, r, w, true)
    }

    fn releasedir(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let ReleaseIn { fh, flags, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.releasedir(
            Context::from(in_header),
            in_header.nodeid.into(),
            flags,
            fh.into(),
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn fsyncdir(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let FsyncIn {
            fh, fsync_flags, ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;
        let datasync = fsync_flags & 0x1 != 0;

        match self.fs.fsyncdir(
            Context::from(in_header),
            in_header.nodeid.into(),
            datasync,
            fh.into(),
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn getlk(&self, in_header: InHeader, mut _r: Reader, w: Writer) -> Result<usize> {
        if let Err(e) = self.fs.getlk() {
            reply_error(e, in_header.unique, w)
        } else {
            Ok(0)
        }
    }

    fn setlk(&self, in_header: InHeader, mut _r: Reader, w: Writer) -> Result<usize> {
        if let Err(e) = self.fs.setlk() {
            reply_error(e, in_header.unique, w)
        } else {
            Ok(0)
        }
    }

    fn setlkw(&self, in_header: InHeader, mut _r: Reader, w: Writer) -> Result<usize> {
        if let Err(e) = self.fs.setlkw() {
            reply_error(e, in_header.unique, w)
        } else {
            Ok(0)
        }
    }

    fn access(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let AccessIn { mask, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self
            .fs
            .access(Context::from(in_header), in_header.nodeid.into(), mask)
        {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn create(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let CreateIn {
            flags,
            mode,
            umask,
            open_flags,
            ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        let namelen = (in_header.len as usize)
            .checked_sub(size_of::<InHeader>())
            .and_then(|l| l.checked_sub(size_of::<CreateIn>()))
            .ok_or(Error::InvalidHeaderLength)?;

        let mut buf = vec![0; namelen];

        r.read_exact(&mut buf).map_err(Error::DecodeMessage)?;
        let mut components = buf.split_inclusive(|c| *c == b'\0');
        let name = components.next().ok_or(Error::MissingParameter)?;

        let options = FsOptions::from_bits_truncate(self.options.load(Ordering::Relaxed));

        let extensions = get_extensions(options, name.len(), buf.as_slice())?;

        let kill_priv = open_flags & OPEN_KILL_SUIDGID != 0;

        match self.fs.create(
            Context::from(in_header),
            in_header.nodeid.into(),
            bytes_to_cstr(name)?,
            mode,
            kill_priv,
            flags,
            umask,
            extensions,
        ) {
            Ok((entry, handle, opts)) => {
                let entry_out = EntryOut {
                    nodeid: entry.inode,
                    generation: entry.generation,
                    entry_valid: entry.entry_timeout.as_secs(),
                    attr_valid: entry.attr_timeout.as_secs(),
                    entry_valid_nsec: entry.entry_timeout.subsec_nanos(),
                    attr_valid_nsec: entry.attr_timeout.subsec_nanos(),
                    attr: entry.attr.into(),
                };
                let open_out = OpenOut {
                    fh: handle.map(Into::into).unwrap_or(0),
                    open_flags: opts.bits(),
                    ..Default::default()
                };

                // Kind of a hack to write both structs.
                reply_ok(
                    Some(entry_out),
                    Some(open_out.as_slice()),
                    in_header.unique,
                    w,
                )
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn interrupt(&self, _in_header: InHeader) -> Result<usize> {
        Ok(0)
    }

    fn bmap(&self, in_header: InHeader, mut _r: Reader, w: Writer) -> Result<usize> {
        if let Err(e) = self.fs.bmap() {
            reply_error(e, in_header.unique, w)
        } else {
            Ok(0)
        }
    }

    fn destroy(&self) -> Result<usize> {
        // No reply to this function.
        self.fs.destroy();

        Ok(0)
    }

    fn ioctl(
        &self,
        in_header: InHeader,
        mut r: Reader,
        w: Writer,
        exit_code: &Arc<AtomicI32>,
    ) -> Result<usize> {
        let IoctlIn {
            fh,
            flags,
            cmd,
            arg,
            in_size,
            out_size,
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.ioctl(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh.into(),
            flags,
            cmd,
            arg,
            in_size,
            out_size,
            exit_code,
        ) {
            Ok(data) => {
                let out = IoctlOut {
                    result: 0,
                    ..Default::default()
                };
                reply_ok(Some(out), Some(&data), in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn poll(&self, in_header: InHeader, mut _r: Reader, w: Writer) -> Result<usize> {
        if let Err(e) = self.fs.poll() {
            reply_error(e, in_header.unique, w)
        } else {
            Ok(0)
        }
    }

    fn notify_reply(&self, in_header: InHeader, mut _r: Reader, w: Writer) -> Result<usize> {
        if let Err(e) = self.fs.notify_reply() {
            reply_error(e, in_header.unique, w)
        } else {
            Ok(0)
        }
    }

    fn batch_forget(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let BatchForgetIn { count, .. } = r.read_obj().map_err(Error::DecodeMessage)?;

        if let Some(size) = (count as usize).checked_mul(size_of::<ForgetOne>()) {
            if size > MAX_BUFFER_SIZE as usize {
                return reply_error(
                    linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                    in_header.unique,
                    w,
                );
            }
        } else {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::EOVERFLOW)),
                in_header.unique,
                w,
            );
        }

        let mut requests = Vec::with_capacity(count as usize);
        for _ in 0..count {
            requests.push(
                r.read_obj::<ForgetOne>()
                    .map(|f| (f.nodeid.into(), f.nlookup))
                    .map_err(Error::DecodeMessage)?,
            );
        }

        self.fs.batch_forget(Context::from(in_header), requests);

        // No reply for forget messages.
        Ok(0)
    }

    fn fallocate(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let FallocateIn {
            fh,
            offset,
            length,
            mode,
            ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.fallocate(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh.into(),
            mode,
            offset,
            length,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn lseek(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let LseekIn {
            fh, offset, whence, ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.lseek(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh.into(),
            offset,
            whence,
        ) {
            Ok(offset) => {
                let out = LseekOut { offset };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn copyfilerange(&self, in_header: InHeader, mut r: Reader, w: Writer) -> Result<usize> {
        let CopyfilerangeIn {
            fh_in,
            off_in,
            nodeid_out,
            fh_out,
            off_out,
            len,
            flags,
            ..
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.copyfilerange(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh_in.into(),
            off_in,
            nodeid_out.into(),
            fh_out.into(),
            off_out,
            len,
            flags,
        ) {
            Ok(count) => {
                let out = WriteOut {
                    size: count as u32,
                    ..Default::default()
                };

                reply_ok(Some(out), None, in_header.unique, w)
            }
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn setupmapping(
        &self,
        in_header: InHeader,
        mut r: Reader,
        w: Writer,
        host_shm_base: u64,
        shm_size: u64,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
    ) -> Result<usize> {
        let SetupmappingIn {
            fh,
            foffset,
            len,
            flags,
            moffset,
        } = r.read_obj().map_err(Error::DecodeMessage)?;

        match self.fs.setupmapping(
            Context::from(in_header),
            in_header.nodeid.into(),
            fh.into(),
            foffset,
            len,
            flags,
            moffset,
            host_shm_base,
            shm_size,
            #[cfg(target_os = "macos")]
            map_sender,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }

    fn removemapping(
        &self,
        in_header: InHeader,
        mut r: Reader,
        w: Writer,
        host_shm_base: u64,
        shm_size: u64,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
    ) -> Result<usize> {
        let RemovemappingIn { count } = r.read_obj().map_err(Error::DecodeMessage)?;

        if let Some(size) = (count as usize).checked_mul(size_of::<RemovemappingOne>()) {
            if size > MAX_BUFFER_SIZE as usize {
                return reply_error(
                    linux_error(io::Error::from_raw_os_error(libc::ENOMEM)),
                    in_header.unique,
                    w,
                );
            }
        } else {
            return reply_error(
                linux_error(io::Error::from_raw_os_error(libc::EOVERFLOW)),
                in_header.unique,
                w,
            );
        }

        let mut requests = Vec::with_capacity(count as usize);
        for _ in 0..count {
            requests.push(
                r.read_obj::<RemovemappingOne>()
                    .map_err(Error::DecodeMessage)?,
            );
        }

        match self.fs.removemapping(
            Context::from(in_header),
            requests,
            host_shm_base,
            shm_size,
            #[cfg(target_os = "macos")]
            map_sender,
        ) {
            Ok(()) => reply_ok(None::<u8>, None, in_header.unique, w),
            Err(e) => reply_error(e, in_header.unique, w),
        }
    }
}

fn reply_ok<T: ByteValued>(
    out: Option<T>,
    data: Option<&[u8]>,
    unique: u64,
    mut w: Writer,
) -> Result<usize> {
    let mut len = size_of::<OutHeader>();

    if out.is_some() {
        len += size_of::<T>();
    }

    if let Some(data) = data {
        len += data.len();
    }

    let header = OutHeader {
        len: len as u32,
        error: 0,
        unique,
    };

    w.write_all(header.as_slice())
        .map_err(Error::EncodeMessage)?;

    if let Some(out) = out {
        w.write_all(out.as_slice()).map_err(Error::EncodeMessage)?;
    }

    if let Some(data) = data {
        w.write_all(data).map_err(Error::EncodeMessage)?;
    }

    debug_assert_eq!(len, w.bytes_written());
    Ok(w.bytes_written())
}

fn reply_error(e: io::Error, unique: u64, mut w: Writer) -> Result<usize> {
    let header = OutHeader {
        len: size_of::<OutHeader>() as u32,
        error: -e.raw_os_error().unwrap_or(libc::EIO),
        unique,
    };

    w.write_all(header.as_slice())
        .map_err(Error::EncodeMessage)?;

    debug_assert_eq!(header.len as usize, w.bytes_written());
    Ok(w.bytes_written())
}

fn bytes_to_cstr(buf: &[u8]) -> Result<&CStr> {
    // Convert to a `CStr` first so that we can drop the '\0' byte at the end
    // and make sure there are no interior '\0' bytes.
    CStr::from_bytes_with_nul(buf).map_err(Error::InvalidCString)
}

fn add_dirent(
    cursor: &mut Writer,
    max: u32,
    d: DirEntry,
    entry: Option<Entry>,
) -> io::Result<usize> {
    if d.name.len() > u32::MAX as usize {
        return Err(linux_error(io::Error::from_raw_os_error(libc::EOVERFLOW)));
    }

    let dirent_len = size_of::<Dirent>()
        .checked_add(d.name.len())
        .ok_or_else(|| linux_error(io::Error::from_raw_os_error(libc::EOVERFLOW)))?;

    // Directory entries must be padded to 8-byte alignment.  If adding 7 causes
    // an overflow then this dirent cannot be properly padded.
    let padded_dirent_len = dirent_len
        .checked_add(7)
        .map(|l| l & !7)
        .ok_or_else(|| linux_error(io::Error::from_raw_os_error(libc::EOVERFLOW)))?;

    let total_len = if entry.is_some() {
        padded_dirent_len
            .checked_add(size_of::<EntryOut>())
            .ok_or_else(|| linux_error(io::Error::from_raw_os_error(libc::EOVERFLOW)))?
    } else {
        padded_dirent_len
    };

    if (max as usize).saturating_sub(cursor.bytes_written()) < total_len {
        Ok(0)
    } else {
        if let Some(entry) = entry {
            cursor.write_all(EntryOut::from(entry).as_slice())?;
        }

        let dirent = Dirent {
            ino: d.ino,
            off: d.offset,
            namelen: d.name.len() as u32,
            type_: d.type_,
        };

        cursor.write_all(dirent.as_slice())?;
        cursor.write_all(d.name)?;

        // We know that `dirent_len` <= `padded_dirent_len` due to the check above
        // so there's no need for checked arithmetic.
        let padding = padded_dirent_len - dirent_len;
        if padding > 0 {
            cursor.write_all(&DIRENT_PADDING[..padding])?;
        }

        Ok(total_len)
    }
}

fn take_object<T: ByteValued>(data: &[u8]) -> Result<(T, &[u8])> {
    if data.len() < size_of::<T>() {
        return Err(Error::DecodeMessage(einval()));
    }

    let (object_bytes, remaining_bytes) = data.split_at(size_of::<T>());
    // SAFETY: `T` implements `ByteValued` that guarantees that it is safe to instantiate
    // `T` with random data.
    let object: T = unsafe { std::ptr::read_unaligned(object_bytes.as_ptr() as *const T) };
    Ok((object, remaining_bytes))
}

fn parse_security_context(nr_secctx: u32, data: &[u8]) -> Result<Option<SecContext>> {
    // Although the FUSE security context extension allows sending several security contexts,
    // currently the guest kernel only sends one.
    if nr_secctx > 1 {
        return Err(Error::DecodeMessage(einval()));
    } else if nr_secctx == 0 {
        // No security context sent. May be no LSM supports it.
        return Ok(None);
    }

    let (secctx, data) = take_object::<Secctx>(data)?;

    if secctx.size == 0 {
        return Err(Error::DecodeMessage(einval()));
    }

    let mut components = data.split_inclusive(|c| *c == b'\0');
    let secctx_name = components.next().ok_or(Error::MissingParameter)?;
    let (_, data) = data.split_at(secctx_name.len());

    if data.len() < secctx.size as usize {
        return Err(Error::DecodeMessage(einval()));
    }

    // Fuse client aligns the whole security context block to 64 byte
    // boundary. So it is possible that after actual security context
    // of secctx.size, there are some null padding bytes left. If
    // we ever parse more data after secctx, we will have to take those
    // null bytes into account. Total size (including null bytes) is
    // available in SecctxHeader->size.
    let (remaining, _) = data.split_at(secctx.size as usize);

    let fuse_secctx = SecContext {
        name: CString::from_vec_with_nul(secctx_name.to_vec()).map_err(Error::InvalidCString2)?,
        secctx: remaining.to_vec(),
    };

    Ok(Some(fuse_secctx))
}

fn get_extensions(options: FsOptions, skip: usize, request_bytes: &[u8]) -> Result<Extensions> {
    let mut extensions = Extensions::default();

    if !(options.contains(FsOptions::SECURITY_CTX)
        || options.contains(FsOptions::CREATE_SUPP_GROUP))
    {
        return Ok(extensions);
    }

    // It's not guaranty to receive an extension even if it's supported by the guest kernel
    if request_bytes.len() < skip {
        return Err(Error::DecodeMessage(einval()));
    }

    // We need to track if a SecCtx was received, because it's valid
    // for the guest to send an empty SecCtx (i.e, nr_secctx == 0)
    let mut secctx_received = false;

    let mut buf = &request_bytes[skip..];
    while !buf.is_empty() {
        let (extension_header, remaining_bytes) = take_object::<ExtHeader>(buf)?;

        let extension_size = (extension_header.size as usize)
            .checked_sub(size_of::<ExtHeader>())
            .ok_or(Error::InvalidHeaderLength)?;

        let (current_extension_bytes, next_extension_bytes) =
            remaining_bytes.split_at(extension_size);

        let ext_type = ExtType::try_from(extension_header.ext_type)
            .map_err(|_| Error::DecodeMessage(einval()))?;

        match ext_type {
            ExtType::SecCtx(nr_secctx) => {
                if !options.contains(FsOptions::SECURITY_CTX) || secctx_received {
                    return Err(Error::DecodeMessage(einval()));
                }

                secctx_received = true;
                extensions.secctx = parse_security_context(nr_secctx, current_extension_bytes)?;
            }
            ExtType::SupGroups => {
                // We're not exposing this feature to the guest, so we shouldn't get
                // any messages including this extension.
                unimplemented!("Support for supplemental groups is not implemented");
            }
        }

        // Let's process the next extension
        buf = next_extension_bytes;
    }

    // The SupGroup extension can be missing, since it is only sent if needed.
    // A SecCtx is always sent in create/synlink/mknod/mkdir if supported.
    if options.contains(FsOptions::SECURITY_CTX) && !secctx_received {
        return Err(Error::MissingExtension);
    }

    Ok(extensions)
}

/// The name of a FUSE opcode, for the device's histogram and diagnostics.
///
/// Only the opcodes this server handles by name are listed; anything else is
/// reported numerically by the caller, so a miscount cannot hide behind a wrong
/// label.
pub fn opcode_name(opcode: u32) -> Option<&'static str> {
    let name = match opcode {
        x if x == Opcode::Lookup as u32 => "LOOKUP",
        x if x == Opcode::Forget as u32 => "FORGET",
        x if x == Opcode::Getattr as u32 => "GETATTR",
        x if x == Opcode::Setattr as u32 => "SETATTR",
        x if x == Opcode::Readlink as u32 => "READLINK",
        x if x == Opcode::Symlink as u32 => "SYMLINK",
        x if x == Opcode::Mknod as u32 => "MKNOD",
        x if x == Opcode::Mkdir as u32 => "MKDIR",
        x if x == Opcode::Unlink as u32 => "UNLINK",
        x if x == Opcode::Rmdir as u32 => "RMDIR",
        x if x == Opcode::Rename as u32 => "RENAME",
        x if x == Opcode::Link as u32 => "LINK",
        x if x == Opcode::Open as u32 => "OPEN",
        x if x == Opcode::Read as u32 => "READ",
        x if x == Opcode::Write as u32 => "WRITE",
        x if x == Opcode::Statfs as u32 => "STATFS",
        x if x == Opcode::Release as u32 => "RELEASE",
        x if x == Opcode::Fsync as u32 => "FSYNC",
        x if x == Opcode::Setxattr as u32 => "SETXATTR",
        x if x == Opcode::Getxattr as u32 => "GETXATTR",
        x if x == Opcode::Listxattr as u32 => "LISTXATTR",
        x if x == Opcode::Removexattr as u32 => "REMOVEXATTR",
        x if x == Opcode::Flush as u32 => "FLUSH",
        x if x == Opcode::Init as u32 => "INIT",
        x if x == Opcode::Opendir as u32 => "OPENDIR",
        x if x == Opcode::Readdir as u32 => "READDIR",
        x if x == Opcode::Releasedir as u32 => "RELEASEDIR",
        x if x == Opcode::Fsyncdir as u32 => "FSYNCDIR",
        x if x == Opcode::Getlk as u32 => "GETLK",
        x if x == Opcode::Setlk as u32 => "SETLK",
        x if x == Opcode::Setlkw as u32 => "SETLKW",
        x if x == Opcode::Access as u32 => "ACCESS",
        x if x == Opcode::Create as u32 => "CREATE",
        x if x == Opcode::Interrupt as u32 => "INTERRUPT",
        x if x == Opcode::Bmap as u32 => "BMAP",
        x if x == Opcode::Destroy as u32 => "DESTROY",
        x if x == Opcode::Ioctl as u32 => "IOCTL",
        x if x == Opcode::Poll as u32 => "POLL",
        x if x == Opcode::NotifyReply as u32 => "NOTIFY_REPLY",
        x if x == Opcode::BatchForget as u32 => "BATCH_FORGET",
        x if x == Opcode::Fallocate as u32 => "FALLOCATE",
        x if x == Opcode::Readdirplus as u32 => "READDIRPLUS",
        x if x == Opcode::Rename2 as u32 => "RENAME2",
        x if x == Opcode::Lseek as u32 => "LSEEK",
        x if x == Opcode::CopyFileRange as u32 => "COPY_FILE_RANGE",
        x if x == Opcode::SetupMapping as u32 => "SETUPMAPPING",
        x if x == Opcode::RemoveMapping as u32 => "REMOVEMAPPING",
        _ => return None,
    };
    Some(name)
}
