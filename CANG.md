# cang downstream libkrun branch

Based on upstream `main` - the 2.0.0 / C-ABI-2 line - as of
`a980e7795b86c8151e867b32a43af4cb984364fa` (2026-09-27). The previous base was
upstream release `v1.19.5` (the `stable-1.19.x` line).

## Carried from upstream, still unmerged

Two open upstream PRs are cherry-picked on top of the base, each as its own
commit with a `(cherry picked from upstream PR #N ...)` trailer so it can be
dropped the day upstream merges it:

- PR 865 - virtio/blk opt-in parallel reads
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
