// Copyright 2026 zeroqn contributors.
// SPDX-License-Identifier: Apache-2.0

//! Best-effort host-side profiling for downstream libkrun consumers.
//!
//! A caller opts in by handing `VmmBuilder::set_profile_path` a file path. The
//! rows are written incrementally because libkrun hands control to the guest
//! event loop without returning; every failure (bad path, unwritable file) is
//! ignored so a profile request can never change launch behavior.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct KrunProfiler {
    path: PathBuf,
}

impl KrunProfiler {
    /// Start a profiler writing to `path`, or `None` when no path was given.
    pub fn start(path: Option<PathBuf>) -> Option<Self> {
        let path = path?;
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = File::create(&path);
        Some(Self { path })
    }

    /// Time `f` and record one TSV row for it.
    pub fn measure<T>(&self, label: &'static str, f: impl FnOnce() -> T) -> T {
        let started_at = Instant::now();
        let result = f();
        self.record(label, started_at.elapsed().as_nanos());
        result
    }

    /// Record a zero-duration row marking a point in the launch sequence.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiler_records_tsv_rows_and_tolerates_a_missing_path() {
        assert!(KrunProfiler::start(None).is_none());

        let dir = std::env::temp_dir().join(format!("krun-profile-{}", std::process::id()));
        let path = dir.join("profile.tsv");
        let profiler = KrunProfiler::start(Some(path.clone())).unwrap();
        assert_eq!(profiler.measure("label_a", || 1 + 1), 2);
        profiler.record_marker("marker_b");

        let text = std::fs::read_to_string(&path).unwrap();
        let labels: Vec<&str> = text
            .lines()
            .map(|line| line.split('\t').next().unwrap())
            .collect();
        assert_eq!(labels, ["label_a", "marker_b"]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
