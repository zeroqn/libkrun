#[cfg(target_os = "macos")]
use crossbeam_channel::Sender;
use std::cmp;
use std::io::Write;
use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use utils::eventfd::{EFD_NONBLOCK, EventFd};
#[cfg(target_os = "macos")]
use utils::worker_message::WorkerMessage;
use virtio_bindings::{virtio_config::VIRTIO_F_VERSION_1, virtio_ring::VIRTIO_RING_F_EVENT_IDX};
use vm_memory::{ByteValued, GuestMemoryMmap};

use super::super::{
    ActivateError, ActivateResult, DeviceQueue, DeviceState, FsError, QueueConfig, VirtioDevice,
    VirtioShmRegion,
};
use super::ExportTable;
use super::passthrough;
use super::virtual_entry::VirtualDirEntry;
use super::worker::{DEFAULT_WORKER_THREADS, FsWorker};
use super::{defs, defs::uapi};
use crate::virtio::InterruptTransport;
use crate::virtio::passthrough::PermissionSemantics;

#[derive(Copy, Clone)]
#[repr(C, packed)]
struct VirtioFsConfig {
    tag: [u8; 36],
    num_request_queues: u32,
}

impl Default for VirtioFsConfig {
    fn default() -> Self {
        VirtioFsConfig {
            tag: [0; 36],
            num_request_queues: 0,
        }
    }
}

unsafe impl ByteValued for VirtioFsConfig {}

pub struct Fs {
    avail_features: u64,
    acked_features: u64,
    device_state: DeviceState,
    config: VirtioFsConfig,
    allow_idmap: bool,
    shm_region: Option<VirtioShmRegion>,
    passthrough_cfg: Option<passthrough::Config>,
    read_only: bool,
    virtual_entries: Vec<VirtualDirEntry<'static>>,
    worker_thread: Option<JoinHandle<()>>,
    worker_stopfd: EventFd,
    /// How many threads serve the request queue. 1 keeps every request on the
    /// single worker thread, exactly as virtiofs did before this device had a
    /// pool.
    worker_threads: usize,
    /// Whether to report the served opcode mix when the device stops.
    opcode_stats: bool,
    exit_code: Arc<AtomicI32>,
    #[cfg(target_os = "macos")]
    map_sender: Option<Sender<WorkerMessage>>,
}

impl Fs {
    pub fn new(
        fs_id: String,
        semantics: PermissionSemantics,
        shared_dir: Option<String>,
        exit_code: Arc<AtomicI32>,
        read_only: bool,
        virtual_entries: Vec<VirtualDirEntry<'static>>,
    ) -> super::Result<Fs> {
        let avail_features = (1u64 << VIRTIO_F_VERSION_1) | (1u64 << VIRTIO_RING_F_EVENT_IDX);

        let tag = fs_id.into_bytes();
        let mut config = VirtioFsConfig::default();
        config.tag[..tag.len()].copy_from_slice(tag.as_slice());
        config.num_request_queues = 1;

        let attr_timeout = if matches!(semantics, PermissionSemantics::LinuxSimplified) {
            // As uid/gid are context-dependent, attributes can't be cached.
            Duration::from_secs(0)
        } else {
            // The value defined as default in virtio-fs.
            Duration::from_secs(5)
        };
        // The value defined as default in virtio-fs.
        let entry_timeout = Duration::from_secs(5);

        let fs_cfg = shared_dir.map(|root_dir| passthrough::Config {
            root_dir,
            semantics,
            attr_timeout,
            entry_timeout,
            ..Default::default()
        });

        let allow_idmap = matches!(semantics, PermissionSemantics::LinuxComplete);

        Ok(Fs {
            avail_features,
            acked_features: 0,
            device_state: DeviceState::Inactive,
            config,
            allow_idmap,
            shm_region: None,
            passthrough_cfg: fs_cfg,
            read_only,
            virtual_entries,
            worker_thread: None,
            worker_stopfd: EventFd::new(EFD_NONBLOCK).map_err(FsError::EventFd)?,
            worker_threads: DEFAULT_WORKER_THREADS,
            opcode_stats: false,
            exit_code,
            #[cfg(target_os = "macos")]
            map_sender: None,
        })
    }

    pub fn id(&self) -> &str {
        defs::FS_DEV_ID
    }

    pub fn set_shm_region(&mut self, shm_region: VirtioShmRegion) {
        self.shm_region = Some(shm_region);
    }

    pub fn set_export_table(&mut self, export_table: ExportTable) -> u64 {
        static FS_UNIQUE_ID: AtomicU64 = AtomicU64::new(0);

        let Some(cfg) = self.passthrough_cfg.as_mut() else {
            // NullFs-backed devices have no passthrough config and don't
            // participate in cross-domain fd export. Consume (and waste) an
            // fsid so numbering stays dense, but don't store the table.
            return FS_UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
        };
        cfg.export_fsid = FS_UNIQUE_ID.fetch_add(1, Ordering::Relaxed);
        cfg.export_table = Some(export_table);

        cfg.export_fsid
    }

    pub fn add_virtual_entry(&mut self, entry: VirtualDirEntry<'static>) {
        self.virtual_entries.push(entry);
    }

    pub fn set_exit_code(&mut self, exit_code: Arc<AtomicI32>) {
        self.exit_code = exit_code;
    }

    /// Set how many threads serve the virtio-fs request queue.
    ///
    /// virtio-fs has no multiqueue feature, so this is a pool behind the single
    /// request queue (as upstream virtiofsd's `process_queue_pool` is): the
    /// device thread keeps reading the available ring and publishing used-ring
    /// entries, the pool threads do the FUSE work and write the replies. It is
    /// worth it only for the class where one server thread saturates - a
    /// directory scan (`READDIRPLUS`), where each entry costs an
    /// `openat(O_PATH)+statx` on the host - and costs nothing measurable for
    /// the small-file classes.
    ///
    /// `1` disables the pool and serves every request on the device thread; the
    /// value is also clamped to the host's available parallelism. The default is
    /// `DEFAULT_WORKER_THREADS`.
    pub fn set_worker_threads(&mut self, threads: usize) {
        self.worker_threads = threads.max(1);
    }

    /// Serve `FUSE_OPEN`/`FUSE_OPENDIR` with `-ENOSYS`, acknowledging
    /// `FUSE_NO_OPEN_SUPPORT`/`FUSE_NO_OPENDIR_SUPPORT`.
    ///
    /// The client then stops sending those requests and completes the matching
    /// releases itself (`fs/fuse/file.c:110-113`, `:139`, `:170-178`), which
    /// removes two round trips from every file open and every directory open -
    /// the largest request-removing lever the guest side has. What the handle
    /// used to provide is served from a descriptor the server opens and caches
    /// per *inode* on first use (`PassthroughFs::handle_of`), so the descriptor
    /// lifetime moves from the open to the inode.
    ///
    /// Two consequences the device keeps in step with: `FUSE_ATOMIC_O_TRUNC`
    /// stops being negotiated (truncate-on-open has no `FUSE_OPEN` to ride, so
    /// the VFS truncates through `SETATTR` instead), and `CREATE` answers
    /// without a handle (the kernel releases such a handle locally too, so one
    /// returned here would never be released).
    pub fn set_zero_message_open(&mut self, enabled: bool) {
        if let Some(cfg) = self.passthrough_cfg.as_mut() {
            cfg.zero_message_open = enabled;
        }
    }

    /// Answer `FUSE_FLUSH` with `-ENOSYS`, i.e. tell the client the server has no
    /// close-time work so it stops sending the request.
    ///
    /// Worth one round trip per close - 21% of the requests a cold `cargo build`
    /// issues - because the server's flush body is a `dup()`+`close()` of the
    /// passthrough descriptor and nothing else. What is given up is the close's
    /// ability to report a *deferred* host error; what is not given up is any
    /// lock state, because this server negotiates neither `POSIX_LOCKS` nor
    /// `FLOCK_LOCKS`, so the client's locks live in its own kernels and
    /// `filp_flush` releases them regardless of what `->flush` does.
    ///
    /// Preferred over the client-side `FOPEN_NOFLUSH` flag, which can only ride
    /// an `OPEN` reply - and this device answers `OPEN` with `-ENOSYS` when
    /// zero-message opens are on - and which additionally skips the client's own
    /// close-time writeback (`fs/fuse/file.c:479-480` returns before it).
    pub fn set_no_flush(&mut self, enabled: bool) {
        if let Some(cfg) = self.passthrough_cfg.as_mut() {
            cfg.no_flush = enabled;
        }
    }

    /// Answer a lookup of a missing name with a cacheable negative entry
    /// (`nodeid == 0` plus `entry_valid`) instead of `-ENOENT`, and let the
    /// kernel cache it for `timeout`. `None` keeps the `-ENOENT` reply.
    ///
    /// `-ENOENT` is not cacheable, so every repeated probe of the same missing
    /// name costs a round trip; a build spends most of its lookups that way (91%
    /// of them in a cold `cargo build`, where caching them removed 72% of the
    /// requests and 21% of the wall time). The price is exactly the timeout: a
    /// file the *host* creates stays invisible to the guest for up to that long.
    /// A quarter second keeps 92% of the saving, because a compiler re-probes the
    /// same name within milliseconds.
    pub fn set_negative_entry(&mut self, timeout: Option<Duration>) {
        if let Some(cfg) = self.passthrough_cfg.as_mut() {
            cfg.negative_entry = timeout;
        }
    }

    /// Report the opcode mix served by this device when it stops.
    ///
    /// One relaxed increment per request on the serving path and one block of
    /// log lines at teardown; the guest kernel this device serves cannot trace
    /// fuse requests, so this is the only way to learn which opcodes a workload
    /// actually issues.
    pub fn set_opcode_stats(&mut self, enabled: bool) {
        self.opcode_stats = enabled;
    }

    #[cfg(target_os = "macos")]
    pub fn set_map_sender(&mut self, map_sender: Sender<WorkerMessage>) {
        self.map_sender = Some(map_sender);
    }
}

impl VirtioDevice for Fs {
    fn avail_features(&self) -> u64 {
        self.avail_features
    }

    fn acked_features(&self) -> u64 {
        self.acked_features
    }

    fn set_acked_features(&mut self, acked_features: u64) {
        self.acked_features = acked_features
    }

    fn device_type(&self) -> u32 {
        uapi::VIRTIO_ID_FS
    }

    fn device_name(&self) -> &str {
        "fs"
    }

    fn queue_config(&self) -> &[QueueConfig] {
        &defs::QUEUE_CONFIG
    }

    fn config_len(&self) -> Option<u32> {
        Some(size_of::<VirtioFsConfig>() as u32)
    }

    fn read_config(&self, offset: u64, mut data: &mut [u8]) {
        let config_slice = self.config.as_slice();
        let config_len = config_slice.len() as u64;
        if offset >= config_len {
            error!("Failed to read config space");
            return;
        }
        if let Some(end) = offset.checked_add(data.len() as u64) {
            // This write can't fail, offset and end are checked against config_len.
            data.write_all(&config_slice[offset as usize..cmp::min(end, config_len) as usize])
                .unwrap();
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        warn!(
            "fs: guest driver attempted to write device config (offset={:x}, len={:x})",
            offset,
            data.len()
        );
    }

    fn activate(
        &mut self,
        mem: GuestMemoryMmap,
        interrupt: InterruptTransport,
        queues: Vec<DeviceQueue>,
    ) -> ActivateResult {
        if self.worker_thread.is_some() {
            panic!("virtio_fs: worker thread already exists");
        }

        // Extract queues and eventfds from DeviceQueues.
        let mut worker_queues = Vec::with_capacity(queues.len());
        let mut queue_evts = Vec::with_capacity(queues.len());
        for dq in queues {
            worker_queues.push(dq.queue);
            queue_evts.push(dq.event);
        }

        let virtual_entries = self.virtual_entries.clone();
        let worker = FsWorker::new(
            worker_queues,
            queue_evts,
            interrupt.clone(),
            mem.clone(),
            self.allow_idmap,
            self.shm_region.clone(),
            self.passthrough_cfg.clone(),
            self.read_only,
            virtual_entries,
            self.worker_stopfd.try_clone().unwrap(),
            self.exit_code.clone(),
            self.worker_threads,
            self.opcode_stats,
            #[cfg(target_os = "macos")]
            self.map_sender.clone(),
        )
        .map_err(|e| {
            error!("virtio_fs: failed to create worker: {}", e);
            ActivateError::BadActivate
        })?;
        self.worker_thread = Some(worker.run());

        self.device_state = DeviceState::Activated(mem, interrupt);
        Ok(())
    }

    fn is_activated(&self) -> bool {
        self.device_state.is_activated()
    }

    fn shm_region(&self) -> Option<&VirtioShmRegion> {
        self.shm_region.as_ref()
    }

    fn reset(&mut self) -> bool {
        if let Some(worker) = self.worker_thread.take() {
            let _ = self.worker_stopfd.write(1);
            if let Err(e) = worker.join() {
                error!("error waiting for worker thread: {e:?}");
            }
        }
        self.device_state = DeviceState::Inactive;
        true
    }
}
