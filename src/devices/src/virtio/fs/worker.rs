#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;

use std::io;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::AtomicI32;
use std::thread;
#[cfg(windows)]
use utils::windows::AsRawFd;

use crossbeam_channel::{Receiver, bounded, unbounded};
use utils::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
use utils::eventfd::{EFD_NONBLOCK, EventFd};
use vm_memory::{GuestAddress, GuestMemoryMmap};

use super::super::{DescriptorChain, Queue};
use super::augment_fs::AugmentFs;
use super::defs::{HPQ_INDEX, REQ_INDEX};
use super::descriptor_utils::{Reader, Writer};
use super::inode_alloc::InodeAllocator;
use super::null_fs::NullFs;
use super::passthrough::{self, PassthroughFs};
use super::read_only::PassthroughFsRo;
use super::server::Server;
use super::virtual_entry::VirtualDirEntry;
use crate::virtio::{InterruptTransport, VirtioShmRegion};

/// The number of pool threads a device asks for when the caller sets nothing.
///
/// Four is deliberately conservative. The measured case for a pool is
/// READDIRPLUS, where a single guest thread already saturates the one server
/// thread (note 20 in cang); the open/read/stat classes leave that thread at
/// ~27% of a core, so they gain little and lose nothing. Every pool thread is a
/// host thread with its own 1 MiB stack, so the default stays small.
pub(super) const DEFAULT_WORKER_THREADS: usize = 4;

/// How many queued jobs the pool accepts per thread before the dispatcher
/// thread blocks.
///
/// The dispatcher blocking is the intended backpressure: the guest's requests
/// that are already complete keep being published from the completion channel,
/// which is unbounded, so a burst cannot deadlock the pair.
const JOB_DEPTH_PER_THREAD: usize = 4;

/// Clamp a requested pool size to what this host can actually run.
///
/// One thread means the dispatcher serves requests inline, exactly as it did
/// before the pool existed, so a caller can turn the pool off without a
/// separate switch.
pub(super) fn resolve_worker_threads(requested: usize) -> usize {
    let available = thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    requested.clamp(1, available.max(1))
}

/// One request handed from the dispatcher thread to a pool thread.
///
/// The dispatcher thread owns the available and used rings; a pool thread only
/// reads the descriptor chain this head index names and writes the FUSE reply
/// into it. The job therefore carries the descriptor-table address and queue
/// size rather than a [`DescriptorChain`]: a `DescriptorChain` - and the
/// `Reader`/`Writer` built from one - borrows the `GuestMemoryMmap`, so it
/// cannot cross a thread boundary. Rebuilding it costs one `GuestAddress` read
/// per chain and leaves the rings on the dispatcher thread, which is the whole
/// point of the split.
struct FsJob {
    queue_index: usize,
    index: u16,
    desc_table: GuestAddress,
    queue_size: u16,
}

/// A served request on its way back to the dispatcher thread, which is the only
/// thread that may touch the used ring.
struct FsCompletion {
    queue_index: usize,
    index: u16,
    len: u32,
}

/// The shared, immutable-per-request device state a pool thread needs.
struct PoolContext {
    mem: GuestMemoryMmap,
    server: Arc<FsServer>,
    allow_idmap: bool,
    shm_region: Option<VirtioShmRegion>,
    exit_code: Arc<AtomicI32>,
    #[cfg(target_os = "macos")]
    map_sender: Option<Sender<WorkerMessage>>,
}

/// A bounded pool of request-serving threads behind the single virtio-fs
/// request queue.
///
/// virtio-fs has no multiqueue feature: the guest driver maps one request queue
/// no matter how many vCPUs it has (`defs::NUM_QUEUES`, `num_request_queues =
/// 1`), so the only way to use more than one host core is to serve that queue's
/// descriptors from several threads - what upstream virtiofsd's
/// `process_queue_pool` does. Completions arrive out of order and that is
/// fine: the FUSE client matches a reply to its request by the `unique` field
/// in the header, and the driver returns the descriptor chain whose head index
/// the used ring names, so no completion order is implied by the protocol.
struct FsPool {
    jobs: Option<crossbeam_channel::Sender<FsJob>>,
    completions: Receiver<FsCompletion>,
    event: EventFd,
    workers: Vec<thread::JoinHandle<()>>,
    /// Requests handed to the pool whose reply is not published yet. Read only
    /// by the dispatcher thread, to tell "nobody else is in flight" from "the
    /// pool is busy and this request belongs with them".
    pending: usize,
}

impl FsPool {
    fn new(size: usize, ctx: PoolContext) -> Self {
        let (jobs, job_rx) = bounded::<FsJob>(size * JOB_DEPTH_PER_THREAD);
        // Workers must be able to finish while the dispatcher joins them.
        let (completion_tx, completions) = unbounded::<FsCompletion>();
        let event = EventFd::new(EFD_NONBLOCK).expect("failed to create fs worker completion fd");
        let ctx = Arc::new(ctx);
        let workers = (0..size)
            .map(|i| {
                let job_rx: Receiver<FsJob> = job_rx.clone();
                let completion_tx = completion_tx.clone();
                let event = event.try_clone().unwrap();
                let ctx = ctx.clone();
                thread::Builder::new()
                    .name(format!("fs work {i}"))
                    .spawn(move || serve_jobs(&job_rx, &completion_tx, &event, &ctx))
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

    fn dispatch(&mut self, job: FsJob) {
        let Some(jobs) = self.jobs.as_ref() else {
            return;
        };
        // Blocking here is the backpressure described on JOB_DEPTH_PER_THREAD.
        // The count is bumped before the send so a reply that completes
        // instantly cannot be published before it was counted.
        self.pending += 1;
        if let Err(e) = jobs.send(job) {
            self.pending -= 1;
            error!("failed to hand a request to the pool: {e}");
        }
    }
}

impl Drop for FsPool {
    fn drop(&mut self) {
        // Closing the job channel makes every worker finish its current request
        // and exit, so the joins below cannot hang on a busy share.
        self.jobs.take();
        for worker in self.workers.drain(..) {
            if let Err(e) = worker.join() {
                error!("error waiting for fs worker thread: {e:?}");
            }
        }
    }
}

/// A pool thread: serve one descriptor chain, hand the reply length back.
fn serve_jobs(
    jobs: &Receiver<FsJob>,
    completions: &crossbeam_channel::Sender<FsCompletion>,
    event: &EventFd,
    ctx: &PoolContext,
) {
    while let Ok(job) = jobs.recv() {
        let len = serve_chain(
            &ctx.mem,
            &ctx.server,
            DescriptorChain::checked_new(&ctx.mem, job.desc_table, job.queue_size, job.index),
            ctx.allow_idmap,
            &ctx.shm_region,
            &ctx.exit_code,
            #[cfg(target_os = "macos")]
            &ctx.map_sender,
        );
        if completions
            .send(FsCompletion {
                queue_index: job.queue_index,
                index: job.index,
                len,
            })
            .is_err()
        {
            return;
        }
        if let Err(e) = event.write(1) {
            error!("failed to signal a fuse completion: {e:?}");
        }
    }
}

enum FsServer {
    ReadWrite(Server<AugmentFs<PassthroughFs>>),
    ReadOnly(Server<AugmentFs<PassthroughFsRo>>),
    Null(Server<AugmentFs<NullFs>>),
}

impl FsServer {
    /// Turn on zero-message opens (`FUSE_NO_OPEN_SUPPORT` /
    /// `FUSE_NO_OPENDIR_SUPPORT`): `open`/`opendir` answer `-ENOSYS` and the
    /// negotiated flag set is adjusted to match. See `Server::set_zero_message_open`.
    fn set_zero_message_open(&self, enabled: bool) {
        match self {
            FsServer::ReadWrite(s) => s.set_zero_message_open(enabled),
            FsServer::ReadOnly(s) => s.set_zero_message_open(enabled),
            FsServer::Null(s) => s.set_zero_message_open(enabled),
        }
    }

    /// Report the opcode mix served so far.
    fn log_opcode_histogram(&self, tag: &str) {
        match self {
            FsServer::ReadWrite(s) => s.log_opcode_histogram(tag),
            FsServer::ReadOnly(s) => s.log_opcode_histogram(tag),
            FsServer::Null(s) => s.log_opcode_histogram(tag),
        }
    }

    /// Whether the server reports its opcode histogram at request milestones.
    fn set_opcode_stats(&self, enabled: bool) {
        match self {
            FsServer::ReadWrite(s) => s.set_opcode_stats(enabled),
            FsServer::ReadOnly(s) => s.set_opcode_stats(enabled),
            FsServer::Null(s) => s.set_opcode_stats(enabled),
        }
    }
}

impl FsServer {
    fn handle_message(
        &self,
        r: Reader,
        w: Writer,
        allow_idmap: bool,
        shm_region: &Option<VirtioShmRegion>,
        exit_code: &Arc<AtomicI32>,
        #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
    ) -> super::Result<usize> {
        match self {
            FsServer::ReadWrite(s) => s.handle_message(
                r,
                w,
                allow_idmap,
                shm_region,
                exit_code,
                #[cfg(target_os = "macos")]
                map_sender,
            ),
            FsServer::ReadOnly(s) => s.handle_message(
                r,
                w,
                allow_idmap,
                shm_region,
                exit_code,
                #[cfg(target_os = "macos")]
                map_sender,
            ),
            FsServer::Null(s) => s.handle_message(
                r,
                w,
                allow_idmap,
                shm_region,
                exit_code,
                #[cfg(target_os = "macos")]
                map_sender,
            ),
        }
    }
}

/// Serve one descriptor chain and return the length to publish in its used-ring
/// entry.
///
/// Descriptor chains come from the guest, so a malformed one is a protocol
/// error for *that* request, not a reason to end the server: complete it with a
/// zero-length reply (the guest reports a fuse protocol error and retries) and
/// keep serving the share. The same holds for a panic inside a handler, and in
/// a pool it matters more than before: a wedged worker would take a share of
/// the guest's root filesystem down with it, and a poisoned `RwLock` would turn
/// every later request into a panic too.
fn serve_chain<'a>(
    mem: &'a GuestMemoryMmap,
    server: &FsServer,
    chain: Option<DescriptorChain<'a>>,
    allow_idmap: bool,
    shm_region: &Option<VirtioShmRegion>,
    exit_code: &Arc<AtomicI32>,
    #[cfg(target_os = "macos")] map_sender: &Option<Sender<WorkerMessage>>,
) -> u32 {
    let Some(chain) = chain else {
        error!("fs: failing request with an invalid descriptor chain");
        return 0;
    };
    let index = chain.index;
    let reader = match Reader::new(mem, chain.clone()) {
        Ok(reader) => reader,
        Err(e) => {
            error!("fs: failing request with unreadable descriptor chain {index}: {e:?}");
            return 0;
        }
    };
    let writer = match Writer::new(mem, chain) {
        Ok(writer) => writer,
        Err(e) => {
            error!("fs: failing request with unwritable descriptor chain {index}: {e:?}");
            return 0;
        }
    };
    let served = std::panic::catch_unwind(AssertUnwindSafe(|| {
        server.handle_message(
            reader,
            writer,
            allow_idmap,
            shm_region,
            exit_code,
            #[cfg(target_os = "macos")]
            map_sender,
        )
    }));
    match served {
        Ok(Ok(len)) => len as u32,
        Ok(Err(e)) => {
            error!("error handling message: {e:?}");
            0
        }
        Err(_) => {
            error!("fs: a fuse handler panicked; failing request {index}");
            0
        }
    }
}

pub struct FsWorker {
    queues: Vec<Queue>,
    queue_evts: Vec<Arc<EventFd>>,
    interrupt: InterruptTransport,
    mem: GuestMemoryMmap,
    allow_idmap: bool,
    shm_region: Option<VirtioShmRegion>,
    server: Arc<FsServer>,
    stop_fd: EventFd,
    exit_code: Arc<AtomicI32>,
    worker_threads: usize,
    /// Whether to report the served opcode mix when the device stops
    /// (`CANG_FS_STATS` on the cang side). Off in a normal launch: the map that
    /// wanted it has the numbers now, and a warning per VM is not worth a line.
    opcode_stats: bool,
    pool: Option<FsPool>,
    #[cfg(target_os = "macos")]
    map_sender: Option<Sender<WorkerMessage>>,
}

impl FsWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        queues: Vec<Queue>,
        queue_evts: Vec<Arc<EventFd>>,
        interrupt: InterruptTransport,
        mem: GuestMemoryMmap,
        allow_idmap: bool,
        shm_region: Option<VirtioShmRegion>,
        passthrough_cfg: Option<passthrough::Config>,
        read_only: bool,
        virtual_entries: Vec<VirtualDirEntry<'static>>,
        stop_fd: EventFd,
        exit_code: Arc<AtomicI32>,
        worker_threads: usize,
        opcode_stats: bool,
        #[cfg(target_os = "macos")] map_sender: Option<Sender<WorkerMessage>>,
    ) -> Result<Self, io::Error> {
        let inode_alloc = Arc::new(InodeAllocator::new());
        let zero_message_open = passthrough_cfg
            .as_ref()
            .is_some_and(|cfg| cfg.zero_message_open);
        let server = match passthrough_cfg {
            Some(cfg) if read_only => {
                let inner = PassthroughFsRo::new(cfg, inode_alloc.clone())?;
                FsServer::ReadOnly(Server::new(AugmentFs::new(
                    inner,
                    &inode_alloc,
                    virtual_entries,
                )))
            }
            Some(cfg) => {
                let inner = PassthroughFs::new(cfg, inode_alloc.clone())?;
                FsServer::ReadWrite(Server::new(AugmentFs::new(
                    inner,
                    &inode_alloc,
                    virtual_entries,
                )))
            }
            None => FsServer::Null(Server::new(AugmentFs::new(
                NullFs,
                &inode_alloc,
                virtual_entries,
            ))),
        };
        server.set_zero_message_open(zero_message_open);
        server.set_opcode_stats(opcode_stats);
        Ok(Self {
            queues,
            queue_evts,
            interrupt,
            mem,
            allow_idmap,
            shm_region,
            server: Arc::new(server),
            stop_fd,
            exit_code,
            worker_threads: resolve_worker_threads(worker_threads),
            opcode_stats,
            pool: None,
            #[cfg(target_os = "macos")]
            map_sender,
        })
    }

    pub fn run(self) -> thread::JoinHandle<()> {
        thread::Builder::new()
            .name("fs worker".into())
            .spawn(|| self.work())
            .unwrap()
    }

    /// Open the pool, if this device asked for more than one thread.
    fn start_pool(&mut self) {
        if self.worker_threads <= 1 {
            return;
        }
        self.pool = Some(FsPool::new(
            self.worker_threads,
            PoolContext {
                mem: self.mem.clone(),
                server: self.server.clone(),
                allow_idmap: self.allow_idmap,
                shm_region: self.shm_region.clone(),
                exit_code: self.exit_code.clone(),
                #[cfg(target_os = "macos")]
                map_sender: self.map_sender.clone(),
            },
        ));
        debug!(
            "fs: serving the request queue from {} pool threads",
            self.worker_threads
        );
    }

    fn work(mut self) {
        self.start_pool();

        let virtq_hpq_ev_fd = self.queue_evts[HPQ_INDEX].as_raw_fd();
        let virtq_req_ev_fd = self.queue_evts[REQ_INDEX].as_raw_fd();
        let stop_ev_fd = self.stop_fd.as_raw_fd();
        let completion_ev_fd = self.pool.as_ref().map(|pool| pool.event.as_raw_fd());

        let mut epoll = Epoll::new().unwrap();

        let _ = epoll.ctl(
            ControlOperation::Add,
            virtq_hpq_ev_fd,
            &EpollEvent::new(EventSet::IN, virtq_hpq_ev_fd as u64),
        );
        let _ = epoll.ctl(
            ControlOperation::Add,
            virtq_req_ev_fd,
            &EpollEvent::new(EventSet::IN, virtq_req_ev_fd as u64),
        );
        let _ = epoll.ctl(
            ControlOperation::Add,
            stop_ev_fd,
            &EpollEvent::new(EventSet::IN, stop_ev_fd as u64),
        );
        if let Some(fd) = completion_ev_fd {
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
                    for event in &epoll_events[0..ev_cnt] {
                        let source = event.fd();
                        let event_set = event.event_set();
                        match event_set {
                            EventSet::IN if source == virtq_hpq_ev_fd => {
                                self.handle_event(HPQ_INDEX);
                            }
                            EventSet::IN if source == virtq_req_ev_fd => {
                                self.handle_event(REQ_INDEX);
                            }
                            EventSet::IN if Some(source) == completion_ev_fd => {
                                self.handle_completions();
                            }
                            EventSet::IN if source == stop_ev_fd => {
                                debug!("stopping worker thread");
                                let _ = self.stop_fd.read();
                                // Drain what the pool already served before the
                                // threads are joined, so a stopping share does
                                // not drop replies the guest could still use.
                                self.handle_completions();
                                self.log_opcode_histogram();
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

    /// Report the opcode mix this device served, once, when it stops.
    ///
    /// The FUSE round trip turned out to cost the same per *request* whatever the
    /// opcode (note 20/22 of the workspace-virtiofs-performance map), so which
    /// opcodes a workload actually issues - and in what proportion - is what
    /// every request-removing lever has to be priced against. The guest kernel
    /// pinned for it has no ftrace and no debugfs, so the count can only come
    /// from here. Emitted at `warn` because cang floors libkrun's log level
    /// there: the line has to survive a default launch.
    fn log_opcode_histogram(&self) {
        if !self.opcode_stats {
            return;
        }
        self.server.log_opcode_histogram("teardown");
    }

    fn handle_event(&mut self, queue_index: usize) {
        debug!("Fs: queue event: {queue_index}");
        if let Err(e) = self.queue_evts[queue_index].read() {
            error!("Failed to get queue event: {e:?}");
        }

        // A guest-memory error while (un)masking notifications must not take
        // the worker down: it is the only thread serving this share, so
        // panicking would leave every later request unanswered forever.
        loop {
            if let Err(e) = self.queues[queue_index].disable_notification(&self.mem) {
                error!("failed to disable notifications on queue {queue_index}: {e}");
                return;
            }

            self.process_queue(queue_index);

            match self.queues[queue_index].enable_notification(&self.mem) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    error!("failed to enable notifications on queue {queue_index}: {e}");
                    return;
                }
            }
        }
    }

    /// Publish every reply the pool has finished, in completion order.
    fn handle_completions(&mut self) {
        let Some(pool) = self.pool.as_mut() else {
            return;
        };
        if let Err(e) = pool.event.read() {
            error!("failed to consume the fuse completion event: {e:?}");
        }
        while let Ok(completion) = pool.completions.try_recv() {
            pool.pending = pool.pending.saturating_sub(1);
            let queue_index = completion.queue_index;
            if let Err(e) =
                self.queues[queue_index].add_used(&self.mem, completion.index, completion.len)
            {
                error!("failed to add used elements to the queue: {e:?}");
            }
            if self.queues[queue_index]
                .needs_notification(&self.mem)
                .unwrap_or(true)
            {
                self.interrupt.signal_used_queue();
            }
        }
    }

    fn process_queue(&mut self, queue_index: usize) {
        while let Some((job, lone)) = self.next_job(queue_index) {
            // The pool is for concurrency, so it is used whenever there is any:
            // either the guest already has another chain available, or a request
            // is still in flight in the pool. Only a request that arrives on an
            // otherwise idle device is served on the dispatcher's own stack,
            // because a lone request pays for two scheduler wakeups (out to a
            // worker and back with the used-ring entry) and nothing hides them:
            // a single guest thread driving the queue that way measured ~20%
            // slower on the small-file classes with no gain at all.
            let to_pool = match self.pool.as_ref() {
                Some(pool) => !lone || pool.pending > 0,
                None => false,
            };
            match self.pool.as_mut() {
                Some(pool) if to_pool => pool.dispatch(job),
                _ => self.serve_inline(job),
            }
        }
    }

    /// Pop one descriptor chain off the available ring.
    ///
    /// The `Queue`'s own bookkeeping (`next_avail`) stays here on the dispatcher
    /// thread, which is why a pool thread is handed a head index and rebuilds
    /// the chain itself.
    ///
    /// The flag reports whether this chain was the *only* one the guest had made
    /// available, i.e. the request arrived alone. `Queue::len` counts the chains
    /// still to be popped *including* this one, so "one available" is the test,
    /// and it must be read before the pop: after it the ring is empty whether or
    /// not the guest had queued a burst. Only the dispatcher pops, so the read
    /// is not racy against anything but the guest (which can only add to it,
    /// which would turn a burst into a lone request at worst).
    fn next_job(&mut self, queue_index: usize) -> Option<(FsJob, bool)> {
        let queue = &mut self.queues[queue_index];
        let desc_table = queue.desc_table;
        let queue_size = queue.actual_size();
        let lone = queue.len(&self.mem) <= 1;
        let head = queue.pop(&self.mem)?;
        Some((
            FsJob {
                queue_index,
                index: head.index,
                desc_table,
                queue_size,
            },
            lone,
        ))
    }

    /// The no-pool path: serve the request on the dispatcher thread, exactly as
    /// this worker did before the pool existed.
    fn serve_inline(&mut self, job: FsJob) {
        let len = serve_chain(
            &self.mem,
            &self.server,
            DescriptorChain::checked_new(&self.mem, job.desc_table, job.queue_size, job.index),
            self.allow_idmap,
            &self.shm_region,
            &self.exit_code,
            #[cfg(target_os = "macos")]
            &self.map_sender,
        );
        if let Err(e) = self.queues[job.queue_index].add_used(&self.mem, job.index, len) {
            error!("failed to add used elements to the queue: {e:?}");
        }
        if self.queues[job.queue_index]
            .needs_notification(&self.mem)
            .unwrap_or(true)
        {
            self.interrupt.signal_used_queue();
        }
    }
}
