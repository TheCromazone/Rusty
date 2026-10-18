//! A fixed picture of the workspace for the system prompt: what is here and
//! what can run, gathered once at startup without a model call. It saves the
//! first few exploratory calls, which matters most for smaller models.
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

const MAX_ENTRIES: usize = 40;
const TOOLS: &[&str] = &[
    "git",
    "python3",
    "uv",
    "pip",
    "node",
    "npm",
    "cargo",
    "go",
    "java",
    "make",
    "docker",
    "kubectl",
    "helm",
    "terraform",
    "tofu",
    "psql",
    "redis-cli",
    "curl",
    "jq",
    "rg",
    "aws",
    "gcloud",
    "az",
    "sc",
];

pub fn take(cwd: &Path) -> String {
    let mut lines = vec!["Workspace snapshot (taken once at startup; re-check anything you rely on):".to_string()];
    if let Some(files) = files(cwd) {
        lines.push(format!("- files here: {files}"));
    }
    if let Some(git) = git(cwd) {
        lines.push(format!("- git: {git}"));
    }
    let tools = on_path(TOOLS);
    if !tools.is_empty() {
        lines.push(format!("- tools on PATH: {}", tools.join(", ")));
    }
    if lines.len() == 1 {
        return String::new();
    }
    format!("\n{}\n", lines.join("\n"))
}

/// Top-level entries, directories first and marked with `/`.
fn files(cwd: &Path) -> Option<String> {
    let mut entries: Vec<(bool, String)> = std::fs::read_dir(cwd)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| (e.file_type().is_ok_and(|t| t.is_dir()), e.file_name().to_string_lossy().into_owned()))
        .filter(|(_, name)| !matches!(name.as_str(), ".git" | "target" | "node_modules" | "__pycache__" | ".venv"))
        .collect();
    if entries.is_empty() {
        return None;
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let more = entries.len().saturating_sub(MAX_ENTRIES);
    let mut shown: Vec<String> =
        entries.into_iter().take(MAX_ENTRIES).map(|(dir, n)| if dir { format!("{n}/") } else { n }).collect();
    if more > 0 {
        shown.push(format!("+{more} more"));
    }
    Some(shown.join(" "))
}

/// Branch, a dirty mark and the last three subjects.
fn git(cwd: &Path) -> Option<String> {
    let run = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(cwd).args(args).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    // Independent reads, so they run together: `status` alone can take
    // longer than the other two in a large tree.
    let (branch, dirty, log) = std::thread::scope(|s| {
        let dirty = s.spawn(|| run(&["status", "--porcelain"]));
        let log = s.spawn(|| run(&["log", "--oneline", "-3", "--no-decorate"]));
        (run(&["rev-parse", "--abbrev-ref", "HEAD"]), dirty.join().unwrap(), log.join().unwrap())
    });
    let branch = branch?;
    let dirty = dirty.is_some_and(|s| !s.is_empty());
    let log = log.unwrap_or_default();
    let recent: Vec<&str> = log.lines().collect();
    Some(format!(
        "branch {branch}{}; recent: {}",
        if dirty { " (uncommitted changes)" } else { "" },
        if recent.is_empty() { "no commits".into() } else { recent.join(" | ") }
    ))
}

fn on_path(names: &[&str]) -> Vec<String> {
    let dirs: Vec<std::path::PathBuf> =
        std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    names.iter().filter(|n| dirs.iter().any(|d| executable(&d.join(n)))).map(|n| n.to_string()).collect()
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Git blob ids of the tracked files as they are now, by path relative to
/// `cwd`: the index's ids, with files already modified hashed as they stand.
/// None outside a git work tree.
pub fn blobs(cwd: &Path) -> Option<HashMap<String, String>> {
    let git = |args: &[&str]| {
        let out = Command::new("git").args(args).current_dir(cwd).output().ok()?;
        out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let (listed, dirty) = std::thread::scope(|s| {
        let dirty = s.spawn(|| git(&["diff", "--name-only", "--relative", "-z"]));
        (git(&["ls-files", "-s", "-z"]), dirty.join().unwrap())
    });
    let mut map = HashMap::new();
    for entry in listed?.split('\0') {
        if let Some((meta, path)) = entry.split_once('\t') {
            if let Some(blob) = meta.split_whitespace().nth(1) {
                map.insert(path.to_string(), blob.to_string());
            }
        }
    }
    let dirty: Vec<String> = dirty?.split('\0').filter(|p| !p.is_empty()).map(String::from).collect();
    for (path, blob) in hash_now(cwd, &dirty) {
        match blob {
            Some(b) => map.insert(path, b),
            None => map.remove(&path),
        };
    }
    Some(map)
}

/// Each path's git blob id as the file stands now, or None if it is gone.
/// Empty when git can't hash them.
pub fn hash_now(cwd: &Path, paths: &[String]) -> Vec<(String, Option<String>)> {
    let present: Vec<&String> = paths.iter().filter(|p| cwd.join(p).is_file()).collect();
    let mut ids = Vec::new();
    if !present.is_empty() {
        let out = Command::new("git")
            .arg("hash-object")
            .arg("--")
            .args(present.iter().map(|p| p.as_str()))
            .current_dir(cwd)
            .output();
        match out.ok().filter(|o| o.status.success()) {
            Some(o) => ids = String::from_utf8_lossy(&o.stdout).lines().map(String::from).collect(),
            // Unknown, not gone: say nothing about any of them.
            None => return Vec::new(),
        }
    }
    let mut ids = ids.into_iter();
    paths
        .iter()
        .map(|p| {
            let id = if cwd.join(p).is_file() { ids.next() } else { None };
            (p.clone(), id)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_files_git_and_tools_within_bounds() {
        let dir = std::env::temp_dir().join(format!("rusty-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        for i in 0..50 {
            std::fs::write(dir.join(format!("f{i:02}.txt")), "").unwrap();
        }
        let ok = |args: &[&str]| Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success();
        let has_git = ok(&["init", "-q"])
            && ok(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "first"]);
        let s = take(&dir);
        assert!(s.contains("files here: src/ f00.txt"), "{s}");
        assert!(s.contains("+11 more") && !s.contains("node_modules"), "{s}");
        if has_git {
            assert!(s.contains("recent: ") && s.contains("first") && s.contains("(uncommitted changes)"), "{s}");
        }
        assert!(s.len() < 2_000, "{}", s.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_directory_adds_nothing_but_tools() {
        let dir = std::env::temp_dir().join(format!("rusty-snapshot-empty-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!take(&dir).contains("files here"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
