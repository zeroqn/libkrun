# cang downstream libkrun branch

Based on upstream `main` - the 2.0.0 / C-ABI-2 line - as of
`a1d8616765da03980f3be1bb8714bfdd3842f33c` (2026-10-16). The previous base was
`a980e7795b86c8151e867b32a43af4cb984364fa`, and before the ABI-2 switch,
upstream release `v1.19.5` (the `stable-1.19.x` line).

## Carried from upstream, still unmerged

Two open upstream PRs are cherry-picked on top of the base, each as its own
commit with a `(cherry picked from upstream PR #N ...)` trailer so it can be
dropped the day upstream merges it:

- PR 865 - virtio/blk parallel reads with an auto-sized, overridable read-pool
  (`krun_block_device_set_read_pool_size`). The PR leaves the feature opt-in;
  the fork turns it on by default (`parallel_reads = true` in
  `BlockDevice::new`), so a caller that never touches the setter still gets the
  thread pool. Because that default is a one-line fork commit, a caller that
  must not depend on it - cang's `crates/cang-libkrun` does - can still set it
  explicitly and survive the commit being dropped.
- PR 840 - guest-clock workarounds

Upstream PR 822 (virtio-gpu `CREATE_GUEST_HANDLE`, the zero-copy SHM fast path)
is deliberately **not** carried: the path is inert on the pinned kernel, the PR's
own commit renumbers `VIRTIO_GPU_F_CREATE_GUEST_HANDLE` from 6 to 5 while our
kernel uses bit 5 for `VIRTIO_GPU_F_FENCE_PASSING`, and its rutabaga half needs
crate support (guest-blob handles) that crates.io `rutabaga_gfx` 0.1.85 does not
have.

## Fork-only work

Re-derived on the ABI-2 builder/object API, because the v1 entry points they used
no longer exist upstream. The C names are regenerated from the Rust API by
`make gen-libkrun-bindings`, so they are `krun_<object>_<method>`:

- `krun_vmm_builder_set_profile_path(VmmBuilder*, KrunStr, KrunError*)` - opt-in
  launch profiling for cang diagnosis: TSV records `<label>\t<duration_nanos>`,
  best-effort and disabled unless a caller supplies a profile path. Attributes
  time spent inside VMM construction before libkrun hands control to the guest
  event loop. Kernel logging flags for profiled launches are no longer part of
  this ABI: cang appends them through the ABI-2
  `krun_payload_append_cmdline`, which keeps the default kernel command line
  unchanged for unprofiled launches.
- `krun_gpu_device_set_render_server_fd(GpuDevice*, int, KrunError*)` - the
  render-server fd entry point behind cang's `--gpu=drm` and `--wayland` modes.
  It validates the fd, owns it, and reaches
  `RutabagaBuilder::set_server_descriptor`, which is what virglrenderer's venus
  render-server path uses. A negative fd is rejected.
- The virtio-gpu device fixes that came with it: fence retirement (polling the
  virgl eventfd, calling `event_poll` on timeout while a fence is pending),
  poison-safe fence-handler locks, and the two-tier poll timeout that stops an
  idle VM from waking every 10ms.
- Host huge pages behind guest RAM: `create_guest_memory` calls
  `madvise(MADV_HUGEPAGE)` on every guest memory region before the payload and
  the VM are set up. Guest RAM is a plain anonymous (or memfd-backed) mapping and
  Linux defaults to `THP=madvise`, so without the advice every guest page gets a
  4 KiB host page - one host page fault per first guest touch and 4 KiB EPT
  entries. The advice is a hint: a `never` host policy, or a memfd region while
  shmem THP is off, ignores it, and it is never fatal. Measured in cang with a
  534-crate offline `cargo build` in the guest (three pairs, arm order
  alternated): 79.7 s -> 65.8 s wall clock (-17%) with the host process's
  `AnonHugePages` going 0 -> ~3.9 GiB at unchanged RSS. This one is plausibly
  upstreamable; it is fork-only because cang needs it now.
- `krun_fs_device_set_worker_threads(FsDevice*, size_t)` - the size of the
  pool that serves the virtio-fs request queue (1 = serve every request on the
  device thread). virtio-fs has no multiqueue feature, so a single `fs worker`
  thread serves the guest's whole root filesystem; one READDIRPLUS scan is
  enough to saturate it while the guest is at 9-22% of a core, which is what a
  pool behind the one queue fixes (see cang's
  `docs/wayfinder/workspace-virtiofs-performance/notes/21-worker-pool-results.md`).
  The device thread keeps the available and used rings; the pool threads run the
  FUSE op and write the reply, so completions land out of order - allowed, the
  FUSE client matches replies by `unique`. Default 4. The panic fix that keeps a
  bad request or a panicking handler from wedging the share applies to the pool,
  not just the single thread.

- `krun_fs_device_set_zero_message_open(FsDevice*, bool)` - acknowledge
  `FUSE_NO_OPEN_SUPPORT`/`FUSE_NO_OPENDIR_SUPPORT` by answering the first
  `FUSE_OPEN`/`FUSE_OPENDIR` with `-ENOSYS`. The guest kernel then stops sending
  them (`fs/fuse/file.c:139`, `:166-178`) and completes `FUSE_RELEASE` /
  `FUSE_RELEASEDIR` itself (`:110-113`), which removes two round trips from every
  file and directory open - the largest request-removing lever available on this
  side, since a FUSE round trip costs about the same per request whatever the
  opcode (cang's `docs/wayfinder/workspace-virtiofs-performance/notes/20` and
  `23`). Requests then name only the inode (`ff->fh` stays 0), so
  `PassthroughFs` opens and caches a descriptor per *inode* on first use
  (`handle_of`) instead of per open; `FUSE_ATOMIC_O_TRUNC` is dropped from the
  negotiated flags with it (no `FUSE_OPEN` to carry `O_TRUNC`, so the VFS
  truncates through `SETATTR`), and `FUSE_CREATE` answers without a handle (the
  kernel releases such a handle locally too, so one handed out would never be
  released). Default off.
- `krun_fs_device_set_no_flush(FsDevice*, bool)` - answer `FUSE_FLUSH` with
  `-ENOSYS`, the protocol's own "this server has no close-time work", so the
  client stops sending it (`fs/fuse/file.c:505-507` sets `no_flush` and treats
  it as success). The server's flush body is `dup()`+`close()` of the passthrough
  descriptor and nothing else, so the only thing lost is the close's ability to
  report a *deferred* host error; no lock state is lost either, because this
  server negotiates neither `POSIX_LOCKS` nor `FLOCK_LOCKS`, so the client's
  locks are local and `filp_flush` -> `locks_remove_posix` releases them whatever
  `->flush` does. Worth 21% of the requests and ~10% of the wall time of a cold
  2401-crate `cargo build` (cang's notes 33 and 35). Preferred over the
  client-side `FOPEN_NOFLUSH` flag, which can only ride an `OPEN` reply (and
  `OPEN` is answered `-ENOSYS` under zero-message opens) and which additionally
  skips the client's own close-time writeback. Default off.
- `krun_fs_device_set_negative_entry(FsDevice*, bool, uint64_t timeout_millis)` -
  answer a lookup of a missing name with the *cacheable* negative entry
  (`nodeid == 0` plus `entry_valid`) instead of `-ENOENT`, and let the client
  cache it for the given time. `-ENOENT` is never cacheable (the kernel drops the
  dentry immediately, `fs/fuse/dir.c`), so every repeated probe of a missing name
  costs a round trip; 91% of the lookups in a cold `cargo build` are such misses.
  Caching them removed 72% of the requests and 21% of the wall time (cang's
  notes 34-35). The price is the timeout itself: a file the *host* creates stays
  invisible to the client for up to that long, which is why cang asks for 250 ms
  - a quarter second keeps 92% of the saving, because a compiler re-probes the
  same name within milliseconds, and the measured invisibility was 74 ms.
  `enabled = false` keeps `-ENOENT`. Default off.
- `krun_fs_device_set_opcode_stats(FsDevice*, bool)` - count served requests per
  FUSE opcode and log the histogram when the device stops. The pinned guest
  kernel has neither ftrace nor debugfs, so the server is the only place a
  workload's opcode mix can be measured; it is what every request-removing lever
  has to be priced against. Default off, one relaxed atomic increment when on.

The vaapi video work the branch used to carry (DRM render-node opened `O_RDWR`,
`get_drm_fd` renderer callback) is **not** re-added: crates.io `rutabaga_gfx`
0.1.85 already ships it (`src/virgl_renderer.rs` opens the render node
read+write with `O_CLOEXEC|O_NONBLOCK|O_NOCTTY` and registers `get_drm_fd` as the
renderer callback). Neither is the v1 `krun_set_kernel_cmdline_append`: ABI 2
exposes the same capability as `krun_payload_append_cmdline`.

Keep profiling opt-in and best-effort. If libkrun rejects a profile path or cannot
write a row, normal VM startup behavior must remain unchanged.

## Building this branch

ABI 2 gates the whole C surface behind the `ffi` cargo feature, and the Makefile
only enables it for `FFI=1`. Build with:

```
make BLK=1 NET=1 GPU=1 INPUT=1 TIMESYNC=1 FFI=1
```

`make install` then produces `libkrun.so.2*` (the VMM + the fork's extensions)
and `libkrun_init.so.0*` (the guest init blob plus its config builder). A build
without `FFI=1` still succeeds and ships a `libkrun.so.2` that exports no
`krun_*` symbol at all, which is why `publish-cang-release.yml` now builds with
`FFI=1` and asserts the exported symbols before packaging.
