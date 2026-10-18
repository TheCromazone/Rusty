//! Live retrieval with bounded, sorted presentation. No query results or filesystem metadata are cached.
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
pub const RENDER_BYTES: usize = 30_000;
const DIAGNOSTIC_BYTES: usize = 2_000;
const MATCHES_PER_FILE: usize = 30;
const MAX_COLUMNS: usize = 300;
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
        self.read_observed(input, &mut |_| {})
    }

    fn read_observed(&mut self, input: &mut impl Read, observe: &mut impl FnMut(&[u8])) -> std::io::Result<()> {
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
                    observe(&buffer[..n]);
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
        // Group ownership requires that no other handler can reap this child.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
            return Err(std::io::Error::other("query child requires exclusive SIGCHLD ownership"));
        }
        cmd.process_group(0);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "bounded workspace search requires Unix pipes"))
    }
}

/// Abnormal cleanup signals only an owned, unreaped process group.
struct QueryChild {
    child: Child,
    group_cleaned: bool,
    reaped: bool,
}
impl QueryChild {
    fn kill_group(&mut self) -> bool {
        if self.group_cleaned || self.reaped {
            return false;
        }
        #[cfg(unix)]
        {
            // Like the SSH transport, observe ownership without releasing the PID.
            loop {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let result = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        self.child.id() as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                if result == 0 {
                    break;
                }
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    self.reaped = true; // Never signal after ECHILD or uncertain ownership.
                    return false;
                }
            }
            self.group_cleaned = true;
            let killed = unsafe { libc::kill(-(self.child.id() as i32), libc::SIGKILL) == 0 };
            if !killed {
                let _ = self.child.kill(); // Ownership was verified and the PID is still unreaped.
            }
            killed
        }
        #[cfg(not(unix))]
        {
            self.group_cleaned = true;
            self.child.kill().is_ok()
        }
    }
}
impl Drop for QueryChild {
    fn drop(&mut self) {
        if !self.reaped {
            self.kill_group();
            if !self.reaped {
                let _ = self.child.wait();
            }
        }
    }
}

/// `observe` sees every byte of stdout, including bytes past the retained prefix.
#[cfg(unix)]
fn collect_query_cancellable(
    child: Child,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
    observe: &mut dyn FnMut(&[u8]),
) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    use std::os::fd::AsRawFd;
    let mut child = QueryChild { child, group_cleaned: false, reaped: false };
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
        output.read_observed(&mut stdout, &mut |bytes: &[u8]| observe(bytes))?;
        diagnostics.read_available(&mut stderr)?;
        if status.is_none() {
            status = child.child.try_wait()?;
            if status.is_some() {
                child.reaped = true;
                exited_at = Some(Instant::now());
            }
        }
        let was_cancelled = cancelled();
        if was_cancelled || started.elapsed() >= timeout {
            termination = if was_cancelled { "cancelled" } else { "deadline" };
            if status.is_none() {
                child.kill_group();
                anyhow::ensure!(!child.reaped, "query child ownership lost before cleanup");
                status = Some(child.child.wait()?);
                child.reaped = true;
            }
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
    _: &mut dyn FnMut(&[u8]),
) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    drop(QueryChild { child, group_cleaned: false, reaped: false });
    anyhow::bail!("bounded workspace search requires Unix pipes")
}

#[cfg(test)]
fn collect_query(child: Child, timeout: Duration) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    collect_query_cancellable(child, timeout, &|| false, &mut |_| {})
}

#[derive(Debug)]
pub struct SearchResult {
    pub engine: &'static str,
    pub scan_complete: bool,
    pub audit_complete: bool,
    pub match_limit_possible: bool,
    pub column_limit_possible: bool,
    pub output: Capture,
    pub diagnostics: Capture,
    pub exit_code: Option<i32>,
    pub termination: &'static str,
}

impl SearchResult {
    pub fn render(&self) -> String {
        let header = format!(
            "[query engine={} scan_complete={} output_truncated={} freshness=live_observation cache=none retrieval_only=true exit={:?} termination={}]\n",
            self.engine, self.scan_complete, self.output.truncated, self.exit_code, self.termination
        );
        let scope = if self.engine == "rg" {
            format!("[scope: ripgrep default ignore/hidden/binary rules with explicit glob overrides, smart case; max_count={MATCHES_PER_FILE} max_columns={MAX_COLUMNS} audit_complete={} match_limit_possible={} column_limit_possible={}; sorted retained records, not a stable truncated subset or atomic snapshot]\n", self.audit_complete, self.match_limit_possible, self.column_limit_possible)
        } else {
            "[scope: grep fallback; case-insensitive ERE, build directories excluded; ripgrep ignore/glob/regex parity unavailable]\n".into()
        };
        let body = if self.output.text.trim().is_empty() {
            if self.scan_complete {
                "no matches"
            } else {
                "no matches observed; search incomplete"
            }
        } else {
            &self.output.text
        };
        let mut diagnostics = String::new();
        if !self.diagnostics.text.is_empty() {
            let _ = write!(diagnostics, "\n[diagnostics; skipped paths/errors]\n{}", self.diagnostics.text);
        }
        if self.diagnostics.truncated {
            diagnostics.push_str("\n[additional diagnostics omitted]");
        }
        render_bounded(header, &scope, body, &diagnostics)
    }
}

fn prefix(text: &str, limit: usize) -> &str {
    let mut end = limit.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Cap the entire response while retaining completeness flags and diagnostics.
fn render_bounded(mut header: String, scope: &str, body: &str, diagnostics: &str) -> String {
    let clipped_diagnostics = diagnostics.len() > DIAGNOSTIC_BYTES;
    let diagnostics = if clipped_diagnostics {
        format!("{}\n[additional diagnostics omitted]", prefix(diagnostics, DIAGNOSTIC_BYTES))
    } else {
        diagnostics.to_owned()
    };
    if header.len() + scope.len() + body.len() + diagnostics.len() > RENDER_BYTES || clipped_diagnostics {
        header = header.replacen("output_truncated=false", "output_truncated=true", 1);
    }
    let budget = RENDER_BYTES.saturating_sub(header.len() + scope.len() + diagnostics.len());
    format!("{header}{scope}{}{diagnostics}", prefix(body, budget))
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
            audit_complete: false,
            match_limit_possible: false,
            column_limit_possible: false,
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
        "--null",
        "--no-context-separator",
        "--max-count",
        "30",
        "--max-columns",
        "300",
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
            grep.args(["-rnEiIH", "-C", &context]);
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
    let mut audit = RgAudit::new();
    let (output, diagnostics, exit_code, termination) =
        collect_query_cancellable(child, QUERY_TIMEOUT, cancelled, &mut |bytes| {
            if engine == "rg" {
                audit.feed(bytes);
            }
        })?;
    audit.complete &= audit.pending.is_empty();
    let audit_complete = engine == "rg" && audit.complete;
    let match_limit_possible = audit.match_limit_possible;
    let column_limit_possible = audit.column_limit_possible;
    let mut output = if engine == "rg" { present_rg(output) } else { output };
    output.truncated |= match_limit_possible || column_limit_possible;
    let scan_complete = engine == "rg"
        && termination == "exited"
        && matches!(exit_code, Some(0 | 1))
        && diagnostics.text.is_empty()
        && audit_complete
        && !match_limit_possible;
    Ok(SearchResult {
        engine,
        scan_complete,
        audit_complete,
        match_limit_possible,
        column_limit_possible,
        output,
        diagnostics,
        exit_code,
        termination,
    })
}

/// Audit every drained frame independently of the retained display prefix.
/// Both an unfinished frame and retained per-file keys have a fixed byte budget.
struct RgAudit {
    pending: Vec<u8>,
    path_end: Option<usize>,
    dropping: bool,
    counts: std::collections::BTreeMap<Vec<u8>, usize>,
    key_bytes: usize,
    complete: bool,
    match_limit_possible: bool,
    column_limit_possible: bool,
}
impl RgAudit {
    fn new() -> Self {
        Self {
            pending: Vec::with_capacity(OUTPUT_BYTES),
            path_end: None,
            dropping: false,
            counts: std::collections::BTreeMap::new(),
            key_bytes: 0,
            complete: true,
            match_limit_possible: false,
            column_limit_possible: false,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.pending.len() < OUTPUT_BYTES && !self.dropping {
                self.pending.push(byte);
            } else {
                self.dropping = true;
                self.complete = false;
            }
            if self.path_end.is_none() && byte == 0 {
                self.path_end = Some(self.pending.len());
            } else if self.path_end.is_some() && byte == b'\n' {
                if !self.dropping {
                    if let Some((path, _, matching, content)) = rg_frame(&self.pending[..self.pending.len() - 1]) {
                        self.column_limit_possible |= content.starts_with(b"[Omitted ");
                        if matching {
                            if let Some(count) = self.counts.get_mut(path) {
                                *count = (*count + 1).min(MATCHES_PER_FILE);
                                self.match_limit_possible |= *count >= MATCHES_PER_FILE;
                            } else if self.counts.len() < ENTRY_LIMIT && self.key_bytes + path.len() <= OUTPUT_BYTES {
                                self.key_bytes += path.len();
                                self.counts.insert(path.to_vec(), 1);
                            } else {
                                self.complete = false;
                            }
                        }
                    } else {
                        self.complete = false;
                    }
                }
                self.pending.clear();
                self.path_end = None;
                self.dropping = false;
            }
        }
    }
}

fn rg_frame(frame: &[u8]) -> Option<(&[u8], u64, bool, &[u8])> {
    let nul = frame.iter().position(|&byte| byte == 0)?;
    if nul == 0 {
        return None;
    }
    let line = &frame[nul + 1..];
    let sep = line.iter().position(|&byte| byte == b':' || byte == b'-')?;
    let number = std::str::from_utf8(&line[..sep]).ok()?.parse().ok()?;
    if number == 0 {
        return None;
    }
    Some((&frame[..nul], number, line[sep] == b':', &line[sep + 1..]))
}

/// Sort only complete retained records, recovering after malformed line numbers.
/// Full-stream audit, rather than display truncation, decides scan completeness.
fn present_rg(raw: Capture) -> Capture {
    let mut records = Vec::new();
    let mut truncated = raw.truncated;
    let mut rest = raw.text.as_str();
    while !rest.is_empty() {
        let Some((path, body)) = rest.split_once('\0') else {
            truncated = true;
            break;
        };
        let Some((line, tail)) = body.split_once('\n') else {
            truncated = true;
            break;
        };
        let frame_len = path.len() + 1 + line.len();
        let parsed = rg_frame(&rest.as_bytes()[..frame_len]);
        rest = tail;
        if let Some((_, number, matching, _)) = parsed {
            records.push((path, number, matching, line));
        } else {
            truncated = true;
        }
    }
    records.sort_unstable_by_key(|&(path, number, matching, line)| (path, number, matching, line));
    let mut text = String::with_capacity(raw.text.len());
    for (path, _, matching, line) in records {
        let _ = writeln!(text, "{path}{}{line}", if matching { ':' } else { '-' });
    }
    Capture { text, truncated }
}

#[derive(Default, Debug)]
pub struct Enumeration {
    pub files: Vec<PathBuf>,
    pub incomplete: bool,
    pub diagnostics: Vec<String>,
    pub skipped: usize,
    visited: usize,
    /// Listed by ripgrep's rules (`search_files`) rather than glob's.
    search_rules: bool,
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
        if !self.incomplete
            && self.diagnostics.is_empty()
            && self.skipped == 0
            && !output_truncated
            && body.len() <= RENDER_BYTES
        {
            return body.to_owned();
        }
        let header = format!(
            "[query scan_complete={} output_truncated={} freshness=live_observation cache=none retrieval_only=true skipped={}]\n",
            !self.incomplete, output_truncated, self.skipped
        );
        let scope = if self.search_rules {
            "[scope: ripgrep's hidden and ignore rules, as search applies them; not an atomic snapshot]\n"
        } else {
            "[scope: build/dependency directories excluded; hidden files included; ignore files not applied; descendant directory symlinks skipped; not an atomic snapshot]\n"
        };
        let mut diagnostics = String::new();
        for diagnostic in &self.diagnostics {
            let _ = write!(diagnostics, "\n[skipped] {diagnostic}");
        }
        if self.skipped > self.diagnostics.len() {
            let _ = write!(
                diagnostics,
                "\n[{} additional skipped paths/subtrees omitted]",
                self.skipped - self.diagnostics.len()
            );
        }
        render_bounded(header, scope, body, &diagnostics)
    }
}

/// The files a search of `path` would read, by ripgrep's own hidden and
/// ignore rules (`rg --files`), for a tool that reads and ranks content
/// itself. It runs under the same deadline and process ownership as search
/// and keeps at most 20,000 paths. Without ripgrep it falls back to
/// [`files_under`] and says so, rather than pass glob's rules off as search's.
pub fn search_files(path: &Path) -> Enumeration {
    let mut scan = Enumeration { search_rules: true, ..Enumeration::default() };
    let mut rg = Command::new("rg");
    rg.args(["--no-config", "--files", "--null", "--"]).arg(path);
    let child = match spawn_query(&mut rg) {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut scan = files_under(path);
            scan.skip(path, "ripgrep not found; listed by glob's rules, ignore files not applied");
            return scan;
        }
        Err(e) => {
            scan.skip(path, format!("cannot start ripgrep: {e}"));
            return scan;
        }
    };
    let (mut name, mut full) = (Vec::new(), false);
    let files = &mut scan.files;
    let listed = collect_query_cancellable(child, QUERY_TIMEOUT, &|| false, &mut |bytes| {
        for &b in bytes {
            if b != 0 {
                if !full {
                    name.push(b);
                }
                continue;
            }
            if files.len() < ENTRY_LIMIT {
                files.push(PathBuf::from(String::from_utf8_lossy(&name).into_owned()));
            } else {
                full = true;
            }
            name.clear();
        }
    });
    match listed {
        Err(e) => scan.skip(path, e),
        Ok((_, diagnostics, code, termination)) => {
            if full {
                scan.skip(path, "listing budget reached; remaining files not listed");
            }
            if termination != "exited" || !matches!(code, Some(0 | 1)) {
                scan.skip(path, format!("ripgrep listing ended: {termination}, exit {code:?}"));
            }
            if let Some(line) = diagnostics.text.lines().next() {
                scan.skip(path, line);
            }
        }
    }
    scan
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
            audit_complete: true,
            match_limit_possible: false,
            column_limit_possible: false,
            output,
            diagnostics: Capture { text: String::new(), truncated: false },
            exit_code: Some(0),
            termination: "exited",
        };
        let rendered = result.render();
        assert!(rendered.contains("scan_complete=true output_truncated=true"));
    }

    #[test]
    fn presentation_frames_paths_and_sorts_numeric_lines_with_context() {
        let raw = Capture {
            text: "z:part\nname\0".to_owned() + "10:needle\n" + "a.rs\0" + "11-context\n" + "a.rs\0" + "2:needle\n",
            truncated: false,
        };
        let mut audit = RgAudit::new();
        audit.feed(raw.text.as_bytes());
        let shown = present_rg(raw);
        assert_eq!(shown.text, "a.rs:2:needle\na.rs-11-context\nz:part\nname:10:needle\n");
        assert!(!shown.truncated && audit.complete && !audit.match_limit_possible && !audit.column_limit_possible);
        let partial = present_rg(Capture { text: "partial-path".into(), truncated: false });
        assert!(partial.truncated && partial.text.is_empty());
    }

    #[test]
    fn full_stream_audit_is_independent_of_display_truncation() {
        let mut bytes = Vec::new();
        for file in 0..100 {
            for line in 1..=20 {
                bytes.extend_from_slice(format!("file{file}\0{line}:needle\n").as_bytes());
            }
        }
        let mut audit = RgAudit::new();
        let mut retained = BoundedPipe::new(40);
        let mut input = Cursor::new(&bytes);
        while !retained.eof {
            retained.read_observed(&mut input, &mut |bytes| audit.feed(bytes)).unwrap();
        }
        let shown = present_rg(retained.finish());
        assert!(shown.truncated && audit.complete && audit.pending.is_empty());
        assert!(!audit.match_limit_possible && !audit.column_limit_possible);
        // A file cap after the display prefix must still be observed.
        for line in 1..=MATCHES_PER_FILE {
            audit.feed(format!("later\0{line}:needle\n").as_bytes());
        }
        assert!(audit.match_limit_possible);
        assert!(audit.pending.capacity() <= OUTPUT_BYTES && audit.key_bytes <= OUTPUT_BYTES);
    }

    #[test]
    fn malformed_frames_preserve_later_records_and_make_audit_incomplete() {
        let raw = "bad.rs\0not-a-number:bad\ngood.rs\0".to_owned() + "2:needle\n";
        let mut audit = RgAudit::new();
        for bytes in raw.as_bytes().chunks(3) {
            audit.feed(bytes);
        }
        let shown = present_rg(Capture { text: raw, truncated: false });
        assert_eq!(shown.text, "good.rs:2:needle\n");
        assert!(shown.truncated && !audit.complete && !audit.match_limit_possible);
        // Oversized frames/too many path bytes are an audit limit, never a match cap.
        let mut bounded = RgAudit::new();
        bounded.feed(format!("{}\0x:bad\nnext\0", "p".repeat(OUTPUT_BYTES + 1)).as_bytes());
        bounded.feed(b"1:needle\n");
        assert!(!bounded.complete && !bounded.match_limit_possible && bounded.pending.is_empty());
        assert!(bounded.pending.capacity() <= OUTPUT_BYTES);
        for file in 0..1000 {
            bounded.feed(format!("{file:04}{}\0", "p".repeat(100)).as_bytes());
            bounded.feed(b"1:needle\n");
        }
        assert!(bounded.key_bytes <= OUTPUT_BYTES && !bounded.complete && !bounded.match_limit_possible);
    }

    #[test]
    fn rendered_bound_retains_completeness_flags_and_diagnostics() {
        let result = SearchResult {
            engine: "rg",
            scan_complete: false,
            audit_complete: true,
            match_limit_possible: false,
            column_limit_possible: false,
            output: Capture { text: "é".repeat(OUTPUT_BYTES / 2), truncated: false },
            diagnostics: Capture { text: "error".repeat(DIAGNOSTIC_BYTES), truncated: false },
            exit_code: Some(2),
            termination: "exited",
        };
        let shown = result.render();
        assert!(shown.len() <= RENDER_BYTES && shown.is_char_boundary(shown.len()));
        assert!(shown.starts_with("[query engine=rg scan_complete=false output_truncated=true"));
        assert!(shown.contains("[diagnostics;") && shown.contains("[additional diagnostics omitted]"));
        assert_eq!(Enumeration::default().render("small result", false), "small result");
        let mut scan = Enumeration::default();
        scan.skip(Path::new(&"p".repeat(OUTPUT_BYTES)), "unreadable");
        let shown = scan.render(&"x".repeat(OUTPUT_BYTES), false);
        assert!(shown.len() <= RENDER_BYTES && shown.contains("scan_complete=false output_truncated=true"));
    }

    #[test]
    #[cfg(unix)]
    fn search_keeps_parallel_traversal_and_engine_output_bounds() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rusty-query-argv-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("rg-fixture");
        fs::write(
            &script,
            r#"#!/bin/sh
case " $* " in *" --sort "*) exit 64;; esac
case " $* " in *" --max-count 30 "*) :;; *) exit 65;; esac
case " $* " in *" --max-columns 300 "*) :;; *) exit 66;; esac
case " $* " in *" --null "*) :;; *) exit 67;; esac
printf 'z.rs\000%s\na.rs\000%s\n' '10:needle' '2:needle'
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let result = search_with(script.to_str().unwrap(), "needle", ".", None, 0, &|| false);
        fs::remove_dir_all(dir).unwrap();
        let result = result.unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert!(result.scan_complete && !result.output.truncated);
        assert_eq!(result.output.text, "a.rs:2:needle\nz.rs:10:needle\n");
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
            audit_complete: false,
            match_limit_possible: false,
            column_limit_possible: false,
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
        let result =
            search_with("/rusty/nonexistent-rg", &"pub fn search".to_uppercase(), file!(), None, 0, &|| false).unwrap();
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
            audit_complete: false,
            match_limit_possible: false,
            column_limit_possible: false,
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
        let (_, _, _, reason) = collect_query_cancellable(
            child,
            Duration::from_secs(5),
            &|| started.elapsed() >= Duration::from_millis(50),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(reason, "cancelled");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert_eq!(unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) }, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
    }

    #[test]
    #[cfg(unix)]
    fn cleanup_refuses_to_signal_an_already_reaped_group() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "exit 0"]);
        let child = spawn_query(&mut cmd).unwrap();
        let mut owned = QueryChild { child, group_cleaned: false, reaped: false };
        owned.child.wait().unwrap();
        assert!(!owned.kill_group());
        assert!(owned.reaped && !owned.group_cleaned);
        // Drop must also skip group and individual PID signals in this state.
        drop(owned);
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_and_deadline_after_normal_reap_preserve_status() {
        for cancel in [false, true] {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "sleep 0.3 & printf done"]);
            let child = spawn_query(&mut cmd).unwrap();
            let until = Instant::now() + Duration::from_secs(2);
            // Observe normal exit without reaping, so the collector's first poll
            // deterministically reaps before the cancellation/deadline check.
            loop {
                let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
                let observed = unsafe {
                    libc::waitid(
                        libc::P_PID,
                        child.id() as libc::id_t,
                        &mut info,
                        libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                    )
                };
                assert_eq!(observed, 0);
                if unsafe { info.si_pid() } != 0 {
                    break;
                }
                assert!(Instant::now() < until);
                std::thread::sleep(Duration::from_millis(1));
            }
            let timeout = if cancel { Duration::from_secs(5) } else { Duration::ZERO };
            let (out, _, status, reason) = collect_query_cancellable(child, timeout, &|| cancel, &mut |_| {}).unwrap();
            assert_eq!(status, Some(0));
            assert_eq!(out.text, "done");
            assert_eq!(reason, if cancel { "cancelled" } else { "deadline" });
            std::thread::sleep(Duration::from_millis(350));
        }
    }

    #[test]
    #[cfg(unix)]
    fn reaped_child_with_inherited_pipes_is_not_signalled_again() {
        for _ in 0..5 {
            let mut cmd = Command::new("sh");
            // The fixture holder owns a finite lifetime; normal exit never signals
            // a released PGID. It may keep the pipe open beyond our grace period.
            cmd.args(["-c", "sleep 0.3 & printf done"]);
            let child = spawn_query(&mut cmd).unwrap();
            let pid = child.id();
            let started = Instant::now();
            let (out, _, code, reason) = collect_query(child, Duration::from_secs(2)).unwrap();
            assert_eq!(out.text, "done");
            assert_eq!(code, Some(0));
            assert_eq!(reason, "inherited_pipes");
            assert!(started.elapsed() < Duration::from_secs(10));
            assert_eq!(unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) }, -1);
            assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ECHILD));
            std::thread::sleep(Duration::from_millis(350));
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
