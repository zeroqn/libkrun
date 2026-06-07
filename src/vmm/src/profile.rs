// Copyright 2026 zeroqn contributors.
// SPDX-License-Identifier: Apache-2.0

//! Best-effort host-side profiling for downstream libkrun consumers.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct KrunProfiler {
    path: PathBuf,
}

impl KrunProfiler {
    pub fn start(path: Option<PathBuf>) -> Option<Self> {
        let path = path?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = File::create(&path);
        Some(Self { path })
    }

    pub fn measure<T>(&self, label: &'static str, f: impl FnOnce() -> T) -> T {
        let started_at = Instant::now();
        let result = f();
        self.record(label, started_at.elapsed().as_nanos());
        result
    }

    pub fn record_marker(&self, label: &'static str) {
        self.record(label, 0);
    }

    fn record(&self, label: &'static str, nanos: u128) {
        let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        else {
            return;
        };
        let _ = writeln!(file, "{label}\t{nanos}");
        let _ = file.flush();
    }
}
