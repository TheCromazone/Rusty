//! A user-fixed acceptance criterion and fresh, executor-owned observations.
//! Records can be exported but cannot be deserialized into passing evidence.

use anyhow::{bail, Context as _, Result};
use ring::digest::{Context, SHA256};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const MAX_ENTRIES: usize = 50_000;
const MAX_BYTES: u64 = 128 * 1024 * 1024;
// This is a bounded source-input manifest, not a snapshot of dependencies,
// external services or arbitrary native processes. Document the exclusions.
const GENERATED: &[&str] = &[".git", "target", "__pycache__", ".pytest_cache"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckSpec {
    command: String,
    timeout_secs: u64,
    /// User-selected acceptance files, captured before the first model call.
    #[serde(default)]
    pinned_files: Vec<PinnedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PinnedFile {
    path: PathBuf,
    sha256: String,
}

impl CheckSpec {
    pub fn new(command: &str, timeout_secs: u64) -> Result<Self> {
        if command.trim().is_empty() || !(1..=600).contains(&timeout_secs) {
            bail!("verification needs a nonempty command and a deadline in 1..600 seconds");
        }
        if crate::infra::redact(command).1 != 0 {
            bail!("verification commands must use environment variables for credentials");
        }
        Ok(Self { command: command.into(), timeout_secs, pinned_files: Vec::new() })
    }

    pub fn validate(&self) -> Result<()> {
        Self::new(&self.command, self.timeout_secs).map(|_| ())
    }

    pub fn pin_files(mut self, root: &Path, paths: &[PathBuf]) -> Result<Self> {
        for path in paths {
            let sha256 = pinned_file_hash(root, path)?;
            self.pinned_files.push(PinnedFile { path: path.clone(), sha256 });
        }
        Ok(self)
    }

    pub fn validate_files(&self, root: &Path) -> Result<()> {
        for pin in &self.pinned_files {
            let current = pinned_file_hash(root, &pin.path)
                .with_context(|| format!("pinned verification file unavailable: {}", pin.path.display()))?;
            if current != pin.sha256 {
                bail!(
                    "pinned verification file changed: {}. Restore the original acceptance file before completion",
                    pin.path.display()
                );
            }
        }
        Ok(())
    }

    pub fn pinned_paths(&self) -> String {
        self.pinned_files.iter().map(|p| p.path.display().to_string()).collect::<Vec<_>>().join(", ")
    }

    fn digest(&self) -> String {
        let mut context = Context::new(&SHA256);
        field(&mut context, self.command.as_bytes());
        field(&mut context, &self.timeout_secs.to_le_bytes());
        for pin in &self.pinned_files {
            field(&mut context, pin.path.as_os_str().as_encoded_bytes());
            field(&mut context, pin.sha256.as_bytes());
        }
        hex(context.finish().as_ref())
    }

    pub fn command(&self) -> &str {
        &self.command
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }
}

#[derive(Debug, Serialize)]
pub enum CheckOutcome {
    Passed,
    Failed,
    TimedOut,
    Interrupted,
    WorkspaceChanged,
}

#[derive(Debug, Serialize)]
pub struct CheckRecord {
    command_sha256: String,
    criterion_sha256: String,
    input_sha256: String,
    final_sha256: String,
    exit_code: Option<i32>,
    outcome: CheckOutcome,
    seconds: f64,
    output_sha256: String,
    /// Redacted and bounded by the caller; exported for diagnosis, not proof.
    output: String,
}

impl CheckRecord {
    pub fn observed(
        spec: &CheckSpec,
        input: String,
        final_input: String,
        status: Option<Option<i32>>,
        interrupted: bool,
        seconds: f64,
        output: String,
    ) -> Self {
        let outcome = if interrupted {
            CheckOutcome::Interrupted
        } else if status.is_none() {
            CheckOutcome::TimedOut
        } else if input != final_input {
            CheckOutcome::WorkspaceChanged
        } else if status == Some(Some(0)) {
            CheckOutcome::Passed
        } else {
            CheckOutcome::Failed
        };
        Self {
            command_sha256: hash(spec.command.as_bytes()),
            criterion_sha256: spec.digest(),
            input_sha256: input,
            final_sha256: final_input,
            exit_code: status.flatten(),
            outcome,
            seconds,
            output_sha256: hash(output.as_bytes()),
            output,
        }
    }

    pub fn passed(&self) -> bool {
        matches!(self.outcome, CheckOutcome::Passed)
    }

    /// Some(pass) when the check ran to a verdict; None when it timed out,
    /// was interrupted or saw the workspace change underneath it.
    pub fn verdict(&self) -> Option<bool> {
        match self.outcome {
            CheckOutcome::Passed => Some(true),
            CheckOutcome::Failed => Some(false),
            _ => None,
        }
    }

    pub fn summary(&self) -> String {
        format!(
            "fixed verification: {:?} (exit {:?}, {:.2}s)\n{}",
            self.outcome, self.exit_code, self.seconds, self.output
        )
    }
}

/// Explicit files only: no path traversal, symlink ancestors or directory walks.
/// Include the workspace identity and mode so saved pins cannot silently move.
fn pinned_file_hash(root: &Path, relative: &Path) -> Result<String> {
    use std::path::Component;
    if relative.as_os_str().is_empty() || relative.components().any(|c| !matches!(c, Component::Normal(_))) {
        bail!("pinned verification files must be workspace-relative file paths: {}", relative.display());
    }
    let root = root.canonicalize()?;
    let mut path = root.clone();
    for part in relative.components() {
        path.push(part);
        if fs::symlink_metadata(&path)?.file_type().is_symlink() {
            bail!("pinned verification file contains a symlink: {}", relative.display());
        }
    }
    let metadata = fs::metadata(&path)?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        bail!("pinned verification input must be a regular file under 128 MiB: {}", relative.display());
    }
    let mut context = Context::new(&SHA256);
    field(&mut context, root.as_os_str().as_encoded_bytes());
    field(&mut context, relative.as_os_str().as_encoded_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        field(&mut context, &metadata.permissions().mode().to_le_bytes());
    }
    let mut file = fs::File::open(path)?;
    let mut buffer = [0; 16 * 1024];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > MAX_BYTES {
            bail!("pinned verification file exceeds 128 MiB");
        }
        context.update(&buffer[..n]);
    }
    Ok(hex(context.finish().as_ref()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hash(bytes: &[u8]) -> String {
    hex(ring::digest::digest(&SHA256, bytes).as_ref())
}

fn field(context: &mut Context, bytes: &[u8]) {
    context.update(&(bytes.len() as u64).to_le_bytes());
    context.update(bytes);
}

/// Canonical workspace, source contents, executable modes and inherited
/// environment. Secrets enter the digest only; never the exported manifest.
/// Enumeration/read failures and symlinks fail closed rather than omit input.
pub fn fingerprint(root: &Path) -> Result<String> {
    let root = root.canonicalize().context("cannot identify verification workspace")?;
    let mut context = Context::new(&SHA256);
    field(&mut context, root.as_os_str().as_encoded_bytes());
    let mut count = 0;
    let mut bytes = 0;
    walk(&root, &root, &mut context, &mut count, &mut bytes)?;
    let mut env: Vec<_> = std::env::vars_os().collect();
    env.sort();
    for (key, value) in env {
        field(&mut context, key.as_encoded_bytes());
        field(&mut context, value.as_encoded_bytes());
    }
    Ok(hex(context.finish().as_ref()))
}

fn walk(root: &Path, dir: &Path, context: &mut Context, count: &mut usize, bytes: &mut u64) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<std::io::Result<_>>()?;
    entries.sort();
    for path in entries {
        // Reject symlinks even if named like a generated directory.
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!("verification input contains a symlink: {}", path.display());
        }
        if GENERATED.iter().any(|name| path.file_name().is_some_and(|n| n == *name)) {
            continue;
        }
        *count += 1;
        if *count > MAX_ENTRIES {
            bail!("verification input exceeds {MAX_ENTRIES} entries");
        }
        field(context, path.strip_prefix(root)?.as_os_str().as_encoded_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            field(context, &metadata.permissions().mode().to_le_bytes());
        }
        if metadata.is_dir() {
            field(context, b"directory");
            walk(root, &path, context, count, bytes)?;
        } else if metadata.is_file() {
            field(context, b"file");
            let mut file = fs::File::open(&path)?;
            let mut content_hash = Context::new(&SHA256);
            let mut buffer = [0; 16 * 1024];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                *bytes += n as u64;
                if *bytes > MAX_BYTES {
                    bail!("verification input exceeds 128 MiB");
                }
                content_hash.update(&buffer[..n]);
            }
            field(context, content_hash.finish().as_ref());
        } else {
            bail!("unsupported verification input: {}", path.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct Workspace(PathBuf);
    impl Workspace {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "rusty-verification-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn input_changes_and_untracked_files_invalidate_fingerprint() {
        let w = Workspace::new();
        fs::write(w.0.join("a.py"), "one").unwrap();
        let a = fingerprint(&w.0).unwrap();
        fs::write(w.0.join("a.py"), "two").unwrap();
        let b = fingerprint(&w.0).unwrap();
        assert_ne!(a, b);
        fs::write(w.0.join("Cargo.lock"), "new dependency").unwrap();
        assert_ne!(b, fingerprint(&w.0).unwrap());
        fs::remove_file(w.0.join("Cargo.lock")).unwrap();
        assert_eq!(b, fingerprint(&w.0).unwrap());
    }

    #[test]
    fn generated_artifacts_do_not_make_checks_stale() {
        let w = Workspace::new();
        let a = fingerprint(&w.0).unwrap();
        fs::create_dir(w.0.join("__pycache__")).unwrap();
        fs::write(w.0.join("__pycache__/a.pyc"), "bytecode").unwrap();
        assert_eq!(a, fingerprint(&w.0).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn symlinks_cannot_hide_input_or_escape_the_manifest() {
        let w = Workspace::new();
        std::os::unix::fs::symlink("/tmp", w.0.join("target")).unwrap();
        assert!(fingerprint(&w.0).is_err());
    }

    #[test]
    fn only_current_completed_success_passes() {
        let spec = CheckSpec::new("python3 tests.py", 1).unwrap();
        for (status, interrupted, after, expected) in [
            (Some(Some(0)), false, "before", true),
            (Some(Some(1)), false, "before", false),
            (Some(None), false, "before", false),
            (None, false, "before", false),
            (Some(Some(0)), true, "before", false),
            (Some(Some(0)), false, "changed", false),
        ] {
            let r = CheckRecord::observed(
                &spec,
                "before".into(),
                after.into(),
                status,
                interrupted,
                0.1,
                "fake passing output".into(),
            );
            assert_eq!(r.passed(), expected, "{}", r.summary());
        }
    }

    #[test]
    fn completion_contract_pins_survive_serialization_without_rebaselining() {
        let w = Workspace::new();
        fs::write(w.0.join("check.py"), "assert False").unwrap();
        let spec =
            CheckSpec::new("python3 check.py", 1).unwrap().pin_files(&w.0, &[PathBuf::from("check.py")]).unwrap();
        let loaded: CheckSpec = serde_json::from_str(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert!(loaded.validate_files(&w.0).is_ok());
        assert_eq!(spec.digest(), loaded.digest());
        fs::write(w.0.join("check.py"), "pass").unwrap();
        assert!(loaded.validate_files(&w.0).is_err());
        fs::write(w.0.join("check.py"), "assert False").unwrap();
        assert!(loaded.validate_files(&w.0).is_ok());
        let other = Workspace::new();
        fs::write(other.0.join("check.py"), "assert False").unwrap();
        assert!(loaded.validate_files(&other.0).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn completion_contract_pins_reject_escapes_symlinks_and_mode_changes() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let w = Workspace::new();
        fs::write(w.0.join("check.py"), "pass").unwrap();
        let spec = CheckSpec::new("python3 check.py", 1).unwrap();
        for path in ["", "..", "../check.py", "/tmp/check.py", "missing"] {
            assert!(spec.clone().pin_files(&w.0, &[path.into()]).is_err(), "{path}");
        }
        symlink(&w.0, w.0.join("linked")).unwrap();
        assert!(spec.clone().pin_files(&w.0, &["linked/check.py".into()]).is_err());
        let pinned = spec.pin_files(&w.0, &["check.py".into()]).unwrap();
        let path = w.0.join("check.py");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        fs::set_permissions(path, fs::Permissions::from_mode(mode ^ 0o100)).unwrap();
        assert!(pinned.validate_files(&w.0).is_err());
    }

    #[test]
    fn loaded_specs_are_revalidated() {
        let spec: CheckSpec =
            serde_json::from_str(r#"{"command":"true","timeout_secs":18446744073709551615}"#).unwrap();
        assert!(spec.validate().is_err());
        assert!(CheckSpec::new("", 1).is_err());
    }
}
