//! Live, deterministic retrieval. No query results or filesystem metadata are cached.
//! Completeness is relative to the reported visibility policy, never an atomic snapshot.

use anyhow::{Context, Result};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub const SKIP_DIRS: &[&str] =
    &[".git", "target", "node_modules", ".venv", "venv", "__pycache__", "dist", "build", ".next"];
pub const OUTPUT_BYTES: usize = 28_000;
const DIAGNOSTIC_BYTES: usize = 2_000;
const ENTRY_LIMIT: usize = 20_000;
const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// A bounded prefix, drained to EOF so presentation limits never stop the scan.
#[derive(Debug)]
pub struct Capture {
    pub text: String,
    pub truncated: bool,
}

/// No reader threads: nonblocking reads retain only the bounded prefixes.
struct BoundedPipe {
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
    eof: bool,
}

impl BoundedPipe {
    fn new(limit: usize) -> Self {
        Self { bytes: Vec::new(), limit, truncated: false, eof: false }
    }

    fn read_available(&mut self, input: &mut impl Read) -> std::io::Result<()> {
        if self.eof {
            return Ok(());
        }
        let mut buffer = [0; 8192];
        // A continuously writing process must not starve stderr or deadline checks.
        for _ in 0..8 {
            match input.read(&mut buffer) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(n) => {
                    let keep = n.min(self.limit.saturating_sub(self.bytes.len()));
                    self.bytes.extend_from_slice(&buffer[..keep]);
                    self.truncated |= keep < n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn finish(self) -> Capture {
        // Lossy conversion can temporarily expand invalid bytes up to 3x.
        // Bound the retained rendered prefix as well as the raw buffer.
        let mut text = String::from_utf8_lossy(&self.bytes).into_owned();
        let mut truncated = self.truncated;
        if text.len() > self.limit {
            let mut end = self.limit;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            text.truncate(end);
            truncated = true;
        }
        if text.capacity() > self.limit {
            text.shrink_to_fit();
        }
        Capture { text, truncated }
    }
}

fn spawn_query(cmd: &mut Command) -> std::io::Result<Child> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "bounded workspace search requires Unix pipes"))
    }
}

/// Always reap the owned child and kill its ordinary descendants, including error paths.
struct QueryChild {
    child: Child,
    group_cleaned: bool,
}
impl QueryChild {
    fn kill_group(&mut self) -> bool {
        if self.group_cleaned {
            return false;
        }
        self.group_cleaned = true;
        #[cfg(unix)]
        {
            unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) == 0 }
        }
        #[cfg(not(unix))]
        {
            self.child.kill().is_ok()
        }
    }
}
impl Drop for QueryChild {
    fn drop(&mut self) {
        self.kill_group();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(unix)]
fn collect_query_cancellable(
    child: Child,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    use std::os::fd::AsRawFd;
    let mut child = QueryChild { child, group_cleaned: false };
    let mut stdout = child.child.stdout.take().expect("piped stdout");
    let mut stderr = child.child.stderr.take().expect("piped stderr");
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags == -1 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
            return Err(std::io::Error::last_os_error()).context("cannot set query pipe nonblocking");
        }
    }
    let mut output = BoundedPipe::new(OUTPUT_BYTES);
    let mut diagnostics = BoundedPipe::new(DIAGNOSTIC_BYTES);
    let started = Instant::now();
    let mut status = None;
    let mut exited_at = None;
    let mut termination = "exited";
    loop {
        output.read_available(&mut stdout)?;
        diagnostics.read_available(&mut stderr)?;
        if status.is_none() {
            status = child.child.try_wait()?;
            if status.is_some() {
                exited_at = Some(Instant::now());
                if child.kill_group() {
                    termination = "descendants_killed";
                }
            }
        }
        let was_cancelled = cancelled();
        if was_cancelled || started.elapsed() >= timeout {
            termination = if was_cancelled { "cancelled" } else { "deadline" };
            child.kill_group();
            let _ = child.child.kill();
            status = Some(child.child.wait()?);
            break;
        }
        if status.is_some() && output.eof && diagnostics.eof {
            break;
        }
        // An escaped descendant can hold a pipe without belonging to our group.
        // Close our descriptors rather than leave a reader thread behind.
        if exited_at.is_some_and(|at| at.elapsed() >= Duration::from_millis(100)) {
            termination = "inherited_pipes";
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok((output.finish(), diagnostics.finish(), status.and_then(|s| s.code()), termination))
}

#[cfg(not(unix))]
fn collect_query_cancellable(
    child: Child,
    _: Duration,
    _: &dyn Fn() -> bool,
) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    drop(QueryChild { child, group_cleaned: false });
    anyhow::bail!("bounded workspace search requires Unix pipes")
}

#[cfg(test)]
fn collect_query(child: Child, timeout: Duration) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    collect_query_cancellable(child, timeout, &|| false)
}

#[derive(Debug)]
pub struct SearchResult {
    pub engine: &'static str,
    pub scan_complete: bool,
    pub output: Capture,
    pub diagnostics: Capture,
    pub exit_code: Option<i32>,
    pub termination: &'static str,
}

impl SearchResult {
    pub fn render(&self) -> String {
        let mut out = format!(
            "[query engine={} scan_complete={} output_truncated={} freshness=live_observation cache=none retrieval_only=true exit={:?} termination={}]\n",
            self.engine, self.scan_complete, self.output.truncated, self.exit_code, self.termination
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
    search_cancellable(pattern, path, glob, context, || false)
}

/// The caller owns cancellation; this layer creates no global state or polling threads.
pub fn search_cancellable(
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    context: u64,
    cancelled: impl Fn() -> bool,
) -> Result<SearchResult> {
    search_with("rg", pattern, path, glob, context, &cancelled)
}

fn search_with(
    program: &str,
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    context: u64,
    cancelled: &dyn Fn() -> bool,
) -> Result<SearchResult> {
    if cancelled() {
        return Ok(SearchResult {
            engine: "rg",
            scan_complete: false,
            output: BoundedPipe::new(0).finish(),
            diagnostics: BoundedPipe::new(0).finish(),
            exit_code: None,
            termination: "cancelled",
        });
    }
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
    let (child, engine) = match spawn_query(&mut rg) {
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
            (spawn_query(&mut grep).context("cannot start grep fallback")?, "grep")
        }
        Err(e) => return Err(e).context("cannot start ripgrep"),
    };
    let (output, diagnostics, exit_code, termination) = collect_query_cancellable(child, QUERY_TIMEOUT, cancelled)?;
    let scan_complete =
        engine == "rg" && termination == "exited" && matches!(exit_code, Some(0 | 1)) && diagnostics.text.is_empty();
    Ok(SearchResult { engine, scan_complete, output, diagnostics, exit_code, termination })
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
        // Charge discovery globally, bounding pending vectors across nested traversal.
        if scan.visited >= limit {
            scan.skip(dir, "enumeration budget reached; remaining entries not scanned");
            break;
        }
        scan.visited += 1;
        match entry {
            Ok(entry) => items.push(entry),
            Err(e) => scan.skip(dir, e),
        }
    }
    items.sort_by_key(|e| e.file_name());
    for entry in items {
        let path = entry.path();
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
        let mut bounded = BoundedPipe::new(40);
        while !bounded.eof {
            bounded.read_available(&mut reader).unwrap();
            assert!(bounded.bytes.len() <= 40);
        }
        let output = bounded.finish();
        assert_eq!(reader.position(), bytes.len() as u64);
        assert!(output.truncated);
        assert_eq!(output.text.len(), 40);
        let result = SearchResult {
            engine: "rg",
            scan_complete: true,
            output,
            diagnostics: Capture { text: String::new(), truncated: false },
            exit_code: Some(0),
            termination: "exited",
        };
        let rendered = result.render();
        assert!(rendered.contains("scan_complete=true output_truncated=true"));
    }

    #[test]
    fn invalid_utf8_cannot_expand_retained_output_beyond_the_cap() {
        let mut reader = Cursor::new(vec![0xff; 100_000]);
        let mut bounded = BoundedPipe::new(17);
        while !bounded.eof {
            bounded.read_available(&mut reader).unwrap();
            assert!(bounded.bytes.len() <= 17);
        }
        assert_eq!(reader.position(), 100_000);
        let result = bounded.finish();
        assert_eq!(result.text, "\u{fffd}".repeat(5));
        assert!(result.text.len() <= 17 && result.text.capacity() <= 17 && result.truncated);
        let mut short = BoundedPipe::new(100);
        short.read_available(&mut Cursor::new(vec![0xff; 30])).unwrap();
        let result = short.finish();
        assert_eq!(result.text.len(), 90);
        assert!(result.text.capacity() <= 100 && !result.truncated);
    }

    #[test]
    fn partial_matches_keep_diagnostics_and_never_claim_complete() {
        let result = SearchResult {
            engine: "rg",
            scan_complete: false,
            output: Capture { text: "good.rs:1:needle".into(), truncated: false },
            diagnostics: Capture { text: "private.rs: Permission denied".into(), truncated: true },
            exit_code: Some(2),
            termination: "exited",
        };
        let rendered = result.render();
        assert!(rendered.contains("scan_complete=false output_truncated=false"));
        assert!(rendered.contains("good.rs:1:needle") && rendered.contains("private.rs: Permission denied"));
        assert!(rendered.contains("additional diagnostics omitted"));
    }

    #[test]
    fn missing_rg_falls_back_without_claiming_semantic_parity() {
        let result = search_with("/rusty/nonexistent-rg", "pub fn search", file!(), None, 0, &|| false).unwrap();
        assert_eq!(result.engine, "grep");
        assert!(!result.scan_complete);
        assert!(!result.output.text.is_empty());
        assert!(result.render().contains("parity unavailable"));
        let empty = search_with("/rusty/nonexistent-rg", "a^", file!(), None, 0, &|| false).unwrap();
        assert!(empty.render().contains("no matches observed; search incomplete"));
    }

    #[test]
    #[cfg(unix)]
    fn deadline_reaps_a_chatty_child_with_bounded_retention() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "while :; do printf '%1000s\\n' x; printf '%1000s\\n' y >&2; done"]);
        let child = spawn_query(&mut cmd).unwrap();
        let pid = child.id();
        let started = Instant::now();
        let (out, err, _, reason) = collect_query(child, Duration::from_millis(200)).unwrap();
        assert_eq!(reason, "deadline");
        assert!(started.elapsed() < Duration::from_secs(10));
        // Both retained raw and rendered prefixes have strict byte limits.
        assert!(out.text.len() <= OUTPUT_BYTES && err.text.len() <= DIAGNOSTIC_BYTES);
        assert_eq!(unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        let result = SearchResult {
            engine: "rg",
            scan_complete: false,
            output: out,
            diagnostics: err,
            exit_code: None,
            termination: reason,
        };
        assert!(result.render().contains("termination=deadline"));
        assert!(result.render().contains("scan_complete=false"));
    }

    #[test]
    #[cfg(unix)]
    fn caller_cancellation_reaps_the_owned_child() {
        let before = search_with("/rusty/nonexistent-rg", "needle", ".", None, 0, &|| true).unwrap();
        assert_eq!(before.termination, "cancelled");
        assert!(!before.scan_complete && before.output.text.is_empty());
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "printf partial; sleep 30"]);
        let child = spawn_query(&mut cmd).unwrap();
        let pid = child.id();
        let started = Instant::now();
        let (_, _, _, reason) = collect_query_cancellable(child, Duration::from_secs(5), &|| {
            started.elapsed() >= Duration::from_millis(50)
        })
        .unwrap();
        assert_eq!(reason, "cancelled");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    #[cfg(unix)]
    fn exited_child_cannot_leave_descendants_holding_output_pipes() {
        for _ in 0..5 {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 30 & printf done"]);
            let child = spawn_query(&mut cmd).unwrap();
            let pid = child.id();
            let started = Instant::now();
            let (out, _, code, reason) = collect_query(child, Duration::from_secs(2)).unwrap();
            assert_eq!(out.text, "done");
            assert_eq!(code, Some(0));
            assert_eq!(reason, "descendants_killed");
            assert!(started.elapsed() < Duration::from_secs(10));
            assert_eq!(unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
        }
    }

    #[test]
    #[cfg(unix)]
    fn escaped_pipe_holder_does_not_leave_a_reader_thread_behind() {
        if Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let mut cmd = Command::new("python3");
        cmd.args([
            "-c",
            r#"import os,time
r,w=os.pipe()
pid=os.fork()
if pid == 0:
    os.close(r)
    os.setsid()
    os.write(w,b'ready')
    os.close(w)
    time.sleep(30)
    os._exit(0)
os.close(w)
os.read(r,5)
print(pid,flush=True)
os._exit(0)
"#,
        ]);
        let child = spawn_query(&mut cmd).unwrap();
        let started = Instant::now();
        let (out, _, _, reason) = collect_query(child, Duration::from_secs(5)).unwrap();
        let escaped: i32 = out.text.trim().parse().unwrap();
        // Escaping our group is outside containment; this fixture owns that process.
        unsafe {
            libc::kill(escaped, libc::SIGKILL);
        }
        assert_eq!(reason, "inherited_pipes");
        assert!(started.elapsed() < Duration::from_secs(10));
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
