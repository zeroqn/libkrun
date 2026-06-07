# loftd downstream libkrun branch

This branch carries an opt-in profiling patch for loftd launch diagnosis:

- C ABI: `krun_set_profile_path(ctx_id, const char *path)`
- C ABI: `krun_set_kernel_cmdline_append(ctx_id, const char *fragment)`
- File format: TSV records, `<label>\t<duration_nanos>`
- Behavior: best-effort and disabled unless a caller supplies a profile path
- Purpose: attribute time spent inside `krun_start_enter` / VMM construction before
  libkrun hands control to the guest event loop
- Profile diagnostics: loftd can append kernel logging flags only for profiled
  launches, leaving the default libkrun kernel command line unchanged otherwise

Keep profiling opt-in and best-effort. If libkrun rejects a profile path or cannot
write a row, normal VM startup behavior should remain unchanged.
