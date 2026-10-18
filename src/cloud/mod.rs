//! The Daytona launcher (`rusty-cloud`) and its localhost tool bridge.
//!
//! Everything talks to Daytona's REST API directly through the blocking
//! `reqwest` client: the control plane for snapshots and sandboxes, and each
//! sandbox's toolbox API for commands and files. The agent itself never links
//! this module; it reaches remote tools only through the bridge.

pub mod api;
pub mod bridge;
pub mod cli;
pub mod digest;
pub mod hybrid;
pub mod image;
pub mod launcher;
pub mod shell;
pub mod ssh;

use anyhow::Result;
use std::collections::BTreeMap;
use std::path::Path;

/// The few things the launcher and the bridge need from a sandbox.
pub trait Remote: Send + Sync {
    fn id(&self) -> &str;
    fn labels(&self) -> BTreeMap<String, String>;
    /// Runs `cmd` with bash and returns its exit code and combined output.
    fn sh(&self, cmd: &str, timeout_secs: u64) -> Result<(i32, String)>;
    /// Starts `cmd` in the background in a named session; returns its id.
    fn start(&self, cmd: &str, session: &str) -> Result<String>;
    /// Copies a local file to `path` (`$HOME` is expanded).
    fn upload(&self, local: &Path, path: &str) -> Result<()>;
    /// A file's bytes, or None if it could not be read.
    fn download(&self, path: &str) -> Option<Vec<u8>>;
}

/// Deletes sandboxes; the launcher only ever asks for its own.
pub trait Sandboxes {
    fn delete_sandbox(&self, id: &str) -> Result<()>;
}
