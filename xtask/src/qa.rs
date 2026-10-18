//! One gate locally and in CI. Extensions spend nothing unless requested.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

#[derive(Default)]
struct Options {
    live: bool,
    perf: bool,
    cloud: bool,
    list: bool,
    changed: bool,
    full: bool,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Self::default();
        for a in args {
            match a.as_str() {
                "--live" => o.live = true,
                "--full" => o.full = true,
                "--changed" => o.changed = true,
                "--perf" => o.perf = true,
                "--cloud" => o.cloud = true,
                "--list" => o.list = true,
                _ => bail!("unknown qa argument {a:?}; use --full, --changed, --live, --perf, --cloud or --list"),
            }
        }
        ensure!(!(o.full && o.changed), "choose --full or --changed");
        ensure!(!(o.changed && (o.live || o.perf || o.cloud)), "extensions require the full gate");
        Ok(o)
    }
    fn preflight(&self) -> Result<()> {
        if self.live || self.cloud {
            required("NVIDIA_API_KEY")?;
        }
        if self.live {
            let filter = std::env::var("EVAL_TASKS").unwrap_or_default();
            ensure!(
                !super::eval::tasks(&super::root().join("evals"), &filter)?.is_empty(),
                "no evals match EVAL_TASKS"
            );
            if let Ok(value) = std::env::var("EVAL_REPEATS") {
                ensure!(value.parse::<u64>().is_ok_and(|n| n > 0), "EVAL_REPEATS must be positive");
            }
        }
        if self.cloud {
            for name in ["DAYTONA_API_KEY", "RUSTY_CLOUD_SNAPSHOT", "RUSTY_CLOUD_REF"] {
                required(name)?;
            }
            ensure!(
                std::env::var("RUSTY_CLOUD_REF")? == git(&["rev-parse", "HEAD"]).unwrap_or_default(),
                "RUSTY_CLOUD_REF must be this checkout's full commit SHA"
            );
        }
        Ok(())
    }
}
fn required(name: &str) -> Result<()> {
    ensure!(std::env::var(name).is_ok_and(|v| !v.trim().is_empty()), "qa extension requires {name}");
    Ok(())
}
struct Step {
    name: &'static str,
    program: String,
    args: Vec<String>,
}
impl Step {
    fn cargo(name: &'static str, args: &[&str]) -> Self {
        Self { name, program: "cargo".into(), args: args.iter().map(|a| (*a).into()).collect() }
    }
    fn task(name: &'static str, args: Vec<String>) -> Self {
        Self { name, program: std::env::current_exe().unwrap().display().to_string(), args }
    }
    fn run(&self, test_scale: u64) -> Result<()> {
        let mut command = Command::new(&self.program);
        if self.name == "evals" {
            command.env("EVAL_BINARY", super::target_dir().join("release/rusty"));
        }
        if matches!(self.name, "tests" | "memory" | "terminal") {
            command.env("RUSTY_TEST_TIME_SCALE", test_scale.to_string());
        }
        let status = command
            .args(&self.args)
            .current_dir(super::root())
            .status()
            .with_context(|| format!("starting {}", self.name))?;
        ensure!(status.success(), "{} failed ({status})", self.name);
        Ok(())
    }
}
fn plan(o: &Options, out: &Path) -> Vec<Step> {
    let debug = super::target_dir().join("debug/rusty").display().to_string();
    let release = super::target_dir().join("release/rusty").display().to_string();
    let mut steps = vec![
        Step::cargo("format", &["fmt", "--all", "--check"]),
        Step::cargo("lint", &["clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"]),
        Step::task(
            "tests",
            if o.changed { vec!["test-suite".into(), "--changed".into()] } else { vec!["test-suite".into()] },
        ),
        Step::cargo("binaries", &["build", "--bins", "--locked"]),
        Step::task(
            "memory",
            vec![
                "memory-smoke".into(),
                "--binary".into(),
                debug.clone(),
                "--report".into(),
                out.join("memory.json").display().to_string(),
            ],
        ),
        Step::task(
            "terminal",
            vec![
                "tty-capture".into(),
                "--all".into(),
                "--binary".into(),
                debug,
                "--output".into(),
                out.join("terminal").display().to_string(),
            ],
        ),
        Step::task(
            "terminal-inspection",
            vec!["tty-inspect".into(), out.join("terminal").display().to_string(), "--all".into()],
        ),
    ];
    if o.changed {
        steps.retain(|s| matches!(s.name, "format" | "lint" | "tests"));
    }
    if o.live || o.perf || o.cloud {
        steps.push(Step::cargo("release", &["build", "--release", "--bins", "--locked"]));
    }
    if o.perf {
        steps.push(Step::task(
            "performance",
            vec![
                "bench".into(),
                "--check".into(),
                "--binary".into(),
                release.clone(),
                "--json".into(),
                out.join("bench.json").display().to_string(),
            ],
        ));
    }
    if o.live {
        steps.push(Step::cargo(
            "live-cli",
            &["test", "--locked", "--test", "cli", "--", "--ignored", "--test-threads", "1"],
        ));
        steps.push(Step::task(
            "live-memory",
            vec![
                "memory-smoke".into(),
                "--live".into(),
                "--binary".into(),
                release.clone(),
                "--report".into(),
                out.join("memory-live.json").display().to_string(),
            ],
        ));
        steps.push(Step::task(
            "live-terminal",
            vec!["tty".into(), "ctrl-c".into(), "--live".into(), "--binary".into(), release],
        ));
        steps.push(Step::task("evals", vec!["eval".into(), std::env::var("EVAL_TASKS").unwrap_or_default()]));
    }
    if o.cloud {
        steps.push(Step::cargo(
            "hosted-cloud",
            &[
                "test",
                "--locked",
                "--test",
                "cloud",
                "--",
                "--ignored",
                "--exact",
                "live_swarm_journey",
                "--test-threads",
                "1",
                "--nocapture",
            ],
        ));
    }
    steps
}
fn save(out: &Path, report: &Value) -> Result<()> {
    std::fs::write(out.join("summary.json"), serde_json::to_string_pretty(report)? + "\n")?;
    let mut md = format!(
        "## Rusty quality\n\nSource: `{}` · {}\n\n| Gate | Result | Seconds |\n|---|---|---:|\n",
        report["revision"].as_str().unwrap_or("unknown"),
        report["status"].as_str().unwrap()
    );
    for s in report["steps"].as_array().unwrap() {
        md.push_str(&format!(
            "| {} | {} | {:.1} |\n",
            s["name"].as_str().unwrap(),
            s["status"].as_str().unwrap(),
            s["seconds"].as_f64().unwrap()
        ));
    }
    md.push_str(&format!("\nSource unchanged at recorded boundaries: {}.\n", report["source_unchanged"]));
    md.push_str(&format!("\nScope: {}. A changed-only run is not full qualification.\n", report["scope"]));
    md.push_str(&format!("\nExtensions requested: live={}, performance={}, hosted cloud={}.\nUnrequested or unrun gates are not qualification evidence.\n", report["live"], report["perf"], report["cloud"]));
    std::fs::write(out.join("summary.md"), md)?;
    Ok(())
}
/// Save before starting and after every step. A failed or never-run gate
/// cannot disappear from the receipt or be mistaken for success.
fn execute(
    steps: &[Step],
    out: &Path,
    report: &mut Value,
    mut run: impl FnMut(&Step) -> Result<()>,
    mut identity: impl FnMut() -> Result<Value>,
) -> Result<()> {
    save(out, report)?;
    for (i, step) in steps.iter().enumerate() {
        println!("\n▸ {}", step.name);
        report["steps"][i]["status"] = json!("running");
        save(out, report)?;
        let start = Instant::now();
        let result = run(step);
        let observed = identity();
        report["source_end"] = observed.as_ref().ok().cloned().unwrap_or(Value::Null);
        report["source_unchanged"] = json!(observed.as_ref().is_ok_and(|s| *s == report["source_start"]));
        let source_check = observed.and_then(|source| {
            ensure!(source == report["source_start"], "source changed during {}; qualification stopped", step.name);
            Ok(())
        });
        let result = result.and(source_check);
        report["steps"][i]["seconds"] = json!(start.elapsed().as_secs_f64());
        report["steps"][i]["status"] = json!(if result.is_ok() { "pass" } else { "fail" });
        if let Err(e) = result {
            report["status"] = json!("fail");
            report["error"] = json!(format!("{e:#}"));
            save(out, report)?;
            return Err(e);
        }
        if matches!(step.name, "binaries" | "release") {
            let profile = if step.name == "release" { "release" } else { "debug" };
            for name in ["rusty", "rusty-memoryd", "rusty-cloud"] {
                let path = super::target_dir().join(profile).join(name);
                report["artifacts"][profile][name] =
                    json!({"path": path, "git_blob": git(&["hash-object", &path.display().to_string()])});
            }
        }
        save(out, report)?;
    }
    report["status"] = json!("pass");
    save(out, report)
}
fn receipt(o: &Options, steps: &[Step]) -> Result<Value> {
    let source = source_identity(&super::root())?;
    let configuration: serde_json::Map<String, Value> = [
        "EVAL_MODELS",
        "EVAL_MEMORY",
        "EVAL_TASKS",
        "EVAL_REPEATS",
        "RUSTY_MODE",
        "RUSTY_MAX_REQUESTS",
        "RUSTY_MAX_BUDGET_TOKENS",
        "RUSTY_BUDGET_SECS",
        "RUSTY_TEST_TIME_SCALE",
        "RUSTY_TEST_JOBS",
        "RUST_TEST_THREADS",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), Value::String(v))))
    .collect();
    Ok(
        json!({"revision": source["revision"], "dirty": source["dirty"], "source_start":source, "source_end":null, "source_unchanged":null,
        "status": "running", "scope": if o.changed { "changed-only" } else { "full" }, "live": o.live, "perf": o.perf, "cloud": o.cloud,
        "configuration": configuration, "test_time_scale":super::testing::time_scale()?,
        "steps": steps.iter().map(|s| json!({"name": s.name, "status": "not-run", "seconds": 0.0})).collect::<Vec<_>>()}),
    )
}

const MAX_IDENTITY_OUTPUT: usize = 256 * 1024;
const MAX_IDENTITY_PATHS: usize = 512;
const MAX_IDENTITY_FILE: u64 = 4 * 1024 * 1024;
const MAX_IDENTITY_CONTENT: u64 = 16 * 1024 * 1024;

/// Bound pipe reads before allocation. All callers capture trusted Git builtins,
/// with external diff/text conversion and fsmonitor disabled.
fn bounded_output(mut child: std::process::Child, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let read = child.stdout.take().unwrap().take((limit + 1) as u64).read_to_end(&mut bytes);
    if read.is_err() || bytes.len() > limit {
        let _ = child.kill();
        let _ = child.wait();
        read?;
        bail!("QA source identity output exceeds {limit} bytes");
    }
    ensure!(child.wait()?.success(), "cannot inspect QA source identity");
    Ok(bytes)
}

/// Git identity plus Git-reported tracked changes and nonignored file hashes.
/// Boundary observations do not attest ignored/external inputs or transient
/// edits restored entirely within one stage; ownership still freezes source.
fn source_identity(root: &Path) -> Result<Value> {
    let query = |args: &[&std::ffi::OsStr]| -> Result<Vec<u8>> {
        let child = Command::new("git")
            .args(["-c", "core.fsmonitor=false"])
            .args(args)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        bounded_output(child, MAX_IDENTITY_OUTPUT)
    };
    fn args<'a>(items: &[&'a str]) -> Vec<&'a std::ffi::OsStr> {
        items.iter().map(|s| std::ffi::OsStr::new(*s)).collect()
    }
    let revision = query(&args(&["rev-parse", "HEAD"]))?;
    let tree = query(&args(&["rev-parse", "HEAD^{tree}"]))?;
    let status = query(&args(&["status", "--porcelain=v1", "-z", "--untracked-files=all"]))?;
    // Raw metadata, never binary patch bodies. Hash bounded changed files
    // individually so a large dirty blob cannot be duplicated in this process.
    let diff = query(&args(&[
        "diff",
        "--raw",
        "--no-abbrev",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "HEAD",
        "--",
    ]))?;
    let changed = query(&args(&[
        "diff",
        "--name-only",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--diff-filter=ACMRTUXB",
        "-z",
        "HEAD",
        "--",
    ]))?;
    let untracked = query(&args(&["ls-files", "--others", "--exclude-standard", "-z"]))?;
    let mut paths = std::collections::BTreeSet::new();
    for path in changed.split(|b| *b == 0).chain(untracked.split(|b| *b == 0)).filter(|p| !p.is_empty()) {
        paths.insert(path);
        ensure!(
            paths.len() <= MAX_IDENTITY_PATHS,
            "QA source identity exceeds {MAX_IDENTITY_PATHS} changed/untracked paths"
        );
    }
    let mut data = Vec::new();
    let mut part = |bytes: &[u8]| {
        data.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        data.extend_from_slice(bytes);
    };
    part(&status);
    part(&diff);
    let mut content = 0u64;
    for path in paths {
        part(path);
        let path = std::ffi::OsStr::from_bytes(path);
        let full = root.join(path);
        let metadata = std::fs::symlink_metadata(&full)?;
        ensure!(
            metadata.len() <= MAX_IDENTITY_FILE,
            "QA source identity file exceeds 4 MiB; commit or ignore large inputs first"
        );
        content = content.saturating_add(metadata.len());
        ensure!(content <= MAX_IDENTITY_CONTENT, "QA source identity changed contents exceed 16 MiB");
        use std::os::unix::fs::PermissionsExt;
        part(&metadata.permissions().mode().to_le_bytes());
        if metadata.file_type().is_symlink() {
            part(std::fs::read_link(&full)?.as_os_str().as_bytes());
        } else {
            ensure!(metadata.is_file(), "QA source identity requires regular files or symlinks");
            part(&query(&["hash-object".as_ref(), "--no-filters".as_ref(), "--".as_ref(), path])?);
        }
    }
    let mut hash = Command::new("git")
        .args(["hash-object", "--stdin"])
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    if let Err(error) = hash.stdin.take().unwrap().write_all(&data) {
        let _ = hash.kill();
        let _ = hash.wait();
        return Err(error.into());
    }
    let fingerprint = bounded_output(hash, 128)?;
    ensure!(revision == query(&args(&["rev-parse", "HEAD"]))?, "source changed while taking identity");
    Ok(
        json!({"root":root.canonicalize()?, "revision":String::from_utf8(revision)?.trim(), "tree":String::from_utf8(tree)?.trim(),
        "dirty":!status.is_empty() || !diff.is_empty() || !untracked.is_empty(),
        "fingerprint":String::from_utf8(fingerprint)?.trim()}),
    )
}

fn git(args: &[&str]) -> Option<String> {
    let o = Command::new("git").args(args).current_dir(super::root()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
pub fn command(args: &[String]) -> Result<ExitCode> {
    let mut o = Options::parse(args)?;
    // Runtime/shared/unknown changes conservatively expand to the full gate.
    if o.changed && super::testing::changed()?.is_none() {
        o.changed = false;
    }
    let out: PathBuf = super::target_dir().join("qa");
    let steps = plan(&o, &out);
    if o.list {
        for s in steps {
            println!("{}: {} {}", s.name, s.program, s.args.join(" "));
        }
        return Ok(ExitCode::SUCCESS);
    }
    // Remove only our own old receipts, so an early failure cannot upload a
    // previous run's evidence. Captures stay available for local inspection.
    std::fs::create_dir_all(&out)?;
    use std::os::fd::AsRawFd;
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(out.join("run.lock"))?;
    // Held until this command exits, including on failure or interruption.
    ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "another qa run owns {}",
        out.display()
    );
    for name in ["summary.json", "summary.md", "memory.json", "memory-live.json", "bench.json"] {
        let _ = std::fs::remove_file(out.join(name));
    }
    // Captures contain mutable fixture workspaces (including approved writes).
    // A rerun must neither pass nor fail because an earlier fixture left data.
    let terminal = out.join("terminal");
    if terminal.exists() {
        std::fs::remove_dir_all(&terminal).context("resetting QA-owned terminal fixtures")?;
    }
    let mut report = receipt(&o, &steps)?;
    if let Err(e) = o.preflight() {
        report["status"] = json!("fail");
        report["error"] = json!(e.to_string());
        save(&out, &report)?;
        return Err(e);
    }
    let test_scale = report["test_time_scale"].as_u64().context("QA receipt lacks a timing profile")?;
    execute(&steps, &out, &mut report, |step| step.run(test_scale), || source_identity(&super::root()))?;
    println!("\n✓ qa passed; receipt: {}", out.join("summary.md").display());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_flags_fail_and_offline_never_selects_paid_extensions() {
        assert!(Options::parse(&["--lve".into()]).is_err());
        assert!(Options::parse(&["--full".into(), "--changed".into()]).is_err());
        assert!(Options::parse(&["--changed".into(), "--live".into()]).is_err());
        let changed = plan(&Options { changed: true, ..Options::default() }, Path::new("out"));
        assert_eq!(changed.iter().map(|s| s.name).collect::<Vec<_>>(), ["format", "lint", "tests"]);
        let p = plan(&Options::default(), Path::new("out"));
        assert!(p
            .iter()
            .all(|s| !s.name.starts_with("live")
                && !matches!(s.name, "evals" | "hosted-cloud" | "release" | "performance")));
        assert!(p.iter().any(|s| s.name == "terminal-inspection"));
        let o = Options::parse(&["--live".into(), "--perf".into(), "--cloud".into()]).unwrap();
        let p = plan(&o, Path::new("out"));
        assert_eq!(p.iter().filter(|s| s.name == "release").count(), 1);
        assert!(p.iter().any(|s| s.name == "hosted-cloud"));
    }
    #[test]
    fn a_real_failed_child_stops_the_gate_and_leaves_an_honest_receipt() {
        let out = std::env::temp_dir().join(format!("rusty-qa-test-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let steps = vec![
            Step { name: "broken", program: "/bin/sh".into(), args: vec!["-c".into(), "exit 7".into()] },
            Step {
                name: "must-not-run",
                program: "/bin/sh".into(),
                args: vec!["-c".into(), format!("touch '{}'", out.join("unexpected").display())],
            },
        ];
        let mut r = receipt(&Options::default(), &steps).unwrap();
        assert!(execute(&steps, &out, &mut r, |step| step.run(20), || source_identity(&super::super::root())).is_err());
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(out.join("summary.json")).unwrap()).unwrap();
        assert_eq!(saved["status"], "fail");
        assert_eq!(saved["steps"][0]["status"], "fail");
        assert_eq!(saved["steps"][1]["status"], "not-run");
        assert!(!out.join("unexpected").exists());
        std::fs::remove_dir_all(out).unwrap();
    }
    #[test]
    fn source_drift_stops_later_stages_and_records_both_identities() {
        let out = std::env::temp_dir().join(format!("rusty-qa-drift-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let steps = vec![Step::cargo("edited", &[]), Step::cargo("must-not-run", &[])];
        let mut report = receipt(&Options::default(), &steps).unwrap();
        let original = report["source_start"].clone();
        let observed = std::cell::RefCell::new(original.clone());
        let calls = std::cell::Cell::new(0);
        let result = execute(
            &steps,
            &out,
            &mut report,
            |_| {
                calls.set(calls.get() + 1);
                observed.borrow_mut()["fingerprint"] = json!("different contents");
                Ok(())
            },
            || Ok(observed.borrow().clone()),
        );
        assert!(result.unwrap_err().to_string().contains("source changed"));
        assert_eq!(calls.get(), 1);
        let saved: Value = serde_json::from_slice(&std::fs::read(out.join("summary.json")).unwrap()).unwrap();
        assert_eq!(saved["status"], "fail");
        assert_eq!(saved["source_start"], original);
        assert_ne!(saved["source_start"], saved["source_end"]);
        assert_eq!(saved["source_unchanged"], false);
        assert_eq!(saved["steps"][1]["status"], "not-run");
        std::fs::remove_dir_all(out).unwrap();
    }
    #[test]
    fn identity_detects_changes_while_dirty_status_stays_true() {
        let root = std::env::temp_dir().join(format!(
            "rusty-qa-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git").args(args).current_dir(&root).status().unwrap().success());
        };
        git(&["init", "-q"]);
        let tracked = root.join("tracked.rs");
        std::fs::write(&tracked, "baseline").unwrap();
        git(&["add", "."]);
        git(&[
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "test: identity fixture",
        ]);
        let clean = source_identity(&root).unwrap();
        assert_eq!(clean["dirty"], false);
        std::fs::write(&tracked, "dirty one").unwrap();
        let one = source_identity(&root).unwrap();
        std::fs::write(&tracked, "dirty two").unwrap();
        let two = source_identity(&root).unwrap();
        assert_eq!(one["dirty"], true);
        assert_eq!(two["dirty"], true);
        assert_ne!(one["fingerprint"], two["fingerprint"]);
        let untracked = root.join("new.rs");
        std::fs::write(&untracked, "new one").unwrap();
        let one = source_identity(&root).unwrap();
        std::fs::write(&untracked, "new two").unwrap();
        let two = source_identity(&root).unwrap();
        assert_ne!(one["fingerprint"], two["fingerprint"]);
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&untracked, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mode_one = source_identity(&root).unwrap();
        std::fs::set_permissions(&untracked, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mode_two = source_identity(&root).unwrap();
        assert_eq!(mode_one["dirty"], mode_two["dirty"]);
        assert_ne!(mode_one["fingerprint"], mode_two["fingerprint"]);
        std::fs::write(root.join("large.bin"), vec![b'x'; MAX_IDENTITY_FILE as usize + 1]).unwrap();
        assert!(source_identity(&root).unwrap_err().to_string().contains("exceeds 4 MiB"));
        std::fs::remove_file(root.join("large.bin")).unwrap();
        for n in 0..MAX_IDENTITY_PATHS {
            std::fs::write(root.join(format!("extra-{n}")), b"").unwrap();
        }
        assert!(source_identity(&root).unwrap_err().to_string().contains("changed/untracked paths"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn oversized_git_capture_is_bounded_and_its_child_is_reaped() {
        let pidfile = std::env::temp_dir().join(format!("rusty-qa-output-{}", std::process::id()));
        let script = "import os,sys,time;open(sys.argv[1],'w').write(str(os.getpid()));sys.stdout.write('x'*1048576);sys.stdout.flush();time.sleep(60)";
        let child = Command::new("python3")
            .args(["-c", script])
            .arg(&pidfile)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        assert!(bounded_output(child, MAX_IDENTITY_OUTPUT).unwrap_err().to_string().contains("exceeds"));
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
        let _ = std::fs::remove_file(pidfile);
    }
    #[test]
    fn unchanged_source_can_pass() {
        let out = std::env::temp_dir().join(format!("rusty-qa-stable-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let steps = vec![Step::cargo("ok", &[])];
        let mut report = receipt(&Options::default(), &steps).unwrap();
        let identity = report["source_start"].clone();
        execute(&steps, &out, &mut report, |_| Ok(()), || Ok(identity.clone())).unwrap();
        assert_eq!(report["status"], "pass");
        assert_eq!(report["source_unchanged"], true);
        std::fs::remove_dir_all(out).unwrap();
    }
}
