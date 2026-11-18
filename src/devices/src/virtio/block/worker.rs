use crate::virtio::descriptor_utils::{Reader, Writer};

use super::super::DeviceQueue;
use super::device::{CacheType, DiskProperties};
use crate::virtio::DescriptorChain;

use crate::virtio::InterruptTransport;
use crossbeam_channel::{Receiver, Sender, unbounded};
use std::collections::HashMap;
use std::io::{self, Write};
use std::num::NonZeroUsize;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::result;
use std::thread::{self, JoinHandle};
use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use utils::eventfd::{EFD_NONBLOCK, EventFd};
#[cfg(target_os = "windows")]
use utils::windows::AsRawFd;
use virtio_bindings::virtio_blk::*;
use vm_memory::{ByteValued, GuestAddress, GuestMemoryMmap};

#[allow(dead_code)]
#[derive(Debug)]
pub enum RequestError {
    Discarding(io::Error),
    DiscardingToZero(io::Error),
    FlushingToDisk(io::Error),
    InvalidDataLength,
    ReadingFromDescriptor(io::Error),
    WritingToDescriptor(io::Error),
    WritingZeroes(io::Error),
    UnknownRequest,
}

/// The request header represents the mandatory fields of each block device request.
///
/// A request header contains the following fields:
///   * request_type: an u32 value mapping to a read, write or flush operation.
///   * reserved: 32 bits are reserved for future extensions of the Virtio Spec.
///   * sector: an u64 value representing the offset where a read/write is to occur.
///
/// The header simplifies reading the request from memory as all request follow
/// the same memory layout.
#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct RequestHeader {
    request_type: u32,
    _reserved: u32,
    sector: u64,
}
// Safe because RequestHeader only contains plain data.
unsafe impl ByteValued for RequestHeader {}

#[derive(Copy, Clone, Default)]
#[repr(C)]
pub struct DiscardWriteData {
    sector: u64,
    num_sectors: u32,
    flags: u32,
}
// Safe because DiscardWriteData only contains plain data.
unsafe impl ByteValued for DiscardWriteData {}

#[derive(Debug)]
struct ReadJob {
    head_index: u16,
    /// Distinguishes in-flight reads for the overlap bookkeeping below.
    id: u64,
    sector: u64,
    data_len: usize,
    buffers: Vec<(GuestAddress, u32)>,
}

struct ReadCompletion {
    job: ReadJob,
    ok: bool,
}

/// A write served by the pool: the data is written to the image straight
/// from the guest's buffers, which a write only reads, so it needs no copy.
#[derive(Debug)]
struct WriteJob {
    head_index: u16,
    sector: u64,
    data_len: usize,
    // The request's readable buffers, header included, and its writable
    // ones: the status byte.
    readable: Vec<(GuestAddress, u32)>,
    writable: Vec<(GuestAddress, u32)>,
}

struct WriteCompletion {
    job: WriteJob,
    ok: bool,
}

enum PoolJob {
    Read(ReadJob),
    Write(WriteJob),
}

enum PoolCompletion {
    Read(ReadCompletion),
    Write(WriteCompletion),
}

/// pread the image at `offset` straight into a writer that targets guest memory.
///
/// This is the same call the serial path makes on the device thread
/// (`Writer::write_from_at`); the only difference here is which thread runs it, which is
/// why the dispatcher admits a pooled read only when its guest ranges are disjoint from
/// every other request already in flight.
fn read_into_guest(
    writer: &mut Writer<'_>,
    disk: &DiskProperties,
    count: usize,
    offset: u64,
) -> io::Result<usize> {
    writer.write_from_at(disk, count, offset)
}

/// Read guest memory ranges for one request, as `(start, len)`.
fn guest_ranges(buffers: &[(GuestAddress, u32)]) -> Vec<(u64, u64)> {
    buffers
        .iter()
        .map(|(addr, len)| (addr.0, *len as u64))
        .collect()
}

/// Whether two requests write any of the same guest memory.
///
/// A pooled read writes the data straight into the guest's descriptor buffers, which is
/// only safe while no other in-flight read (or serial request) writes the same bytes.
/// The virtio contract says a driver must not post overlapping buffers for requests that
/// are in flight together, but a guest can still do it, so the dispatcher admits a read
/// only when its ranges are disjoint from everything already in flight and defers it
/// otherwise.
fn ranges_overlap(a: &[(u64, u64)], b: &[(u64, u64)]) -> bool {
    a.iter().any(|&(a_start, a_len)| {
        let a_end = a_start.saturating_add(a_len);
        b.iter().any(|&(b_start, b_len)| {
            let b_end = b_start.saturating_add(b_len);
            a_start < b_end && b_start < a_end
        })
    })
}

struct IoPool {
    jobs: Option<Sender<PoolJob>>,
    completions: Receiver<PoolCompletion>,
    event: EventFd,
    workers: Vec<JoinHandle<()>>,
    pending: usize,
}

impl IoPool {
    fn new(size: usize, disk: &DiskProperties, mem: &GuestMemoryMmap) -> Self {
        let (jobs, rx) = unbounded::<PoolJob>();
        // Workers must be able to finish while the dispatcher joins them on drop.
        let (tx, completions) = unbounded();
        let event = EventFd::new(EFD_NONBLOCK).unwrap();
        let disk_id = disk.image_id().to_vec();
        let cache_type = disk.cache_type();
        let workers = (0..size)
            .map(|i| {
                let rx = rx.clone();
                let tx = tx.clone();
                let event = event.try_clone().unwrap();
                let disk = disk.file.clone();
                let properties =
                    DiskProperties::new(disk.clone(), disk_id.clone(), cache_type).unwrap();
                let mem = mem.clone();
                thread::Builder::new()
                    .name(format!("block pool worker {i}"))
                    .spawn(move || {
                        while let Ok(job) = rx.recv() {
                            let job = match job {
                                PoolJob::Read(job) => job,
                                PoolJob::Write(job) => {
                                    let ok = write_from_guest(&mem, &properties, &job);
                                    if tx
                                        .send(PoolCompletion::Write(WriteCompletion { job, ok }))
                                        .is_err()
                                    {
                                        return;
                                    }
                                    if let Err(e) = event.write(1) {
                                        error!("failed to signal write completion: {e:?}");
                                    }
                                    continue;
                                }
                            };
                            let ok = (|| {
                                let mut writer = Writer::from_guest_buffers(
                                    &mem,
                                    job.buffers.iter().copied(),
                                )
                                .map_err(io::Error::other)?;
                                // `split_at` returns the remainder, so the receiver keeps the
                                // data region and the status byte stays outside it - the same
                                // arrangement the serial path makes.
                                writer
                                    .split_at(job.data_len)
                                    .map_err(io::Error::other)?;
                                let offset = job
                                    .sector
                                    .checked_mul(512)
                                    .ok_or_else(|| io::Error::other("invalid read sector"))?;
                                read_into_guest(&mut writer, &properties, job.data_len, offset)
                                    .map(|_| ())
                            })();
                            let ok = match ok {
                                Ok(()) => true,
                                Err(e) => {
                                    error!("error processing read request: {e:?}");
                                    false
                                }
                            };
                            if tx
                                .send(PoolCompletion::Read(ReadCompletion { job, ok }))
                                .is_err()
                            {
                                return;
                            }
                            if let Err(e) = event.write(1) {
                                error!("failed to signal read completion: {e:?}");
                            }
                        }
                    })
                    .unwrap()
            })
            .collect();
        Self {
            jobs: Some(jobs),
            completions,
            event,
            workers,
            pending: 0,
        }
    }
}

impl Drop for IoPool {
    fn drop(&mut self) {
        self.jobs.take();
        for worker in self.workers.drain(..) {
            if let Err(e) = worker.join() {
                error!("error waiting for pool worker thread: {e:?}");
            }
        }
    }
}

// Keep allocation fallible because the size comes from guest descriptors.
#[allow(clippy::slow_vector_initialization)]

fn write_from_guest(mem: &GuestMemoryMmap, disk: &DiskProperties, job: &WriteJob) -> bool {
    let result = (|| {
        let mut reader = Reader::from_guest_buffers(mem, job.readable.iter().copied())
            .map_err(io::Error::other)?;
        let mut data = reader
            .split_at(size_of::<RequestHeader>())
            .map_err(io::Error::other)?;
        let offset = job
            .sector
            .checked_mul(512)
            .ok_or_else(|| io::Error::other("invalid write sector"))?;
        data.read_to_at(disk, job.data_len, offset)
    })();
    match result {
        Ok(_) => true,
        Err(e) => {
            error!("error processing write request: {e:?}");
            false
        }
    }
}

fn write_write_completion(mem: &GuestMemoryMmap, completion: WriteCompletion) -> (u16, u32) {
    let head_index = completion.job.head_index;
    let status: u8 = if completion.ok {
        VIRTIO_BLK_S_OK.try_into().unwrap()
    } else {
        VIRTIO_BLK_S_IOERR.try_into().unwrap()
    };
    match Writer::from_guest_buffers(mem, completion.job.writable.iter().copied())
        .map_err(io::Error::other)
        .and_then(|mut w| w.write_obj(status))
    {
        Ok(()) => (head_index, 1),
        Err(e) => {
            error!("Failed to write virtio block status: {e:?}");
            (head_index, 0)
        }
    }
}

fn write_completion(mem: &GuestMemoryMmap, completion: PoolCompletion) -> (u16, u32) {
    match completion {
        PoolCompletion::Read(c) => write_read_completion(mem, c),
        PoolCompletion::Write(c) => write_write_completion(mem, c),
    }
}

fn write_read_completion(mem: &GuestMemoryMmap, completion: ReadCompletion) -> (u16, u32) {
    let head_index = completion.job.head_index;
    let mut writer = match Writer::from_guest_buffers(mem, completion.job.buffers.iter().copied()) {
        Ok(writer) => writer,
        Err(e) => {
            error!("invalid read completion descriptor chain: {e:?}");
            return (head_index, 0);
        }
    };
    let mut status_writer = match writer.split_at(completion.job.data_len) {
        Ok(writer) => writer,
        Err(e) => {
            error!("invalid read completion data length: {e:?}");
            return (head_index, 0);
        }
    };

    // The data is already in the guest's buffers - a pool worker pread into them - so
    // only the status byte is left to write here.
    let (status, len): (u8, usize) = if completion.ok {
        (VIRTIO_BLK_S_OK.try_into().unwrap(), completion.job.data_len)
    } else {
        (VIRTIO_BLK_S_IOERR.try_into().unwrap(), 0)
    };

    let status_len = match status_writer.write_obj(status) {
        Ok(()) => 1,
        Err(e) => {
            error!("Failed to write virtio block status: {e:?}");
            0
        }
    };

    (head_index, (len + status_len) as u32)
}

pub struct BlockWorker {
    device_queue: DeviceQueue,
    interrupt: InterruptTransport,
    mem: GuestMemoryMmap,
    disk: DiskProperties,
    stop_fd: EventFd,
    parallel_reads: bool,
    parallel_writes: bool,
    pool_size: Option<NonZeroUsize>,
    /// Guest ranges written by reads that are in flight in the pool, keyed by job id.
    inflight_read_ranges: HashMap<u64, Vec<(u64, u64)>>,
    next_read_id: u64,
}

impl BlockWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        device_queue: DeviceQueue,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        disk: DiskProperties,
        stop_fd: EventFd,
        parallel_reads: bool,
        parallel_writes: bool,
        pool_size: Option<NonZeroUsize>,
    ) -> Self {
        Self {
            device_queue,
            interrupt,
            mem,
            disk,
            stop_fd,
            parallel_reads,
            parallel_writes,
            pool_size,
            inflight_read_ranges: HashMap::new(),
            next_read_id: 0,
        }
    }

    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("block worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    fn work(mut self) {
        let virtq_ev_fd = self.device_queue.event.as_raw_fd();
        let stop_ev_fd = self.stop_fd.as_raw_fd();
        let pool_size = if self.parallel_reads || self.parallel_writes {
            resolve_pool_size(
                self.pool_size,
                thread::available_parallelism().ok().map(|n| n.get()),
            )
        } else {
            None
        };
        let mut pool = pool_size.map(|size| IoPool::new(size, &self.disk, &self.mem));
        let pool_completion_ev_fd = pool.as_ref().map(|pool| pool.event.as_raw_fd());
        let mut epoll = Epoll::new().unwrap();

        let _ = epoll.ctl(
            ControlOperation::Add,
            virtq_ev_fd,
            &EpollEvent::new(EventSet::IN, virtq_ev_fd as u64),
        );

        let _ = epoll.ctl(
            ControlOperation::Add,
            stop_ev_fd,
            &EpollEvent::new(EventSet::IN, stop_ev_fd as u64),
        );

        if let Some(fd) = pool_completion_ev_fd {
            let _ = epoll.ctl(
                ControlOperation::Add,
                fd,
                &EpollEvent::new(EventSet::IN, fd as u64),
            );
        }

        let mut epoll_events = vec![EpollEvent::new(EventSet::empty(), 0); 32];
        loop {
            match epoll.wait(epoll_events.len(), -1, epoll_events.as_mut_slice()) {
                Ok(ev_cnt) => {
                    for epoll_event in &epoll_events[0..ev_cnt] {
                        let source = epoll_event.fd();
                        let event_set = epoll_event.event_set();
                        match event_set {
                            EventSet::IN if source == virtq_ev_fd => {
                                if let Err(e) = self.device_queue.event.read() {
                                    error!("Failed to get queue event: {e:?}");
                                } else {
                                    self.process_virtio_queues(pool.as_mut());
                                }
                            }
                            EventSet::IN if Some(source) == pool_completion_ev_fd => {
                                let pool = pool.as_mut().unwrap();
                                if let Err(e) = pool.event.read() {
                                    error!("Failed to get read completion event: {e:?}");
                                }
                                self.process_pool_completions(pool);
                            }
                            EventSet::IN if source == stop_ev_fd => {
                                debug!("stopping worker thread");
                                let _ = self.stop_fd.read();
                                if let Some(pool) = pool.as_mut() {
                                    pool.jobs.take();
                                    for completion in pool.completions.iter() {
                                        let (head, len) = write_completion(&self.mem, completion);
                                        self.add_used(head, len);
                                    }
                                    self.signal_used_queue();
                                }
                                return;
                            }
                            _ => {
                                log::warn!(
                                    "Received unknown event: {event_set:?} from fd: {source:?}"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    debug!("failed to consume muxer epoll event: {e}");
                }
            }
        }
    }

    fn add_used(&mut self, head_index: u16, len: u32) {
        if let Err(e) = self.device_queue.queue.add_used(&self.mem, head_index, len) {
            error!("failed to add used elements to the queue: {e:?}");
        }
    }

    fn signal_used_queue(&mut self) {
        if self
            .device_queue
            .queue
            .needs_notification(&self.mem)
            .unwrap()
            && let Err(e) = self.interrupt.try_signal_used_queue()
        {
            error!("error signalling queue: {e:?}");
        }
    }

    /// Forget the guest ranges of a read that has finished.
    fn retire_read(&mut self, completion: &PoolCompletion) {
        if let PoolCompletion::Read(read) = completion {
            self.inflight_read_ranges.remove(&read.job.id);
        }
    }

    /// Block until no in-flight read writes any of `ranges`.
    ///
    /// Returns `false` only if the pool stops making progress, in which case the caller
    /// serves the request on this thread instead.
    fn wait_until_read_ranges_are_free(&mut self, pool: &mut IoPool, ranges: &[(u64, u64)]) -> bool {
        loop {
            let clash = self
                .inflight_read_ranges
                .values()
                .any(|inflight| ranges_overlap(inflight, ranges));
            if !clash {
                return true;
            }
            if pool.pending == 0 {
                return false;
            }
            let Ok(completion) = pool.completions.recv() else {
                return false;
            };
            pool.pending -= 1;
            self.retire_read(&completion);
            let (head, len) = write_completion(&self.mem, completion);
            self.add_used(head, len);
            self.signal_used_queue();
        }
    }

    fn process_pool_completions(&mut self, pool: &mut IoPool) {
        let pending = pool.pending;
        for completion in pool.completions.try_iter() {
            pool.pending -= 1;
            self.retire_read(&completion);
            let (head, len) = write_completion(&self.mem, completion);
            self.add_used(head, len);
        }
        if pool.pending != pending {
            self.signal_used_queue();
        }
    }

    /// Waits for everything in flight on the pool, for requests that must come
    /// after it: flushes, discards and write-zeroes.
    fn drain_pool(&mut self, pool: &mut IoPool) {
        while pool.pending > 0 {
            let completion = pool.completions.recv().unwrap();
            pool.pending -= 1;
            self.retire_read(&completion);
            let (head, len) = write_completion(&self.mem, completion);
            self.add_used(head, len);
        }
        self.signal_used_queue();
    }

    fn process_virtio_queues(&mut self, mut pool: Option<&mut IoPool>) {
        let mem = self.mem.clone();
        loop {
            self.device_queue.queue.disable_notification(&mem).unwrap();
            while let Some(head) = self.device_queue.queue.pop(&mem) {
                self.dispatch_request(&mem, pool.as_deref_mut(), head);
            }
            if !self.device_queue.queue.enable_notification(&mem).unwrap() {
                break;
            }
        }
    }

    fn dispatch_request(
        &mut self,
        mem: &GuestMemoryMmap,
        pool: Option<&mut IoPool>,
        head: DescriptorChain<'_>,
    ) {
        let head_index = head.index;
        let buffers: Vec<_> = head
            .clone()
            .into_iter()
            .writable()
            .map(|desc| (desc.addr, desc.len))
            .collect();
        let mut writer = match Writer::from_guest_buffers(mem, buffers.iter().copied()) {
            Ok(writer) => writer,
            Err(e) => {
                error!("invalid descriptor chain: {e:?}");
                self.add_used(head_index, 0);
                self.signal_used_queue();
                return;
            }
        };
        let Some(data_len) = writer.available_bytes().checked_sub(1) else {
            self.add_used(head_index, 0);
            self.signal_used_queue();
            return;
        };
        let status_writer = writer.split_at(data_len).unwrap();
        let mut reader = match Reader::new(mem, head.clone()) {
            Ok(r) => r,
            Err(e) => {
                error!("invalid descriptor chain: {e:?}");
                self.complete_request(
                    head_index,
                    status_writer,
                    Err(RequestError::InvalidDataLength),
                );
                return;
            }
        };
        let request_header: RequestHeader = match reader.read_obj() {
            Ok(h) => h,
            Err(e) => {
                error!("invalid request header: {e:?}");
                self.complete_request(
                    head_index,
                    status_writer,
                    Err(RequestError::ReadingFromDescriptor(e)),
                );
                return;
            }
        };

        if request_header.request_type == VIRTIO_BLK_T_IN
            && self.parallel_reads
            && let Some(pool) = pool
        {
            self.process_pool_completions(pool);
            if pool.pending == pool.workers.len() {
                let completion = pool.completions.recv().unwrap();
                pool.pending -= 1;
                self.retire_read(&completion);
                let (head, len) = write_completion(mem, completion);
                self.add_used(head, len);
                self.signal_used_queue();
            }
            if !data_len.is_multiple_of(512) {
                self.complete_request(
                    head_index,
                    status_writer,
                    Err(RequestError::InvalidDataLength),
                );
                return;
            }
            let ranges = guest_ranges(&buffers);
            if self.wait_until_read_ranges_are_free(pool, &ranges) {
                let id = self.next_read_id;
                self.next_read_id += 1;
                self.inflight_read_ranges.insert(id, ranges);
                pool.jobs
                    .as_ref()
                    .unwrap()
                    .send(PoolJob::Read(ReadJob {
                        head_index,
                        id,
                        sector: request_header.sector,
                        data_len,
                        buffers,
                    }))
                    .unwrap();
                pool.pending += 1;
                return;
            }
            // The pool never drained: fall through to the serial path, which writes the
            // same guest memory from this thread.
            debug!("pool reads still overlapping; serving this read serially");
        } else if request_header.request_type == VIRTIO_BLK_T_OUT
            && self.parallel_writes
            && let Some(pool) = pool
        {
            self.process_pool_completions(pool);
            if pool.pending == pool.workers.len() {
                let completion = pool.completions.recv().unwrap();
                pool.pending -= 1;
                self.retire_read(&completion);
                let (head, len) = write_completion(mem, completion);
                self.add_used(head, len);
                self.signal_used_queue();
            }
            let data_len = reader.available_bytes();
            if !data_len.is_multiple_of(512) {
                self.complete_request(
                    head_index,
                    status_writer,
                    Err(RequestError::InvalidDataLength),
                );
                return;
            }
            let readable = head
                .clone()
                .into_iter()
                .readable()
                .map(|desc| (desc.addr, desc.len))
                .collect();
            pool.jobs
                .as_ref()
                .unwrap()
                .send(PoolJob::Write(WriteJob {
                    head_index,
                    sector: request_header.sector,
                    data_len,
                    readable,
                    writable: buffers,
                }))
                .unwrap();
            pool.pending += 1;
            return;
        } else if matches!(
            request_header.request_type,
            VIRTIO_BLK_T_FLUSH | VIRTIO_BLK_T_DISCARD | VIRTIO_BLK_T_WRITE_ZEROES
        ) && let Some(pool) = pool
        {
            self.drain_pool(pool);
        }

        let result = Self::process_request(request_header, &mut reader, &mut writer, &self.disk);
        self.complete_request(head_index, status_writer, result);
    }

    fn complete_request(
        &mut self,
        head_index: u16,
        mut writer: Writer,
        result: result::Result<usize, RequestError>,
    ) {
        let (status, len): (u8, usize) = match result {
            Ok(l) => (VIRTIO_BLK_S_OK.try_into().unwrap(), l),
            Err(e) => {
                error!("error processing request: {e:?}");
                (VIRTIO_BLK_S_IOERR.try_into().unwrap(), 0)
            }
        };

        let status_len = match writer.write_obj(status) {
            Ok(()) => 1,
            Err(e) => {
                error!("Failed to write virtio block status: {e:?}");
                0
            }
        };
        self.add_used(head_index, (len + status_len) as u32);
        self.signal_used_queue();
    }

    fn process_request(
        request_header: RequestHeader,
        reader: &mut Reader,
        writer: &mut Writer,
        disk: &DiskProperties,
    ) -> result::Result<usize, RequestError> {
        match request_header.request_type {
            VIRTIO_BLK_T_IN => {
                let data_len = writer.available_bytes();
                let offset = request_header
                    .sector
                    .checked_mul(512)
                    .ok_or(RequestError::InvalidDataLength)?;
                if !data_len.is_multiple_of(512) {
                    Err(RequestError::InvalidDataLength)
                } else {
                    writer
                        .write_from_at(disk, data_len, offset)
                        .map_err(RequestError::WritingToDescriptor)
                }
            }
            VIRTIO_BLK_T_OUT => {
                let data_len = reader.available_bytes();
                if !data_len.is_multiple_of(512) {
                    Err(RequestError::InvalidDataLength)
                } else {
                    reader
                        .read_to_at(disk, data_len, request_header.sector * 512)
                        .map_err(RequestError::ReadingFromDescriptor)
                }
            }
            VIRTIO_BLK_T_FLUSH => match disk.cache_type() {
                CacheType::Writeback => {
                    let diskfile = disk.file.read().unwrap();
                    diskfile.flush().map_err(RequestError::FlushingToDisk)?;
                    diskfile.sync().map_err(RequestError::FlushingToDisk)?;
                    Ok(0)
                }
                CacheType::Unsafe => Ok(0),
            },
            VIRTIO_BLK_T_GET_ID => {
                let data_len = writer.available_bytes();
                let disk_id = disk.image_id();
                if data_len < disk_id.len() {
                    Err(RequestError::InvalidDataLength)
                } else {
                    writer
                        .write_all(disk_id)
                        .map_err(RequestError::WritingToDescriptor)?;
                    Ok(disk_id.len())
                }
            }
            VIRTIO_BLK_T_DISCARD => {
                let discard_write_data: DiscardWriteData = reader
                    .read_obj()
                    .map_err(RequestError::ReadingFromDescriptor)?;
                disk.file
                    .write()
                    .unwrap()
                    .discard_to_any(
                        discard_write_data.sector * 512,
                        discard_write_data.num_sectors as u64 * 512,
                    )
                    .map_err(RequestError::Discarding)?;
                Ok(0)
            }
            VIRTIO_BLK_T_WRITE_ZEROES => {
                let discard_write_data: DiscardWriteData = reader
                    .read_obj()
                    .map_err(RequestError::ReadingFromDescriptor)?;
                let unmap = (discard_write_data.flags & VIRTIO_BLK_WRITE_ZEROES_FLAG_UNMAP) != 0;
                if unmap {
                    disk.file
                        .write()
                        .unwrap()
                        .discard_to_zero(
                            discard_write_data.sector * 512,
                            discard_write_data.num_sectors as u64 * 512,
                        )
                        .map_err(RequestError::DiscardingToZero)?;
                } else {
                    disk.file
                        .read()
                        .unwrap()
                        .write_zeroes(
                            discard_write_data.sector * 512,
                            discard_write_data.num_sectors as u64 * 512,
                        )
                        .map_err(RequestError::WritingZeroes)?;
                }
                Ok(0)
            }
            _ => Err(RequestError::UnknownRequest),
        }
    }
}

fn resolve_pool_size(
    requested: Option<NonZeroUsize>,
    available_parallelism: Option<usize>,
) -> Option<usize> {
    match requested {
        Some(requested) => available_parallelism
            .filter(|available| *available >= requested.get())
            .map(|_| requested.get()),
        None => Some(available_parallelism.map(|n| n.clamp(2, 8)).unwrap_or(4)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::legacy::DummyIrqChip;
    use crate::virtio::descriptor_utils::{DescriptorType, create_descriptor_chain};
    use crate::virtio::queue::tests::VirtQueue;
    use imago::{DynStorage, FormatAccess, Storage, StorageOpenOptions, file::File, raw::Raw};
    use std::sync::{Arc, RwLock};
    use utils::tempfile::TempFile;
    use vm_memory::Bytes;

    fn test_disk(direct_io: bool) -> DiskProperties {
        test_disk_with(direct_io, false)
    }

    fn test_disk_with(direct_io: bool, writable: bool) -> DiskProperties {
        let backing = TempFile::new().unwrap();
        let mut file = backing.as_file();
        file.write_all(&[0x11; 512]).unwrap();
        file.write_all(&[0x22; 512]).unwrap();
        let file = File::open(
            StorageOpenOptions::new()
                .filename(backing.as_path().to_str().unwrap())
                .direct(direct_io)
                .write(writable),
        )
        .unwrap();
        let raw = Raw::<Box<dyn DynStorage>>::open_image(Box::new(file), writable).unwrap();
        DiskProperties::new(
            Arc::new(RwLock::new(FormatAccess::new(raw))),
            Vec::new(),
            CacheType::Unsafe,
        )
        .unwrap()
    }

    fn wait_for_completion(pool: &IoPool) {
        let mut epoll = Epoll::new().unwrap();
        epoll
            .ctl(
                ControlOperation::Add,
                pool.event.as_raw_fd(),
                &EpollEvent::new(EventSet::IN, 0),
            )
            .unwrap();
        let mut events = [EpollEvent::new(EventSet::empty(), 0)];
        assert_eq!(epoll.wait(1, 1000, &mut events).unwrap(), 1);
    }


    #[test]
    fn pooled_reads_write_straight_into_guest_memory() {
        // The pool must pread into the guest's own buffers: no staging buffer, no copy.
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let queue = VirtQueue::new(GuestAddress(0), &mem, 16);
        let head = create_descriptor_chain(
            &mem,
            GuestAddress(0),
            GuestAddress(0x1000),
            vec![
                (DescriptorType::Readable, 16),
                (DescriptorType::Writable, 512),
                (DescriptorType::Writable, 1),
            ],
            0,
        )
        .unwrap();
        mem.write_obj(
            RequestHeader {
                request_type: VIRTIO_BLK_T_IN,
                sector: 0,
                ..Default::default()
            },
            GuestAddress(0x1000),
        )
        .unwrap();
        let data_addr = GuestAddress(0x1010);
        let status_addr = GuestAddress(0x1210);
        mem.write_slice(&[0xaa; 512], data_addr).unwrap();

        let mut worker = BlockWorker::new(
            DeviceQueue::new(
                queue.create_queue(),
                Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
            ),
            InterruptTransport::new(DummyIrqChip::new().into(), "block test".into()).unwrap(),
            mem.clone(),
            test_disk(false),
            EventFd::new(EFD_NONBLOCK).unwrap(),
            true,
            false,
            None,
        );
        let mut pool = IoPool::new(1, &worker.disk, &mem);
        worker.dispatch_request(&mem, Some(&mut pool), head);
        assert_eq!(pool.pending, 1, "the read must be handed to the pool");

        wait_for_completion(&pool);
        worker.process_pool_completions(&mut pool);

        let mut data = [0u8; 512];
        mem.read_slice(&mut data, data_addr).unwrap();
        assert_eq!(data, [0x11; 512], "the pool worker must write the guest's buffers");
        assert_eq!(mem.read_obj::<u8>(status_addr).unwrap(), VIRTIO_BLK_S_OK as u8);
    }


    #[test]
    fn overlapping_guest_read_ranges_are_detected() {
        // Disjoint: two halves of one buffer, and a neighbouring buffer.
        assert!(!ranges_overlap(&[(0x1000, 512)], &[(0x1200, 512)]));
        assert!(!ranges_overlap(&[(0x1000, 512)], &[(0x2000, 16)]));
        // Overlapping: a shared byte, a containing range, and an identical range.
        assert!(ranges_overlap(&[(0x1000, 512)], &[(0x11ff, 512)]));
        assert!(ranges_overlap(&[(0x1000, 1024)], &[(0x1200, 16)]));
        assert!(ranges_overlap(&[(0x1000, 512)], &[(0x1000, 512)]));
        // Empty ranges never clash.
        assert!(!ranges_overlap(&[], &[(0x1000, 512)]));
    }

    #[test]
    fn serialized_and_fallback_reads_keep_status_outside_data() {
        for (parallel, data_len, sector, status, used_len) in [
            (false, 512, 0, VIRTIO_BLK_S_OK, 513),
            (false, 512, u64::MAX, VIRTIO_BLK_S_IOERR, 1),
            (true, 512, 0, VIRTIO_BLK_S_OK, 513),
            (true, 512, u64::MAX, VIRTIO_BLK_S_IOERR, 1),
            (true, 512, u64::MAX, VIRTIO_BLK_S_IOERR, 1),
            (true, 511, 0, VIRTIO_BLK_S_IOERR, 1),
        ] {
            let mem =
                GuestMemoryMmap::from_ranges(&[(GuestAddress(0), data_len + 0x2000)]).unwrap();
            let queue = VirtQueue::new(GuestAddress(0), &mem, 16);
            let head = create_descriptor_chain(
                &mem,
                GuestAddress(0),
                GuestAddress(0x1000),
                vec![
                    (DescriptorType::Readable, 16),
                    (DescriptorType::Writable, (data_len + 1) as u32),
                ],
                0,
            )
            .unwrap();
            mem.write_obj(
                RequestHeader {
                    request_type: VIRTIO_BLK_T_IN,
                    sector,
                    ..Default::default()
                },
                GuestAddress(0x1000),
            )
            .unwrap();
            let data_addr = GuestAddress(0x1010);
            let status_addr = GuestAddress(0x1010 + data_len as u64);
            mem.write_obj(0xaa_u8, data_addr).unwrap();
            mem.write_obj(0xff_u8, status_addr).unwrap();
            let mut worker = BlockWorker::new(
                DeviceQueue::new(
                    queue.create_queue(),
                    Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
                ),
                InterruptTransport::new(DummyIrqChip::new().into(), "block test".into()).unwrap(),
                mem.clone(),
                test_disk(false),
                EventFd::new(EFD_NONBLOCK).unwrap(),
                parallel,
                false,
                None,
            );
            let mut pool = parallel.then(|| IoPool::new(2, &worker.disk, &mem));
            worker.dispatch_request(&mem, pool.as_mut(), head);
            if let Some(pool) = pool.as_mut()
                && pool.pending != 0
            {
                assert_eq!(mem.read_obj::<u8>(status_addr).unwrap(), 0xff);
                mem.write_slice(&[0; 32], GuestAddress(0)).unwrap();
                wait_for_completion(pool);
                worker.process_pool_completions(pool);
            }

            assert_eq!(mem.read_obj::<u8>(status_addr).unwrap(), status as u8);
            assert_eq!(queue.used.idx.get(), 1);
            assert_eq!(queue.used.ring[0].get().len, used_len);
            assert_eq!(
                mem.read_obj::<u8>(data_addr).unwrap(),
                if status == VIRTIO_BLK_S_OK {
                    0x11
                } else {
                    0xaa
                }
            );
            assert!(pool.as_ref().is_none_or(|pool| pool.pending == 0));
        }
    }

    #[test]
    fn pooled_reads_drain_on_drop_and_keep_their_own_buffers() {
        // Disjoint guest buffers: the pool may run both at once, and each must land in its
        // own region with only the status byte written by the completion path.
        for direct_io in [false, true] {
            let disk = test_disk(direct_io);
            let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x1000)]).unwrap();
            mem.write_slice(&[0xff; 513], GuestAddress(0x100)).unwrap();
            mem.write_slice(&[0xff; 513], GuestAddress(0x400)).unwrap();
            let mut pool = IoPool::new(2, &disk, &mem);
            let completions = pool.completions.clone();
            for (sector, addr) in [(0u64, 0x100u64), (1, 0x400)] {
                pool.jobs
                    .as_ref()
                    .unwrap()
                    .send(PoolJob::Read(ReadJob {
                        head_index: sector as u16,
                        id: sector,
                        sector,
                        data_len: 512,
                        buffers: vec![(GuestAddress(addr), 513)],
                    }))
                    .unwrap();
            }
            wait_for_completion(&pool);
            pool.jobs.take();
            drop(pool);

            let mut completed = 0;
            for completion in completions.try_iter() {
                let PoolCompletion::Read(completion) = completion else {
                    panic!("a read completed as a write");
                };
                let head = completion.job.head_index;
                assert_eq!(write_read_completion(&mem, completion), (head, 513));
                let addr = if head == 0 { 0x100 } else { 0x400 };
                let mut data = [0; 513];
                mem.read_slice(&mut data, GuestAddress(addr)).unwrap();
                assert_eq!(
                    &data[..512],
                    &[if head == 0 { 0x11 } else { 0x22 }; 512],
                    "each pooled read must fill its own guest buffers"
                );
                assert_eq!(data[512], VIRTIO_BLK_S_OK as u8);
                completed += 1;
            }
            assert_eq!(completed, 2);
            assert!(completions.recv().is_err());
        }
    }

    #[test]
    fn overlapping_pooled_reads_are_admitted_one_at_a_time() {
        // Two reads aimed at the same guest bytes must not run concurrently: the dispatcher
        // admits one and defers the other until the first has retired.
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let queue = VirtQueue::new(GuestAddress(0), &mem, 16);
        let mut heads = Vec::new();
        for sector in 0..2u64 {
            let head = create_descriptor_chain(
                &mem,
                GuestAddress(0),
                GuestAddress(0x1000),
                vec![
                    (DescriptorType::Readable, 16),
                    (DescriptorType::Writable, 512),
                    (DescriptorType::Writable, 1),
                ],
                0,
            )
            .unwrap();
            mem.write_obj(
                RequestHeader {
                    request_type: VIRTIO_BLK_T_IN,
                    sector,
                    ..Default::default()
                },
                GuestAddress(0x1000),
            )
            .unwrap();
            heads.push(head);
        }
        let mut worker = BlockWorker::new(
            DeviceQueue::new(
                queue.create_queue(),
                Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
            ),
            InterruptTransport::new(DummyIrqChip::new().into(), "block test".into()).unwrap(),
            mem.clone(),
            test_disk(false),
            EventFd::new(EFD_NONBLOCK).unwrap(),
            true,
            false,
            None,
        );
        let mut pool = IoPool::new(1, &worker.disk, &mem);
        worker.dispatch_request(&mem, Some(&mut pool), heads.remove(0));
        assert_eq!(pool.pending, 1, "the first read goes to the pool");
        assert_eq!(worker.inflight_read_ranges.len(), 1);
        // The second overlaps the first, so it is served only after that one retires; it
        // must not leave two entries in flight over the same bytes.
        worker.dispatch_request(&mem, Some(&mut pool), heads.remove(0));
        assert_eq!(
            worker.inflight_read_ranges.len(),
            1,
            "at most one read may own the overlapping range"
        );
        assert_eq!(pool.pending, 1, "the deferred read replaced the retired one");
    }

    #[test]
    fn pooled_writes_reach_the_disk_and_complete_with_status_alone() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let queue = VirtQueue::new(GuestAddress(0), &mem, 16);
        let head = create_descriptor_chain(
            &mem,
            GuestAddress(0),
            GuestAddress(0x1000),
            vec![
                (DescriptorType::Readable, 16),
                (DescriptorType::Readable, 512),
                (DescriptorType::Writable, 1),
            ],
            0,
        )
        .unwrap();
        mem.write_obj(
            RequestHeader {
                request_type: VIRTIO_BLK_T_OUT,
                sector: 1,
                ..Default::default()
            },
            GuestAddress(0x1000),
        )
        .unwrap();
        mem.write_slice(&[0x5a; 512], GuestAddress(0x1010)).unwrap();
        let status_addr = GuestAddress(0x1210);
        mem.write_obj(0xff_u8, status_addr).unwrap();
        let mut worker = BlockWorker::new(
            DeviceQueue::new(
                queue.create_queue(),
                Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
            ),
            InterruptTransport::new(DummyIrqChip::new().into(), "block test".into()).unwrap(),
            mem.clone(),
            test_disk_with(false, true),
            EventFd::new(EFD_NONBLOCK).unwrap(),
            true,
            true,
            None,
        );
        let mut pool = IoPool::new(2, &worker.disk, &mem);
        worker.dispatch_request(&mem, Some(&mut pool), head);
        assert_eq!(pool.pending, 1);
        wait_for_completion(&pool);
        worker.process_pool_completions(&mut pool);

        assert_eq!(pool.pending, 0);
        assert_eq!(
            mem.read_obj::<u8>(status_addr).unwrap(),
            VIRTIO_BLK_S_OK as u8
        );
        assert_eq!(queue.used.idx.get(), 1);
        assert_eq!(queue.used.ring[0].get().len, 1);
        let mut data = [0; 1024];
        worker
            .disk
            .file
            .read()
            .unwrap()
            .read(&mut data[..], 0)
            .unwrap();
        assert_eq!(&data[..512], &[0x11; 512]);
        assert_eq!(&data[512..], &[0x5a; 512]);
    }

    #[test]
    fn reads_stay_serialized_when_only_writes_are_parallel() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x4000)]).unwrap();
        let queue = VirtQueue::new(GuestAddress(0), &mem, 16);
        let head = create_descriptor_chain(
            &mem,
            GuestAddress(0),
            GuestAddress(0x1000),
            vec![
                (DescriptorType::Readable, 16),
                (DescriptorType::Writable, 512),
                (DescriptorType::Writable, 1),
            ],
            0,
        )
        .unwrap();
        mem.write_obj(
            RequestHeader {
                request_type: VIRTIO_BLK_T_IN,
                sector: 0,
                ..Default::default()
            },
            GuestAddress(0x1000),
        )
        .unwrap();
        let status_addr = GuestAddress(0x1210);
        mem.write_obj(0xff_u8, status_addr).unwrap();
        let mut worker = BlockWorker::new(
            DeviceQueue::new(
                queue.create_queue(),
                Arc::new(EventFd::new(EFD_NONBLOCK).unwrap()),
            ),
            InterruptTransport::new(DummyIrqChip::new().into(), "block test".into()).unwrap(),
            mem.clone(),
            test_disk(false),
            EventFd::new(EFD_NONBLOCK).unwrap(),
            false,
            true,
            None,
        );
        let mut pool = IoPool::new(2, &worker.disk, &mem);
        worker.dispatch_request(&mem, Some(&mut pool), head);

        assert_eq!(pool.pending, 0);
        assert_eq!(
            mem.read_obj::<u8>(status_addr).unwrap(),
            VIRTIO_BLK_S_OK as u8
        );
        let mut data = [0; 512];
        mem.read_slice(&mut data, GuestAddress(0x1010)).unwrap();
        assert_eq!(data, [0x11; 512]);
    }

    #[test]
    fn explicit_pool_size_requires_enough_available_parallelism() {
        assert_eq!(resolve_pool_size(NonZeroUsize::new(4), Some(4)), Some(4));
        assert_eq!(resolve_pool_size(NonZeroUsize::new(5), Some(4)), None);
        assert_eq!(resolve_pool_size(NonZeroUsize::new(4), None), None);
    }

    #[test]
    fn automatic_pool_size_uses_existing_sizing() {
        assert_eq!(resolve_pool_size(None, Some(1)), Some(2));
        assert_eq!(resolve_pool_size(None, Some(16)), Some(8));
        assert_eq!(resolve_pool_size(None, None), Some(4));
    }

    #[test]
    fn read_completion_writes_only_status_and_reports_the_data_length() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let chain = create_descriptor_chain(
            &mem,
            GuestAddress(0),
            GuestAddress(0x100),
            vec![(DescriptorType::Readable, 8), (DescriptorType::Writable, 5)],
            0,
        )
        .unwrap();
        let buffers: Vec<_> = chain
            .into_iter()
            .writable()
            .map(|desc| (desc.addr, desc.len))
            .collect();
        mem.write_slice(&[0; 32], GuestAddress(0)).unwrap();
        // The pool worker has already pread into the data region; the completion must not
        // touch it and must report data length + status byte as used.
        mem.write_slice(&[0x11, 0x22, 0x33, 0x44], GuestAddress(0x108))
            .unwrap();

        let used = write_read_completion(
            &mem,
            ReadCompletion {
                job: ReadJob {
                    head_index: 0,
                    id: 0,
                    sector: 0,
                    data_len: 4,
                    buffers,
                },
                ok: true,
            },
        );

        assert_eq!(used, (0, 5));
        let mut data = [0; 4];
        let mut status = [0; 1];
        mem.read_slice(&mut data, GuestAddress(0x108)).unwrap();
        mem.read_slice(&mut status, GuestAddress(0x10c)).unwrap();
        assert_eq!(data, [0x11, 0x22, 0x33, 0x44]);
        assert_eq!(status, [VIRTIO_BLK_S_OK as u8]);
    }

    #[test]
    fn failed_read_completion_writes_status_after_data_region() {
        let mem = GuestMemoryMmap::from_ranges(&[(GuestAddress(0), 0x10000)]).unwrap();
        let chain = create_descriptor_chain(
            &mem,
            GuestAddress(0),
            GuestAddress(0x100),
            vec![
                (DescriptorType::Readable, 8),
                (DescriptorType::Writable, 4),
                (DescriptorType::Writable, 1),
            ],
            0,
        )
        .unwrap();
        let buffers: Vec<_> = chain
            .into_iter()
            .writable()
            .map(|desc| (desc.addr, desc.len))
            .collect();

        let used = write_read_completion(
            &mem,
            ReadCompletion {
                job: ReadJob {
                    head_index: 0,
                    id: 0,
                    sector: 0,
                    data_len: 4,
                    buffers,
                },
                ok: false,
            },
        );

        assert_eq!(used, (0, 1));
        let mut status = [0; 1];
        mem.read_slice(&mut status, GuestAddress(0x10c)).unwrap();
        assert_eq!(status, [VIRTIO_BLK_S_IOERR as u8]);
    }
}
