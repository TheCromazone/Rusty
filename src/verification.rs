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

/// Fresh private Python bytecode and Cargo artifact namespaces for each executor-owned check.
/// `-B` alone does not prevent Python from loading planted cached bytecode.
pub struct CheckCache(PathBuf);
impl CheckCache {
    pub fn new() -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        for _ in 0..100 {
            let path = std::env::temp_dir().join(format!(
                "rusty-check-cache-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        bail!("could not allocate a fresh verification cache")
    }
    pub fn command(&self, command: &str) -> String {
        let path = self.0.to_string_lossy().replace('\'', "'\"'\"'");
        format!("export PYTHONPYCACHEPREFIX='{path}/python'; export CARGO_TARGET_DIR='{path}/cargo';\n{command}")
    }
}
impl Drop for CheckCache {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

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

    /// `/goal --verify [--timeout SECONDS] COMMAND`; commands stay literal shell text.
    pub fn parse_interactive(value: &str) -> Result<Self> {
        let value = value.trim();
        if let Some(rest) = value.strip_prefix("--timeout ") {
            let (seconds, command) = rest
                .trim_start()
                .split_once(char::is_whitespace)
                .context("usage: /goal --verify --timeout SECONDS COMMAND")?;
            return Self::new(command.trim_start(), seconds.parse().context("invalid verification timeout")?);
        }
        Self::new(value, 120)
    }

    pub fn validate(&self) -> Result<()> {
        Self::new(&self.command, self.timeout_secs).map(|_| ())
    }

    pub fn pin_files(mut self, root: &Path, paths: &[PathBuf]) -> Result<Self> {
        for path in paths {
            let sha256 = pinned_file_hash(root, path)
                .with_context(|| format!("pinned verification file unavailable: {}", path.display()))?;
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
    EvidenceUnavailable,
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
    observation_error: Option<String>,
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
            observation_error: None,
        }
    }

    pub fn invalidate(&mut self, reason: String) {
        self.outcome = CheckOutcome::EvidenceUnavailable;
        self.observation_error = Some(reason);
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
            "fixed verification: {:?} (exit {:?}, {:.2}s)\n{}\n{}",
            self.outcome,
            self.exit_code,
            self.seconds,
            self.output,
            self.observation_error.as_deref().unwrap_or("")
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
    walk(&root, &mut context, &mut count, &mut bytes, MAX_ENTRIES)?;
    let mut env: Vec<_> = std::env::vars_os().collect();
    env.sort();
    for (key, value) in env {
        field(&mut context, key.as_encoded_bytes());
        field(&mut context, value.as_encoded_bytes());
    }
    Ok(hex(context.finish().as_ref()))
}

// Count entries while reading, before sorting or retaining an unbounded directory.
// The budget is shared across the entire traversal, including ignored entries.
fn entries(dir: &Path, count: &mut usize, limit: usize) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)? {
        if *count >= limit {
            bail!("verification input exceeds {limit} entries");
        }
        paths.push(entry?.path());
        *count += 1;
    }
    paths.sort();
    paths.reverse();
    Ok(paths)
}

fn walk(root: &Path, context: &mut Context, count: &mut usize, bytes: &mut u64, limit: usize) -> Result<()> {
    // Reverse-sorted pending paths preserve the original depth-first hash order
    // without retaining a Rust call frame for every nested directory.
    let mut pending = entries(root, count, limit)?;
    while let Some(path) = pending.pop() {
        // Reject symlinks even if named like a generated directory.
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            bail!("verification input contains a symlink: {}", path.display());
        }
        if GENERATED.iter().any(|name| path.file_name().is_some_and(|n| n == *name)) {
            continue;
        }
        field(context, path.strip_prefix(root)?.as_os_str().as_encoded_bytes());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            field(context, &metadata.permissions().mode().to_le_bytes());
        }
        if metadata.is_dir() {
            field(context, b"directory");
            pending.extend(entries(&path, count, limit)?);
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

    #[test]
    fn fresh_cache_rejects_planted_bytecode_and_is_removed() {
        use std::process::Command;
        let workspace = Workspace::new();
        fs::write(workspace.0.join("calc.py"), "def add(a, b): return a - b\n").unwrap();
        let planted = Command::new("python3")
            .current_dir(&workspace.0)
            .args([
                "-c",
                r#"
import importlib.util, marshal, pathlib, struct, sys
sys.pycache_prefix = None
source = pathlib.Path('calc.py')
cache = pathlib.Path(importlib.util.cache_from_source(str(source)))
cache.parent.mkdir(exist_ok=True)
st = source.stat()
header = importlib.util.MAGIC_NUMBER + struct.pack('<III', 0, int(st.st_mtime), st.st_size)
cache.write_bytes(header + marshal.dumps(compile('def add(a, b): return a + b', 'calc.py', 'exec')))
"#,
            ])
            .status()
            .unwrap();
        assert!(planted.success());
        let check = "python3 -B -c 'from calc import add; assert add(2, 3) == 5'";
        assert!(Command::new("bash")
            .current_dir(&workspace.0)
            .args([
                "-c",
                "python3 -B -c 'import sys; sys.pycache_prefix = None; from calc import add; assert add(2, 3) == 5'"
            ])
            .env_remove("PYTHONPYCACHEPREFIX")
            .status()
            .unwrap()
            .success());
        let cache = CheckCache::new().unwrap();
        let path = cache.0.clone();
        assert!(!Command::new("bash")
            .current_dir(&workspace.0)
            .args(["-c", &cache.command(check)])
            .status()
            .unwrap()
            .success());
        drop(cache);
        assert!(!path.exists());
    }

    #[test]
    fn completion_contract_fresh_cargo_target_rejects_restored_pinned_test() {
        use std::process::Command;
        let workspace = Workspace::new();
        fs::create_dir(workspace.0.join("tests")).unwrap();
        fs::create_dir(workspace.0.join("src")).unwrap();
        fs::write(workspace.0.join("Cargo.toml"), "[package]\nname='cache_probe'\nversion='0.1.0'\nedition='2021'\n")
            .unwrap();
        fs::write(workspace.0.join("src/lib.rs"), "").unwrap();
        let source = "#[test] fn acceptance() { assert!(false); }\n";
        fs::write(workspace.0.join("tests/acceptance.rs"), source).unwrap();
        let check = CheckSpec::new("cargo test --offline --test acceptance", 120)
            .unwrap()
            .pin_files(&workspace.0, &[PathBuf::from("tests/acceptance.rs")])
            .unwrap();
        // Build a passing replacement, then restore both original bytes and mtime.
        let control = Command::new("python3")
            .current_dir(&workspace.0)
            .env("CARGO_TARGET_DIR", workspace.0.join("target"))
            .args([
                "-c",
                r#"
import os, pathlib, subprocess
p = pathlib.Path('tests/acceptance.rs')
original, st = p.read_bytes(), p.stat()
p.write_text('#[test] fn acceptance() { assert!(true); }\n')
subprocess.run(['cargo','test','--offline','--test','acceptance'], check=True, timeout=60)
p.write_bytes(original)
os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns))
subprocess.run(['cargo','test','--offline','--test','acceptance'], check=True, timeout=60)
"#,
            ])
            .output()
            .unwrap();
        assert!(control.status.success(), "{}", String::from_utf8_lossy(&control.stderr));
        check.validate_files(&workspace.0).unwrap();
        let cache = CheckCache::new().unwrap();
        let path = cache.0.clone();
        let output = Command::new("bash")
            .current_dir(&workspace.0)
            .env("CARGO_TARGET_DIR", workspace.0.join("target"))
            .args(["-c", &cache.command(check.command())])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(101), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("acceptance ... FAILED"));
        check.validate_files(&workspace.0).unwrap();
        drop(cache);
        assert!(!path.exists());
    }

    #[test]
    fn interactive_timeout_is_explicit_and_bounded() {
        let check = CheckSpec::parse_interactive("--timeout 7 python3 check.py").unwrap();
        assert_eq!(check.timeout(), Duration::from_secs(7));
        assert_eq!(check.command(), "python3 check.py");
        assert_eq!(CheckSpec::parse_interactive("true").unwrap().timeout(), Duration::from_secs(120));
        for bad in ["--timeout 0 true", "--timeout 601 true", "--timeout nope true", "--timeout 5"] {
            assert!(CheckSpec::parse_interactive(bad).is_err(), "{bad}");
        }
    }

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
    fn completion_contract_enumeration_budget_is_global_and_fails_closed() {
        let w = Workspace::new();
        fs::create_dir(w.0.join("nested")).unwrap();
        fs::write(w.0.join("nested/a"), "a").unwrap();
        fs::write(w.0.join("nested/b"), "b").unwrap();
        let mut count = 0;
        let error = walk(&w.0, &mut Context::new(&SHA256), &mut count, &mut 0, 2).unwrap_err();
        assert!(error.to_string().contains("exceeds 2 entries"));
        assert_eq!(count, 2);
        assert!(walk(&w.0, &mut Context::new(&SHA256), &mut 0, &mut 0, 3).is_ok());
    }

    #[test]
    fn completion_contract_directory_capture_stops_at_budget() {
        let w = Workspace::new();
        for name in ["a", "b", "c"] {
            fs::write(w.0.join(name), "").unwrap();
        }
        let mut count = 0;
        assert!(entries(&w.0, &mut count, 2).is_err());
        assert_eq!(count, 2);
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
