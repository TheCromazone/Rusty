//! Build once, then run independent libtest binaries concurrently. Cargo keeps
//! its own build lock; the test processes do not need to hold it.
use super::test_process::{self, capture};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};
const CAPTURE_LIMIT: usize = 16 * 1024 * 1024;
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Clone)]
struct Binary {
    name: String,
    path: PathBuf,
}

fn binaries() -> Result<Vec<Binary>> {
    let out = capture(
        Command::new("cargo")
            .args(["test", "--workspace", "--locked", "--no-run", "--message-format=json"])
            .current_dir(super::root()),
        CAPTURE_LIMIT,
        CAPTURE_TIMEOUT,
    )?;
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    ensure!(
        out.success(),
        "building tests failed: {:?}; cleanup: {:?}; notes: {:?}; group {}",
        out.problem,
        out.cleanup_error,
        out.cleanup_notes,
        out.pid
    );
    discovered(&out.stdout)
}

fn discovered(bytes: &[u8]) -> Result<Vec<Binary>> {
    let mut bins = Vec::new();
    let mut complete = false;
    for line in std::str::from_utf8(bytes)?.lines().filter(|line| !line.trim().is_empty()) {
        ensure!(!complete, "Cargo discovery data after completion");
        let v: Value = serde_json::from_str(line).context("malformed Cargo discovery JSON")?;
        if v["reason"] == "build-finished" {
            ensure!(!complete && v["success"] == true, "invalid Cargo build completion");
            complete = true;
        }
        if v["profile"]["test"] == true {
            if let (Some(name), Some(path)) = (v["target"]["name"].as_str(), v["executable"].as_str()) {
                bins.push(Binary { name: name.into(), path: path.into() });
            }
        }
    }
    bins.sort_by(|a, b| a.path.cmp(&b.path));
    bins.dedup_by(|a, b| a.path == b.path);
    ensure!(complete && !bins.is_empty(), "Cargo did not complete nonempty test discovery");
    Ok(bins)
}

/// Unknown paths select everything. In particular a runtime or shared fixture
/// edit must never silently omit a consumer's integration tests.
pub fn changed() -> Result<Option<BTreeSet<String>>> {
    let out =
        Command::new("git").args(["diff", "--name-only", "origin/main", "--"]).current_dir(super::root()).output()?;
    if !out.status.success() {
        bail!("diff against origin/main failed");
    }
    let untracked =
        Command::new("git").args(["ls-files", "--others", "--exclude-standard"]).current_dir(super::root()).output()?;
    if !untracked.status.success() {
        bail!("listing untracked files failed");
    }
    let paths = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&untracked.stdout));
    Ok(selected_paths(&paths))
}

fn selected_paths(paths: &str) -> Option<BTreeSet<String>> {
    let mut names = BTreeSet::new();
    for path in paths.lines().filter(|p| !p.is_empty()) {
        if let Some(name) = path.strip_prefix("tests/").and_then(|p| p.strip_suffix(".rs")).filter(|n| !n.contains('/'))
        {
            names.insert(name.into());
        } else if path == "xtask/src/memory.rs" {
            return None;
        } else if path.starts_with("xtask/") {
            names.insert("xtask".into());
        } else if path.starts_with("src/")
            || path.starts_with("tests/")
            || !(path.ends_with(".md") || path.starts_with("docs/"))
        {
            return None;
        }
    }
    Some(names)
}

struct Run {
    failures: BTreeMap<String, Vec<String>>,
    times: Vec<(String, f64)>,
    binaries: Vec<Value>,
}

struct ParsedSuite {
    passed: u64,
    failed: u64,
    ignored: u64,
    times: Vec<(String, f64)>,
    failures: BTreeMap<String, Vec<String>>,
}

fn parse_suite(bytes: &[u8]) -> Result<ParsedSuite> {
    let mut started = None;
    let mut complete = false;
    let mut active = BTreeSet::new();
    let mut done = BTreeSet::new();
    let mut counts = [0u64; 4];
    let mut parsed = ParsedSuite { passed: 0, failed: 0, ignored: 0, times: Vec::new(), failures: BTreeMap::new() };
    for line in std::str::from_utf8(bytes)?.lines().filter(|line| !line.trim().is_empty()) {
        ensure!(!complete, "libtest output after suite completion");
        let v: Value = serde_json::from_str(line).context("malformed libtest JSON")?;
        let kind = v["type"].as_str().unwrap_or("");
        let event = v["event"].as_str().unwrap_or("");
        match (kind, event) {
            ("suite", "started") => {
                ensure!(started.is_none(), "duplicate libtest suite start");
                started = Some(v["test_count"].as_u64().context("suite lacks test count")?);
            }
            ("suite", "ok" | "failed") => {
                let count = started.context("suite completed before starting")?;
                ensure!(active.is_empty(), "suite has unfinished tests");
                for (i, key) in ["passed", "failed", "ignored", "measured"].into_iter().enumerate() {
                    ensure!(v[key].as_u64() == Some(counts[i]), "suite {key} count disagrees with events");
                }
                let total =
                    counts.iter().try_fold(0u64, |sum, n| sum.checked_add(*n)).context("suite counts overflow")?;
                ensure!(total == count, "suite did not finish every declared test");
                ensure!(v["filtered_out"].as_u64().is_some(), "suite lacks filtered count");
                ensure!((event == "ok") == (counts[1] == 0), "suite result disagrees with failures");
                complete = true;
            }
            ("test", "started" | "ok" | "failed" | "ignored" | "timeout") | ("bench", _) => {
                ensure!(started.is_some(), "test before suite start");
                let name = v["name"].as_str().filter(|name| !name.is_empty()).context("test lacks name")?;
                if event == "started" {
                    ensure!(!done.contains(name) && active.insert(name.to_owned()), "duplicate test start");
                    continue;
                }
                if event == "timeout" {
                    ensure!(active.contains(name), "timeout for unknown test");
                    continue;
                }
                ensure!(done.insert(name.to_owned()), "duplicate test result");
                if kind == "bench" {
                    active.remove(name);
                    counts[3] += 1;
                } else if event == "ignored" {
                    active.remove(name);
                    counts[2] += 1;
                } else {
                    ensure!(active.remove(name), "test completed without starting");
                    counts[usize::from(event == "failed")] += 1;
                }
                if let Some(seconds) = v["exec_time"].as_f64() {
                    parsed.times.push((name.into(), seconds));
                }
                if event == "failed" {
                    parsed
                        .failures
                        .entry(name.into())
                        .or_default()
                        .push(v["stdout"].as_str().unwrap_or("no failure output").into());
                }
            }
            _ => bail!("unrecognized libtest event: {kind}/{event}"),
        }
    }
    ensure!(complete, "libtest suite did not complete");
    parsed.passed = counts[0];
    parsed.failed = counts[1];
    parsed.ignored = counts[2];
    Ok(parsed)
}
fn parse_time_scale(changed: bool, explicit: Option<&str>) -> Result<u64> {
    let scale = match explicit {
        Some(raw) => raw.parse::<u64>().context("RUSTY_TEST_TIME_SCALE must be an integer from 1 to 100")?,
        None => {
            if changed {
                1
            } else {
                20
            }
        }
    };
    if !(1..=100).contains(&scale) {
        bail!("RUSTY_TEST_TIME_SCALE must be an integer from 1 to 100");
    }
    Ok(scale)
}
/// Full qualification keeps the load-tolerant scale; changed checks use the
/// ordinary boundary unless the caller explicitly requests another scale.
pub fn time_scale(changed: bool) -> Result<u64> {
    match std::env::var("RUSTY_TEST_TIME_SCALE") {
        Ok(raw) => parse_time_scale(changed, Some(&raw)),
        Err(std::env::VarError::NotPresent) => parse_time_scale(changed, None),
        Err(e) => Err(e).context("RUSTY_TEST_TIME_SCALE is not valid Unicode"),
    }
}
fn suite(bins: &[Binary], scale: u64) -> Result<Run> {
    let mut run = Run { failures: BTreeMap::new(), times: Vec::new(), binaries: Vec::new() };
    let jobs: usize =
        std::env::var("RUSTY_TEST_JOBS").unwrap_or_else(|_| "4".into()).parse().context("RUSTY_TEST_JOBS")?;
    if jobs == 0 {
        bail!("RUSTY_TEST_JOBS must be positive");
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..jobs.min(bins.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let Some(bin) = bins.get(i) else { break };
                let start = Instant::now();
                // Only libtest gets this flag: no compiler feature gates change.
                // JSON and exec_time are still unstable in the stable harness.
                if test_process::cancelled() {
                    break;
                }
                let out = capture(
                    Command::new(&bin.path)
                        .current_dir(super::root())
                        .env("RUSTC_BOOTSTRAP", "1")
                        .env("RUSTY_TEST_TIME_SCALE", scale.to_string())
                        .args(["-Z", "unstable-options", "--format=json", "--report-time"]),
                    CAPTURE_LIMIT,
                    CAPTURE_TIMEOUT,
                );
                let seconds = start.elapsed().as_secs_f64();
                println!(
                    "{:20} {seconds:8.2}s  {}",
                    bin.name,
                    if out.as_ref().is_ok_and(|o| o.success()) { "captured" } else { "FAILED" }
                );
                results.lock().unwrap().push((bin, seconds, out));
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    for (bin, seconds, out) in results {
        let out = out.with_context(|| format!("running {}", bin.path.display()))?;
        match parse_suite(&out.stdout) {
            Ok(parsed) => {
                println!(
                    "{}: {} passed, {} failed, {} ignored",
                    bin.name, parsed.passed, parsed.failed, parsed.ignored
                );
                run.binaries.push(json!({"name":bin.name,"path":bin.path,"seconds":seconds,
                    "passed":parsed.passed,"failed":parsed.failed,"ignored":parsed.ignored}));
                run.times.extend(parsed.times.into_iter().map(|(name, time)| (format!("{}::{name}", bin.name), time)));
                for (name, messages) in parsed.failures {
                    run.failures.entry(format!("{}::{name}", bin.name)).or_default().extend(messages);
                }
                if out.success() && parsed.failed == 0 {
                    continue;
                }
            }
            Err(error) => {
                run.failures.entry(format!("{} (protocol)", bin.name)).or_default().push(format!(
                    "{error}\n{}\n{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                ));
            }
        }
        if !out.success() {
            run.failures.entry(format!("{} (harness)", bin.name)).or_default().push(format!(
                "{}; {:?}; cleanup: {:?}; notes: {:?}; group {}\n{}\n{}",
                out.status,
                out.problem,
                out.cleanup_error,
                out.cleanup_notes,
                out.pid,
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
    ensure!(!test_process::cancelled(), "test suite interrupted");
    Ok(run)
}
fn print_failures(failures: &BTreeMap<String, Vec<String>>) {
    for (name, messages) in failures {
        println!("\n{name}: failed {} time(s)", messages.len());
        for msg in messages {
            println!("{msg}");
        }
    }
}
fn fresh_report(name: &str) -> Result<PathBuf> {
    let path = super::target_dir().join(name);
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

pub fn tests(only_changed: bool) -> Result<bool> {
    let report = fresh_report("test-suite.json")?;
    let scale = time_scale(only_changed)?;
    println!("Test profile: {} (time scale {scale})", if only_changed { "changed" } else { "full" });
    let mut bins = binaries()?;
    let selection = if only_changed { changed()? } else { None };
    if let Some(names) = &selection {
        bins.retain(|b| names.contains(&b.name));
    }
    let mut run = suite(&bins, scale)?;
    run.times.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!("\nSlowest 20 tests:");
    for (name, time) in run.times.iter().take(20) {
        println!("{time:8.3}s {name}");
    }
    std::fs::write(
        &report,
        serde_json::to_string_pretty(&json!({"binaries":run.binaries,"failures":run.failures,"times":run.times}))?,
    )?;
    print_failures(&run.failures);
    if !run.failures.is_empty() {
        bail!("test suite failed");
    }
    if selection.is_none() {
        let docs = capture(
            Command::new("cargo").args(["test", "--workspace", "--locked", "--doc"]).current_dir(super::root()),
            CAPTURE_LIMIT,
            CAPTURE_TIMEOUT,
        )?;
        print!("{}", String::from_utf8_lossy(&docs.stdout));
        eprint!("{}", String::from_utf8_lossy(&docs.stderr));
        ensure!(
            docs.success(),
            "doctests failed: {:?}; cleanup: {:?}; notes: {:?}; group {}",
            docs.problem,
            docs.cleanup_error,
            docs.cleanup_notes,
            docs.pid
        );
    }
    Ok(selection.is_none())
}
pub fn flaky(args: &[String]) -> Result<ExitCode> {
    let runs = match args {
        [] => 10,
        [flag, n] if flag == "--runs" => n.parse::<usize>().context("--runs")?,
        _ => bail!("usage: cargo xtask flaky [--runs N]"),
    };
    if runs == 0 {
        bail!("--runs must be positive");
    }
    let report = fresh_report("flaky.json")?;
    let scale = time_scale(false)?;
    println!("Test profile: full (time scale {scale})");
    let bins = binaries()?;
    let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut observations = Vec::new();
    for n in 1..=runs {
        let start = Instant::now();
        let load = Command::new("uptime").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        println!("\nRun {n}/{runs}: {}", load.as_deref().unwrap_or("load unavailable"));
        let mut run = suite(&bins, scale)?;
        // Doc tests are part of a full suite too.
        let docs = capture(
            Command::new("cargo").args(["test", "--workspace", "--locked", "--doc"]).current_dir(super::root()),
            CAPTURE_LIMIT,
            CAPTURE_TIMEOUT,
        )?;
        if !docs.success() {
            run.failures.entry("doctests".into()).or_default().push(format!(
                "{:?}; cleanup: {:?}; notes: {:?}; group {}\n{}\n{}",
                docs.problem,
                docs.cleanup_error,
                docs.cleanup_notes,
                docs.pid,
                String::from_utf8_lossy(&docs.stdout),
                String::from_utf8_lossy(&docs.stderr)
            ));
        }
        observations.push(json!({"run":n,"load":load,"seconds":start.elapsed().as_secs_f64(),"test_time_scale":scale,"failures":run.failures,"binaries":run.binaries,"tests":run.times}));
        for (name, messages) in run.failures {
            failures.entry(name).or_default().extend(messages.into_iter().map(|m| format!("run {n}: {m}")));
        }
        std::fs::write(&report, serde_json::to_string_pretty(&json!({"runs":observations,"failures":failures}))?)?;
    }
    print_failures(&failures);
    println!(
        "\n{} tests failed at least once in {runs} runs; report: {}",
        failures.len(),
        super::target_dir().join("flaky.json").display()
    );
    Ok(if failures.is_empty() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

#[cfg(test)]
mod tests {
    use super::{discovered, parse_suite, parse_time_scale, selected_paths};
    use serde_json::{json, Value};
    fn protocol(events: &[Value]) -> Vec<u8> {
        events.iter().map(|v| format!("{v}\n")).collect::<String>().into_bytes()
    }
    fn end(passed: u64, failed: u64, ignored: u64) -> Value {
        json!({"type":"suite","event":if failed==0 {"ok"} else {"failed"},"passed":passed,"failed":failed,"ignored":ignored,"measured":0,"filtered_out":0})
    }
    #[test]
    fn full_discovery_requires_binaries_and_successful_completion() {
        for bytes in [
            Vec::new(),
            protocol(&[json!({"reason":"build-finished","success":true})]),
            protocol(&[json!({"profile":{"test":true},"target":{"name":"fake"},"executable":"/fake"})]),
        ] {
            assert!(discovered(&bytes).is_err());
        }
        let bytes = protocol(&[
            json!({"profile":{"test":true},"target":{"name":"fake"},"executable":"/fake"}),
            json!({"reason":"build-finished","success":true}),
        ]);
        assert_eq!(discovered(&bytes).unwrap().len(), 1);
    }
    #[test]
    fn zero_test_binary_requires_a_completed_suite() {
        let bytes = protocol(&[json!({"type":"suite","event":"started","test_count":0}), end(0, 0, 0)]);
        let suite = parse_suite(&bytes).unwrap();
        assert_eq!((suite.passed, suite.failed, suite.ignored), (0, 0, 0));
    }
    #[test]
    fn completed_counts_and_failure_messages_are_preserved() {
        let bytes = protocol(&[
            json!({"type":"suite","event":"started","test_count":3}),
            json!({"type":"test","event":"started","name":"passes"}),
            json!({"type":"test","event":"ok","name":"passes","exec_time":0.1}),
            json!({"type":"test","event":"ignored","name":"needs_service"}),
            json!({"type":"test","event":"started","name":"fails"}),
            json!({"type":"test","event":"failed","name":"fails","stdout":"expected marker missing"}),
            end(1, 1, 1),
        ]);
        let suite = parse_suite(&bytes).unwrap();
        assert_eq!((suite.passed, suite.failed, suite.ignored), (1, 1, 1));
        assert_eq!(suite.failures["fails"], ["expected marker missing"]);
    }
    #[test]
    fn incomplete_malformed_or_contradictory_suites_fail_closed() {
        for bytes in [
            Vec::new(),
            b"diagnostic\n".to_vec(),
            b"{\"type\":\"suite\"".to_vec(),
            protocol(&[json!({"type":"suite","event":"started","test_count":0})]),
            protocol(&[end(0, 0, 0)]),
            protocol(&[json!({"type":"suite","event":"started","test_count":1}), end(1, 0, 0)]),
            protocol(&[json!({"type":"suite","event":"started","test_count":0}), end(0, 0, 0), end(0, 0, 0)]),
            protocol(&[
                json!({"type":"suite","event":"started","test_count":0}),
                json!({"type":"suite","event":"failed","passed":0,"failed":0,"ignored":0,"measured":0,"filtered_out":0}),
            ]),
        ] {
            assert!(parse_suite(&bytes).is_err(), "{}", String::from_utf8_lossy(&bytes));
        }
    }
    #[test]
    fn exit_zero_fake_harnesses_do_not_qualify_without_completion() {
        for output in ["diagnostic", r#"{"type":"suite","event":"started","test_count":0}"#] {
            let captured = super::capture(
                std::process::Command::new("/bin/sh")
                    .args(["-c", r#"printf '%s\n' "$FIXTURE""#])
                    .env("FIXTURE", output),
                4096,
                std::time::Duration::from_secs(60),
            )
            .unwrap();
            assert!(captured.success());
            assert!(parse_suite(&captured.stdout).is_err());
        }
    }

    #[test]
    fn timing_profiles_validate_and_canonicalize_before_launch() {
        assert_eq!(parse_time_scale(false, None).unwrap(), 20);
        assert_eq!(parse_time_scale(true, None).unwrap(), 1);
        for changed in [false, true] {
            for (raw, canonical) in [("1", 1), ("001", 1), ("20", 20), ("100", 100)] {
                assert_eq!(parse_time_scale(changed, Some(raw)).unwrap(), canonical);
            }
            for raw in ["0", "101", "1000", "garbage", "", "-1", "1.0", " 1", "18446744073709551616"] {
                assert!(parse_time_scale(changed, Some(raw)).is_err(), "{raw:?}");
            }
        }
    }
    #[test]
    fn test_and_xtask_edits_select_their_binaries() {
        let names = selected_paths("tests/cli.rs\nxtask/src/main.rs\n").unwrap();
        assert_eq!(names.into_iter().collect::<Vec<_>>(), ["cli", "xtask"]);
    }
    #[test]
    fn runtime_shared_fixtures_and_smoke_changes_require_the_full_gate() {
        for path in [
            "src/budget.rs",
            "src/prompt.md",
            "tests/support/mod.rs",
            "tests/fixture.md",
            "Cargo.lock",
            "xtask/src/memory.rs",
        ] {
            assert!(selected_paths(path).is_none(), "{path}");
        }
    }
    #[test]
    fn documentation_does_not_select_a_test_binary() {
        assert!(selected_paths("AGENTS.md\ndocs/MEMORY.md").unwrap().is_empty());
    }
}
