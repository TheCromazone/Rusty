//! Live, deterministic retrieval. No query results or filesystem metadata are cached.
//! Completeness is relative to the reported visibility policy, never an atomic snapshot.

use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const SKIP_DIRS: &[&str] =
    &[".git", "target", "node_modules", ".venv", "venv", "__pycache__", "dist", "build", ".next"];
pub const OUTPUT_BYTES: usize = 28_000;
const DIAGNOSTIC_BYTES: usize = 2_000;
const ENTRY_LIMIT: usize = 20_000;

/// A bounded prefix, drained to EOF so presentation limits never stop the scan.
#[derive(Debug)]
pub struct Capture {
    pub text: String,
    pub truncated: bool,
}

fn capture(mut input: impl Read, limit: usize) -> std::io::Result<Capture> {
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        let keep = n.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..keep]);
        truncated |= keep < n;
    }
    Ok(Capture { text: String::from_utf8_lossy(&bytes).into_owned(), truncated })
}

#[derive(Debug)]
pub struct SearchResult {
    pub engine: &'static str,
    pub scan_complete: bool,
    pub output: Capture,
    pub diagnostics: Capture,
    pub exit_code: Option<i32>,
}

impl SearchResult {
    pub fn render(&self) -> String {
        let mut out = format!(
            "[query engine={} scan_complete={} output_truncated={} freshness=live_observation cache=none retrieval_only=true exit={:?}]\n",
            self.engine, self.scan_complete, self.output.truncated, self.exit_code
        );
        if self.engine == "rg" {
            out.push_str("[scope: ripgrep default ignore/hidden/binary rules with explicit glob overrides, smart case; not an atomic snapshot]\n");
        } else {
            out.push_str("[scope: grep fallback; case-insensitive ERE, build directories excluded; ripgrep ignore/glob/regex parity unavailable]\n");
        }
        if self.output.text.trim().is_empty() {
            out.push_str(if self.scan_complete { "no matches" } else { "no matches observed; search incomplete" });
        } else {
            out.push_str(&self.output.text);
        }
        if !self.diagnostics.text.is_empty() {
            let _ = write!(out, "\n[diagnostics; skipped paths/errors]\n{}", self.diagnostics.text);
        }
        if self.diagnostics.truncated {
            out.push_str("\n[additional diagnostics omitted]");
        }
        out
    }
}

/// Spawn rg directly each time: cached executable availability could outlive a PATH change.
/// Only a missing executable selects fallback; a failed rg query must retain its diagnostics.
pub fn search(pattern: &str, path: &str, glob: Option<&str>, context: u64) -> Result<SearchResult> {
    search_with("rg", pattern, path, glob, context)
}

fn search_with(program: &str, pattern: &str, path: &str, glob: Option<&str>, context: u64) -> Result<SearchResult> {
    let context = context.min(10).to_string();
    let mut rg = Command::new(program);
    rg.args([
        "--no-config",
        "--line-number",
        "--with-filename",
        "--no-heading",
        "--color",
        "never",
        "--smart-case",
        "--sort",
        "path",
        "-C",
        &context,
    ]);
    if let Some(g) = glob {
        rg.args(["-g", g]);
    }
    rg.args(["-e", pattern, "--", path]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let (mut child, engine) = match rg.spawn() {
        Ok(child) => (child, "rg"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut grep = Command::new("grep");
            grep.args(["-rnEIH", "-C", &context]);
            for d in SKIP_DIRS {
                grep.arg(format!("--exclude-dir={d}"));
            }
            if let Some(g) = glob {
                grep.arg(format!("--include={g}"));
            }
            grep.args(["-e", pattern, "--", path]);
            grep.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
            (grep.spawn().context("cannot start grep fallback")?, "grep")
        }
        Err(e) => return Err(e).context("cannot start ripgrep"),
    };
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    // Drain both streams concurrently, including after output fills up.
    let err_reader = std::thread::spawn(move || capture(stderr, DIAGNOSTIC_BYTES));
    let output = capture(stdout, OUTPUT_BYTES);
    if output.is_err() {
        // A failed reader must not leave a child blocked on a full stdout pipe.
        let _ = child.kill();
    }
    let status = child.wait()?;
    let diagnostics = err_reader.join().expect("diagnostic reader panicked")?;
    let exit_code = status.code();
    let scan_complete = engine == "rg" && matches!(exit_code, Some(0 | 1)) && diagnostics.text.is_empty();
    Ok(SearchResult { engine, scan_complete, output: output?, diagnostics, exit_code })
}

#[derive(Default, Debug)]
pub struct Enumeration {
    pub files: Vec<PathBuf>,
    pub incomplete: bool,
    pub diagnostics: Vec<String>,
    pub skipped: usize,
    visited: usize,
}

impl Enumeration {
    pub fn skip(&mut self, path: &Path, reason: impl std::fmt::Display) {
        self.incomplete = true;
        self.skipped += 1;
        if self.diagnostics.len() < 8 {
            self.diagnostics.push(format!("{}: {reason}", path.display()));
        }
    }

    pub fn render(&self, body: &str, output_truncated: bool) -> String {
        let mut out = format!(
            "[query scan_complete={} output_truncated={} freshness=live_observation cache=none retrieval_only=true skipped={}]\n\
             [scope: build/dependency directories excluded; hidden files included; ignore files not applied; descendant directory symlinks skipped; not an atomic snapshot]\n{}",
            !self.incomplete, output_truncated, self.skipped, body
        );
        for diagnostic in &self.diagnostics {
            let _ = write!(out, "\n[skipped] {diagnostic}");
        }
        if self.skipped > self.diagnostics.len() {
            let _ =
                write!(out, "\n[{} additional skipped paths/subtrees omitted]", self.skipped - self.diagnostics.len());
        }
        out
    }
}

pub fn files_under(root: &Path) -> Enumeration {
    let mut scan = Enumeration::default();
    enumerate(root, 0, ENTRY_LIMIT, &mut scan);
    scan
}

fn enumerate(dir: &Path, depth: usize, limit: usize, scan: &mut Enumeration) {
    if depth >= 128 || scan.visited >= limit {
        scan.skip(dir, "enumeration budget reached; subtree not scanned");
        return;
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            scan.skip(dir, e);
            return;
        }
    };
    let mut items = Vec::new();
    for entry in rd {
        // Bound enumeration memory even for a single enormous directory.
        if items.len() >= limit.saturating_sub(scan.visited) {
            scan.skip(dir, "enumeration budget reached; remaining entries not scanned");
            break;
        }
        match entry {
            Ok(entry) => items.push(entry),
            Err(e) => scan.skip(dir, e),
        }
    }
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        let path = entry.path();
        if scan.visited >= limit {
            scan.skip(dir, "enumeration budget reached; remaining entries not scanned");
            break;
        }
        scan.visited += 1;
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                scan.skip(&path, e);
                continue;
            }
        };
        if metadata.is_dir() {
            if SKIP_DIRS.iter().any(|d| entry.file_name() == *d) {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_symlink() => scan.skip(&path, "directory symlink not followed"),
                Ok(_) => enumerate(&path, depth + 1, limit, scan),
                Err(e) => scan.skip(&path, e),
            }
        } else if metadata.is_file() {
            scan.files.push(path);
        } else {
            scan.skip(&path, "not a regular file");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn display_limits_drain_without_changing_scan_completeness() {
        let bytes = [b"start".as_slice(), &vec![b'x'; 100_000]].concat();
        let mut reader = Cursor::new(&bytes);
        let output = capture(&mut reader, 40).unwrap();
        assert_eq!(reader.position(), bytes.len() as u64);
        assert!(output.truncated);
        assert_eq!(output.text.len(), 40);
        let result = SearchResult {
            engine: "rg",
            scan_complete: true,
            output,
            diagnostics: Capture { text: String::new(), truncated: false },
            exit_code: Some(0),
        };
        let rendered = result.render();
        assert!(rendered.contains("scan_complete=true output_truncated=true"));
    }

    #[test]
    fn partial_matches_keep_diagnostics_and_never_claim_complete() {
        let result = SearchResult {
            engine: "rg",
            scan_complete: false,
            output: Capture { text: "good.rs:1:needle".into(), truncated: false },
            diagnostics: Capture { text: "private.rs: Permission denied".into(), truncated: true },
            exit_code: Some(2),
        };
        let rendered = result.render();
        assert!(rendered.contains("scan_complete=false output_truncated=false"));
        assert!(rendered.contains("good.rs:1:needle") && rendered.contains("private.rs: Permission denied"));
        assert!(rendered.contains("additional diagnostics omitted"));
    }

    #[test]
    fn missing_rg_falls_back_without_claiming_semantic_parity() {
        let result = search_with("/rusty/nonexistent-rg", "pub fn search", file!(), None, 0).unwrap();
        assert_eq!(result.engine, "grep");
        assert!(!result.scan_complete);
        assert!(!result.output.text.is_empty());
        assert!(result.render().contains("parity unavailable"));
        let empty = search_with("/rusty/nonexistent-rg", "a^", file!(), None, 0).unwrap();
        assert!(empty.render().contains("no matches observed; search incomplete"));
    }

    #[test]
    fn enumeration_budget_reports_omitted_subtree() {
        let dir = std::env::temp_dir().join(format!("rusty-enumeration-budget-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..4 {
            fs::write(dir.join(format!("{i}.rs")), "").unwrap();
        }
        let mut scan = Enumeration::default();
        enumerate(&dir, 0, 2, &mut scan);
        assert_eq!(scan.files.len(), 2);
        assert!(scan.incomplete && scan.skipped > 0);
        assert!(scan.render("no files observed", false).contains("remaining entries not scanned"));
        fs::remove_dir_all(dir).unwrap();
    }
}
