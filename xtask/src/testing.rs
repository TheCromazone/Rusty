//! Build once, then run independent libtest binaries concurrently. Cargo keeps
//! its own build lock; the test processes do not need to hold it.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::time::Instant;

#[derive(Clone)]
struct Binary {
    name: String,
    path: PathBuf,
}

fn binaries() -> Result<Vec<Binary>> {
    let out = Command::new("cargo")
        .args(["test", "--workspace", "--locked", "--no-run", "--message-format=json"])
        .current_dir(super::root())
        .output()?;
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    if !out.status.success() {
        bail!("building tests failed");
    }
    let mut bins = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        if v["profile"]["test"] == true {
            if let (Some(name), Some(path)) = (v["target"]["name"].as_str(), v["executable"].as_str()) {
                bins.push(Binary { name: name.into(), path: path.into() });
            }
        }
    }
    bins.sort_by(|a, b| a.path.cmp(&b.path));
    bins.dedup_by(|a, b| a.path == b.path);
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
    binaries: Vec<(String, f64)>,
}
fn suite(bins: &[Binary]) -> Result<Run> {
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
                let out = Command::new(&bin.path)
                    .current_dir(super::root())
                    .env("RUSTC_BOOTSTRAP", "1")
                    .env("RUSTY_TEST_TIME_SCALE", "20")
                    .args(["-Z", "unstable-options", "--format=json", "--report-time"])
                    .output();
                let seconds = start.elapsed().as_secs_f64();
                println!(
                    "{:20} {seconds:8.2}s  {}",
                    bin.name,
                    if out.as_ref().is_ok_and(|o| o.status.success()) { "ok" } else { "FAILED" }
                );
                results.lock().unwrap().push((bin, seconds, out));
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    for (bin, seconds, out) in results {
        let out = out.with_context(|| format!("running {}", bin.path.display()))?;
        run.binaries.push((bin.name.clone(), seconds));
        let mut failures = 0;
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
            if v["type"] != "test" {
                continue;
            }
            let name = format!("{}::{}", bin.name, v["name"].as_str().unwrap_or("?"));
            if let Some(t) = v["exec_time"].as_f64() {
                run.times.push((name.clone(), t));
            }
            if v["event"] == "failed" {
                failures += 1;
                run.failures.entry(name).or_default().push(v["stdout"].as_str().unwrap_or("no failure output").into());
            }
        }
        if !out.status.success() && failures == 0 {
            run.failures.entry(format!("{} (harness)", bin.name)).or_default().push(format!(
                "{}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            ));
        }
    }
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
pub fn tests(only_changed: bool) -> Result<bool> {
    let mut bins = binaries()?;
    let selection = if only_changed { changed()? } else { None };
    if let Some(names) = &selection {
        bins.retain(|b| names.contains(&b.name));
    }
    let mut run = suite(&bins)?;
    run.times.sort_by(|a, b| b.1.total_cmp(&a.1));
    println!("\nSlowest 20 tests:");
    for (name, time) in run.times.iter().take(20) {
        println!("{time:8.3}s {name}");
    }
    print_failures(&run.failures);
    if !run.failures.is_empty() {
        bail!("test suite failed");
    }
    if selection.is_none() {
        super::sh("cargo", &["test", "--workspace", "--locked", "--doc"])?;
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
    let bins = binaries()?;
    let mut failures: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut observations = Vec::new();
    for n in 1..=runs {
        let start = Instant::now();
        let load = Command::new("uptime").output().ok().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        println!("\nRun {n}/{runs}: {}", load.as_deref().unwrap_or("load unavailable"));
        let run = suite(&bins)?;
        // Doc tests are part of a full suite too.
        let docs = Command::new("cargo")
            .args(["test", "--workspace", "--locked", "--doc"])
            .current_dir(super::root())
            .output()?;
        if !docs.status.success() {
            failures.entry("doctests".into()).or_default().push(format!(
                "run {n}: {}\n{}",
                String::from_utf8_lossy(&docs.stdout),
                String::from_utf8_lossy(&docs.stderr)
            ));
        }
        observations.push(json!({"run":n,"load":load,"seconds":start.elapsed().as_secs_f64(),"failures":run.failures,"binaries":run.binaries,"tests":run.times}));
        for (name, messages) in run.failures {
            failures.entry(name).or_default().extend(messages.into_iter().map(|m| format!("run {n}: {m}")));
        }
        let report = super::target_dir().join("flaky.json");
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
    use super::selected_paths;
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
