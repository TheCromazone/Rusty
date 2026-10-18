//! Offline correctness receipts: compare live queries with an unbounded direct rg reference.
use rusty::workspace_query::{files_under, search, search_files, SearchResult};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rusty-query-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    fn put(&self, path: &str, contents: &str) {
        let path = self.0.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    fn query(&self, pattern: &str, glob: Option<&str>, context: u64) -> SearchResult {
        let result = search(pattern, self.0.to_str().unwrap(), glob, context).unwrap();
        assert_eq!(result.engine, "rg", "correctness fixtures require installed ripgrep");
        let mut reference = Command::new("rg");
        reference.args([
            "--no-config",
            "--no-context-separator",
            "-nH",
            "--no-heading",
            "--color",
            "never",
            "--smart-case",
        ]);
        reference.args(["-C", &context.min(10).to_string()]);
        if let Some(g) = glob {
            reference.args(["-g", g]);
        }
        let raw = reference.args(["-e", pattern, "--"]).arg(&self.0).output().unwrap();
        assert_eq!(result.exit_code, raw.status.code());
        if !result.output.truncated {
            let canonical = |text: &str| {
                let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
                lines.sort_unstable();
                lines
            };
            assert_eq!(canonical(&result.output.text), canonical(&String::from_utf8_lossy(&raw.stdout)));
        }
        assert_eq!(
            result.scan_complete,
            matches!(raw.status.code(), Some(0 | 1))
                && raw.stderr.is_empty()
                && result.audit_complete
                && !result.match_limit_possible
        );
        result
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn fixed_regex_case_glob_context_and_visibility_match_reference() {
    let f = Fixture::new();
    git(&f.0, &["init", "-q"]);
    f.put(".gitignore", "ignored/\n");
    f.put("src/a.rs", "before\nneedle retry\nNeedle deadline\nafter\n");
    f.put("src/nested/b.rs", "deadline\n");
    f.put("docs/a.txt", "needle\n");
    f.put("ignored/no.rs", "needle\n");
    f.put(".hidden.rs", "needle\n");
    f.put("src/binary.rs", "\0needle\n");
    for pattern in ["needle", "Needle", "retry|deadline", "^before$", "absent_literal", "\\bdeadline\\b"] {
        for glob in [None, Some("*.rs"), Some("src/**/*.rs"), Some("!*.txt")] {
            for context in [0, 2, 100] {
                let result = f.query(pattern, glob, context);
                assert!(result.scan_complete);
                assert!(!result.output.text.contains("binary.rs"));
                if glob.is_none() {
                    assert!(!result.output.text.contains("ignored/no.rs"));
                    assert!(!result.output.text.contains(".hidden.rs"));
                }
            }
        }
    }
    let enumeration = files_under(&f.0);
    assert!(!enumeration.incomplete);
    // Glob/outline intentionally retain their prior visibility policy, unlike rg.
    assert!(enumeration.files.contains(&f.0.join(".hidden.rs")));
    assert!(enumeration.files.contains(&f.0.join("ignored/no.rs")));
    assert!(!enumeration.files.iter().any(|p| p.starts_with(f.0.join(".git"))));
}

#[test]
fn empty_results_observe_user_shell_add_delete_rename_and_ignore_changes() {
    let f = Fixture::new();
    git(&f.0, &["init", "-q"]);
    f.put("a.rs", "old value\n");
    assert!(f.query("needle", None, 0).output.text.is_empty());
    // Same-length overwrite: timestamps/length are insufficient freshness keys.
    f.put("a.rs", "needle!!!\n");
    assert!(f.query("needle", None, 0).output.text.contains("a.rs:1:"));
    f.put("new.rs", "needle\n");
    assert!(f.query("needle", None, 0).output.text.contains("new.rs:1:"));
    fs::rename(f.0.join("new.rs"), f.0.join("renamed.rs")).unwrap();
    let renamed = f.query("needle", None, 0).output.text;
    assert!(renamed.contains("renamed.rs:1:") && !renamed.contains("new.rs:1:"));
    fs::remove_file(f.0.join("a.rs")).unwrap();
    assert!(!f.query("needle", None, 0).output.text.contains("a.rs:1:"));
    f.put(".gitignore", "renamed.rs\n");
    assert!(f.query("needle", None, 0).output.text.is_empty());
    f.put(".gitignore", "");
    assert!(f.query("needle", None, 0).output.text.contains("renamed.rs:1:"));
    let status = Command::new("sh")
        .args(["-c", "printf 'needle from shell\n' > shell.rs; exit 1"])
        .current_dir(&f.0)
        .status()
        .unwrap();
    assert!(!status.success());
    assert!(f.query("needle", None, 0).output.text.contains("shell.rs:1:"));
}

#[test]
fn separate_git_worktrees_never_share_query_results() {
    let f = Fixture::new();
    git(&f.0, &["init", "-q"]);
    f.put("a.rs", "first\n");
    git(&f.0, &["add", "."]);
    git(&f.0, &["-c", "user.name=Query Test", "-c", "user.email=query@example.invalid", "commit", "-qm", "fixture"]);
    let other = Fixture::new();
    git(&f.0, &["worktree", "add", "--detach", other.0.to_str().unwrap(), "HEAD"]);
    other.put("a.rs", "needle\n");
    assert!(f.query("needle", None, 0).output.text.is_empty());
    assert!(other.query("needle", None, 0).output.text.contains("a.rs:1:needle"));
    f.put("a.rs", "needle root\n");
    assert!(f.query("needle", None, 0).output.text.contains("needle root"));
    assert!(!other.query("needle", None, 0).output.text.contains("needle root"));
}

#[test]
fn output_truncation_is_distinct_from_incomplete_scans() {
    let f = Fixture::new();
    f.put("a.txt", &"needle\n".repeat(2_000));
    let full = f.query("needle", None, 0);
    assert!(!full.scan_complete && full.output.truncated && full.match_limit_possible);
    assert!(full.render().contains("scan_complete=false output_truncated=true"));
    // Keep the engine's cheap per-file limit and disclose possible omissions.
    f.put("a.txt", &"needle\n".repeat(40));
    let forty = f.query("needle", None, 0);
    assert_eq!(forty.output.text.lines().count(), 30);
    assert!(forty.output.truncated && forty.match_limit_possible && !forty.scan_complete);
    f.put("a.txt", &"needle\n".repeat(29));
    let below = f.query("needle", None, 0);
    assert!(below.scan_complete && !below.output.truncated);
    f.put("a.txt", &"needle\n".repeat(30));
    let boundary = f.query("needle", None, 0);
    assert!(boundary.match_limit_possible, "exactly 30 is conservatively incomplete");
    f.put("a.txt", &format!("{}needle\n", "x".repeat(400)));
    let wide = f.query("needle", None, 0);
    assert!(wide.scan_complete && wide.output.truncated && wide.column_limit_possible);
    assert!(!wide.output.text.contains(&"x".repeat(400)));
    assert!(wide.output.text.contains("[Omitted "));
    let invalid = f.query("[", None, 0);
    assert!(!invalid.scan_complete && !invalid.diagnostics.text.is_empty());
    let missing = search("needle", f.0.join("missing").to_str().unwrap(), None, 0).unwrap();
    assert!(!missing.scan_complete && missing.render().contains("search incomplete"));
    assert!(missing.render().contains("missing"));
}

#[test]
fn dense_and_wide_files_do_not_expand_engine_output() {
    let f = Fixture::new();
    f.put("dense.txt", &"needle\n".repeat(100_000));
    let dense = f.query("needle", None, 0);
    assert_eq!(dense.output.text.lines().count(), 30);
    assert!(dense.match_limit_possible && dense.output.truncated && !dense.scan_complete);
    fs::remove_file(f.0.join("dense.txt")).unwrap();
    f.put("wide.txt", &format!("{}needle\n", "x".repeat(1_000_000)));
    let wide = f.query("needle", None, 0);
    assert!(wide.scan_complete && wide.column_limit_possible && wide.output.truncated);
    assert!(wide.output.text.len() < 1_000);
}

#[test]
fn bounded_parallel_output_is_sorted_without_claiming_a_stable_subset() {
    let f = Fixture::new();
    for i in (0..80).rev() {
        f.put(&format!("src/{i:03}.txt"), &"needle\n".repeat(20));
    }
    let result = f.query("needle", None, 0);
    assert!(result.output.truncated && !result.match_limit_possible && result.audit_complete && result.scan_complete);
    assert!(result.output.text.len() <= rusty::workspace_query::OUTPUT_BYTES);
    let mut previous = None;
    for line in result.output.text.lines() {
        let (file, tail) = line.split_once(':').unwrap();
        let number = tail.split_once(':').unwrap().0.parse::<u64>().unwrap();
        if let Some(prior) = previous {
            assert!(prior <= (file, number));
        }
        previous = Some((file, number));
    }
}

#[test]
#[cfg(unix)]
fn skipped_paths_and_partial_matches_keep_error_evidence() {
    use std::os::unix::{fs::symlink, fs::PermissionsExt};
    let f = Fixture::new();
    f.put("good.rs", "needle\n");
    f.put("private.rs", "needle secret\n");
    fs::set_permissions(f.0.join("private.rs"), fs::Permissions::from_mode(0o0)).unwrap();
    let partial = f.query("needle", None, 0);
    fs::set_permissions(f.0.join("private.rs"), fs::Permissions::from_mode(0o600)).unwrap();
    assert!(!partial.scan_complete);
    assert!(partial.output.text.contains("good.rs"));
    assert!(partial.render().contains("private.rs") && partial.render().contains("Permission denied"));
    symlink(f.0.join("good.rs"), f.0.join("alias.rs")).unwrap();
    let aliases = f.query("needle", None, 0);
    assert!(!aliases.output.text.contains("alias.rs"), "rg does not follow descendant file symlinks");
    symlink(&f.0, f.0.join("loop")).unwrap();
    symlink(f.0.join("absent"), f.0.join("broken.rs")).unwrap();
    let scan = files_under(&f.0);
    assert!(scan.incomplete && scan.skipped == 2);
    assert!(scan.files.contains(&f.0.join("alias.rs")), "glob/outline retain regular-file symlinks");
    let report = scan.render("files observed", false);
    assert!(report.contains("loop") && report.contains("broken.rs"));
    assert!(report.contains("scan_complete=false output_truncated=false"));
}

/// Same-machine plumbing timing, not solve-rate evidence. Explicitly opt in.
#[test]
#[ignore = "opt-in same-machine timing benchmark; run with --ignored --nocapture"]
fn benchmark_query_reference_and_legacy() {
    use std::time::Instant;
    let f = Fixture::new();
    for i in 0..400 {
        f.put(
            &format!("src/{i:04}.rs"),
            &format!("{}{}\n", "ordinary content\n".repeat(24), if i % 10 == 0 { "needle" } else { "other" }),
        );
    }
    let mut old = Vec::new();
    let mut new = Vec::new();
    for i in 0..16 {
        let legacy = || {
            let start = Instant::now();
            let output = Command::new("rg")
                .args([
                    "--line-number",
                    "--no-heading",
                    "--color",
                    "never",
                    "--smart-case",
                    "--max-columns",
                    "300",
                    "--max-count",
                    "30",
                    "-C",
                    "0",
                    "-e",
                    "needle",
                ])
                .arg(&f.0)
                .output()
                .unwrap();
            assert!(output.status.success());
            (start.elapsed().as_micros(), output.stdout)
        };
        let current = || {
            let start = Instant::now();
            let result = search("needle", f.0.to_str().unwrap(), None, 0).unwrap();
            let elapsed = start.elapsed().as_micros();
            assert!(result.scan_complete && !result.output.truncated);
            (elapsed, result.output.text)
        };
        let (a, b) = if i % 2 == 0 {
            let a = legacy();
            (a, current())
        } else {
            let b = current();
            (legacy(), b)
        };
        let mut old_lines: Vec<_> = std::str::from_utf8(&a.1).unwrap().lines().collect();
        let mut new_lines: Vec<_> = b.1.lines().collect();
        old_lines.sort_unstable();
        new_lines.sort_unstable();
        assert_eq!(old_lines, new_lines);
        eprintln!("query_bench iteration={i} legacy_us={} current_us={} equal=true", a.0, b.0);
        old.push(a.0);
        new.push(b.0);
    }
    let cold = (old.remove(0), new.remove(0));
    old.sort_unstable();
    new.sort_unstable();
    eprintln!(
        "query_bench files=400 cold_us={cold:?} warm_n=15 legacy_median_us={} current_median_us={} cache=none",
        old[7], new[7]
    );
}

#[test]
fn search_files_lists_what_ripgrep_would_search() {
    let f = Fixture::new();
    f.put("src/a.rs", "fn a() {}\n");
    f.put(".hidden/b.rs", "fn b() {}\n");
    f.put("ignored/c.rs", "fn c() {}\n");
    f.put(".ignore", "ignored/\n");
    let scan = search_files(&f.0);
    let mut listed: Vec<String> = scan.files.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    listed.sort();
    let reference = Command::new("rg").args(["--no-config", "--files", "--"]).arg(&f.0).output().unwrap();
    let mut expected: Vec<String> = String::from_utf8_lossy(&reference.stdout).lines().map(String::from).collect();
    expected.sort();
    assert_eq!(listed, expected);
    assert!(listed.iter().any(|p| p.ends_with("src/a.rs")), "{listed:?}");
    assert!(!listed.iter().any(|p| p.contains("ignored") || p.contains(".hidden")), "{listed:?}");
    assert!(!scan.incomplete && scan.skipped == 0, "{:?}", scan.diagnostics);
}
