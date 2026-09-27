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
no longer exist upstream:

- Opt-in launch profiling for cang diagnosis: TSV records
  `<label>\t<duration_nanos>`, best-effort and disabled unless a caller supplies a
  profile path. Attributes time spent inside VMM construction before libkrun hands
  control to the guest event loop, and allows appending kernel logging flags for
  profiled launches only, leaving the default kernel command line unchanged.
- The render-server fd entry point behind cang's `--gpu=drm` and `--wayland`
  modes, reaching `RutabagaBuilder::set_server_descriptor`.
- The virtio-gpu device fixes that came with the above: fence retirement (polling
  the virgl eventfd and calling `event_poll`), blob-map overflow, DRM render-node
  gating, and the idle poll.

The vaapi video work the branch used to carry (DRM render-node opened `O_RDWR`,
`get_drm_fd` renderer callback) is **not** re-added: crates.io `rutabaga_gfx`
0.1.85 already ships it (`src/virgl_renderer.rs` opens the render node
read+write with `O_CLOEXEC|O_NONBLOCK|O_NOCTTY` and registers `get_drm_fd` as the
renderer callback).

Keep profiling opt-in and best-effort. If libkrun rejects a profile path or cannot
write a row, normal VM startup behavior must remain unchanged.
