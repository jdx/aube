//! Progress reporting hook. This crate is a library — it never prints.
//! The CLI wires an implementation backed by `clx::progress`; tests
//! and non-interactive callers use [`NoopProgress`].

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallPhase {
    Resolving,
    Downloading,
    Verifying,
    Extracting,
}

pub trait DownloadProgress: Send + Sync {
    fn on_phase(&self, _version: &node_semver::Version, _phase: InstallPhase) {}
    fn on_download_start(&self, _total_bytes: Option<u64>) {}
    fn on_download_chunk(&self, _bytes: u64) {}
    fn on_done(&self) {}
}

pub struct NoopProgress;

impl DownloadProgress for NoopProgress {}
