# loftd downstream libkrun branch

This branch carries an opt-in profiling patch for loftd launch diagnosis:

- C ABI: `krun_set_profile_path(ctx_id, const char *path)`
- File format: TSV records, `<label>\t<duration_nanos>`
- Behavior: best-effort and disabled unless a caller supplies a profile path
- Purpose: attribute time spent inside `krun_start_enter` / VMM construction before
  libkrun hands control to the guest event loop

Keep profiling opt-in and best-effort. If libkrun rejects a profile path or cannot
write a row, normal VM startup behavior should remain unchanged.
