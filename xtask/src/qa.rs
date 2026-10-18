//! One gate locally and in CI. Extensions spend nothing unless requested.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

#[derive(Default)]
struct Options {
    live: bool,
    perf: bool,
    cloud: bool,
    list: bool,
}
impl Options {
    fn parse(args: &[String]) -> Result<Self> {
        let mut o = Self::default();
        for a in args {
            match a.as_str() {
                "--live" => o.live = true,
                "--perf" => o.perf = true,
                "--cloud" => o.cloud = true,
                "--list" => o.list = true,
                _ => bail!("unknown qa argument {a:?}; use --live, --perf, --cloud or --list"),
            }
        }
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
    fn run(&self) -> Result<()> {
        let mut command = Command::new(&self.program);
        if self.name == "evals" {
            command.env("EVAL_BINARY", super::target_dir().join("release/rusty"));
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
        Step::cargo("tests", &["test", "--workspace", "--locked", "--", "--test-threads", "2"]),
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
    md.push_str(&format!("\nExtensions requested: live={}, performance={}, hosted cloud={}.\nUnrequested or unrun gates are not qualification evidence.\n", report["live"], report["perf"], report["cloud"]));
    std::fs::write(out.join("summary.md"), md)?;
    Ok(())
}
/// Save before starting and after every step. A failed or never-run gate
/// cannot disappear from the receipt or be mistaken for success.
fn execute(steps: &[Step], out: &Path, report: &mut Value, mut run: impl FnMut(&Step) -> Result<()>) -> Result<()> {
    save(out, report)?;
    for (i, step) in steps.iter().enumerate() {
        println!("\n▸ {}", step.name);
        report["steps"][i]["status"] = json!("running");
        save(out, report)?;
        let start = Instant::now();
        let result = run(step);
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
fn receipt(o: &Options, steps: &[Step]) -> Value {
    let configuration: serde_json::Map<String, Value> = [
        "EVAL_MODELS",
        "EVAL_MEMORY",
        "EVAL_TASKS",
        "EVAL_REPEATS",
        "RUSTY_MODE",
        "RUSTY_MAX_REQUESTS",
        "RUSTY_MAX_BUDGET_TOKENS",
        "RUSTY_BUDGET_SECS",
    ]
    .iter()
    .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), Value::String(v))))
    .collect();
    json!({"revision": git(&["rev-parse", "HEAD"]), "dirty": git(&["status", "--porcelain"]).is_none_or(|s| !s.is_empty()),
        "status": "running", "live": o.live, "perf": o.perf, "cloud": o.cloud,
        "configuration": configuration,
        "steps": steps.iter().map(|s| json!({"name": s.name, "status": "not-run", "seconds": 0.0})).collect::<Vec<_>>()})
}

fn git(args: &[&str]) -> Option<String> {
    let o = Command::new("git").args(args).current_dir(super::root()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}
pub fn command(args: &[String]) -> Result<ExitCode> {
    let o = Options::parse(args)?;
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
    let mut report = receipt(&o, &steps);
    if let Err(e) = o.preflight() {
        report["status"] = json!("fail");
        report["error"] = json!(e.to_string());
        save(&out, &report)?;
        return Err(e);
    }
    execute(&steps, &out, &mut report, Step::run)?;
    println!("\n✓ qa passed; receipt: {}", out.join("summary.md").display());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_flags_fail_and_offline_never_selects_paid_extensions() {
        assert!(Options::parse(&["--lve".into()]).is_err());
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
        let mut r = receipt(&Options::default(), &steps);
        assert!(execute(&steps, &out, &mut r, Step::run).is_err());
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(out.join("summary.json")).unwrap()).unwrap();
        assert_eq!(saved["status"], "fail");
        assert_eq!(saved["steps"][0]["status"], "fail");
        assert_eq!(saved["steps"][1]["status"], "not-run");
        assert!(!out.join("unexpected").exists());
        std::fs::remove_dir_all(out).unwrap();
    }
    #[test]
    fn scope_runs_unknown_code_and_deletions_but_skips_only_documentation() {
        let out = std::env::temp_dir().join(format!("rusty-scope-test-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let git = |args: &[&str]| {
            let status = Command::new("git").args(args).current_dir(&out).status().unwrap();
            assert!(status.success());
        };
        git(&["init", "-q"]);
        std::fs::write(out.join("runtime.rs"), "baseline").unwrap();
        git(&["add", "."]);
        git(&["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "-qm", "test: baseline"]);
        let scope = |base: &str| {
            let o = Command::new("bash")
                .arg(super::super::root().join(".github/ci-scope.sh"))
                .args([base, "HEAD"])
                .current_dir(&out)
                .output()
                .unwrap();
            assert!(o.status.success());
            String::from_utf8(o.stdout).unwrap()
        };
        std::fs::write(out.join("README.md"), "docs").unwrap();
        git(&["add", "."]);
        git(&["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "-qm", "docs: explain"]);
        assert_eq!(scope("HEAD^"), "runtime=false\n");
        std::fs::remove_file(out.join("runtime.rs")).unwrap();
        std::fs::write(out.join("unknown.config"), "new runtime input").unwrap();
        git(&["add", "-A"]);
        git(&["-c", "user.name=test", "-c", "user.email=test@example.com", "commit", "-qm", "test: runtime change"]);
        assert_eq!(scope("HEAD^"), "runtime=true\n");
        assert_eq!(scope("missing-revision"), "runtime=true\n");
        std::fs::remove_dir_all(out).unwrap();
    }
}
