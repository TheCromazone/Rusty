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

#[cfg(unix)]
fn collect_query_cancellable(
    child: Child,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
    mut audit: Option<&mut RgAudit>,
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
        output.read_observed(&mut stdout, &mut |bytes| {
            if let Some(audit) = audit.as_deref_mut() {
                audit.feed(bytes);
            }
        })?;
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
    _: Option<&mut RgAudit>,
) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    drop(QueryChild { child, group_cleaned: false, reaped: false });
    anyhow::bail!("bounded workspace search requires Unix pipes")
}

#[cfg(test)]
fn collect_query(child: Child, timeout: Duration) -> Result<(Capture, Capture, Option<i32>, &'static str)> {
    collect_query_cancellable(child, timeout, &|| false, None)
}

/// What a search reports: matching lines with optional context, only the
/// names of the files that match, or how many matches each file has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Content,
    Files,
    Count,
}

impl Mode {
    pub fn parse(name: Option<&str>) -> Result<Self> {
        match name.unwrap_or("") {
            "" | "content" => Ok(Self::Content),
            "files_with_matches" => Ok(Self::Files),
            "count" => Ok(Self::Count),
            other => anyhow::bail!("unknown output_mode `{other}`; use content, files_with_matches or count"),
        }
    }
}

#[derive(Debug)]
pub struct SearchResult {
    pub engine: &'static str,
    pub mode: Mode,
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
        let scope = match (self.engine, self.mode) {
            ("rg", Mode::Content) => format!("[scope: ripgrep default ignore/hidden/binary rules with explicit glob overrides, smart case; max_count={MATCHES_PER_FILE} max_columns={MAX_COLUMNS} audit_complete={} match_limit_possible={} column_limit_possible={}; sorted retained records, not a stable truncated subset or atomic snapshot]\n", self.audit_complete, self.match_limit_possible, self.column_limit_possible),
            ("rg", Mode::Files) => "[scope: ripgrep default ignore/hidden/binary rules with explicit glob overrides, smart case; names of matching files, most recently modified first]\n".into(),
            ("rg", Mode::Count) => "[scope: ripgrep default ignore/hidden/binary rules with explicit glob overrides, smart case; matches per file, sorted by path]\n".into(),
            ("git-grep", _) => "[scope: git grep fallback; tracked and untracked files with .gitignore applied, smart case ERE; ripgrep hidden/glob/regex parity unavailable]\n".into(),
            _ => "[scope: grep fallback; smart case ERE, build directories excluded, ignore files not applied; ripgrep ignore/glob/regex parity unavailable]\n".into(),
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
pub fn search(pattern: &str, path: &str, glob: Option<&str>, context: u64, mode: Mode) -> Result<SearchResult> {
    search_cancellable(pattern, path, glob, context, mode, || false)
}

/// The caller owns cancellation; this layer creates no global state or polling threads.
pub fn search_cancellable(
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    context: u64,
    mode: Mode,
    cancelled: impl Fn() -> bool,
) -> Result<SearchResult> {
    search_with("rg", pattern, path, glob, context, mode, &cancelled)
}

fn search_with(
    program: &str,
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    context: u64,
    mode: Mode,
    cancelled: &dyn Fn() -> bool,
) -> Result<SearchResult> {
    if cancelled() {
        return Ok(SearchResult {
            engine: "rg",
            mode,
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
    rg.args(["--no-config", "--color", "never", "--smart-case", "--null"]);
    match mode {
        Mode::Content => rg.args([
            "--line-number",
            "--with-filename",
            "--no-heading",
            "--no-context-separator",
            "--max-count",
            "30",
            "--max-columns",
            "300",
            "-C",
            &context,
        ]),
        Mode::Files => rg.arg("--files-with-matches"),
        Mode::Count => rg.args(["--count-matches", "--with-filename"]),
    };
    if let Some(g) = glob {
        rg.args(["-g", g]);
    }
    rg.args(["-e", pattern, "--", path]);
    let child = match spawn_query(&mut rg) {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fallback(pattern, path, glob, &context, mode, cancelled)
        }
        Err(e) => return Err(e).context("cannot start ripgrep"),
    };
    // Only line output has per-file match and column limits to audit.
    let mut audit = RgAudit::new();
    let (output, diagnostics, exit_code, termination) =
        collect_query_cancellable(child, QUERY_TIMEOUT, cancelled, (mode == Mode::Content).then_some(&mut audit))?;
    audit.complete &= audit.pending.is_empty();
    let audit_complete = audit.complete;
    let match_limit_possible = audit.match_limit_possible;
    let column_limit_possible = audit.column_limit_possible;
    let mut output = match mode {
        Mode::Content => present_rg(output),
        Mode::Files => present_files(output, '\0'),
        Mode::Count => present_counts(output, '\0', false),
    };
    output.truncated |= match_limit_possible || column_limit_possible;
    let scan_complete = termination == "exited"
        && matches!(exit_code, Some(0 | 1))
        && diagnostics.text.is_empty()
        && audit_complete
        && !match_limit_possible;
    Ok(SearchResult {
        engine: "rg",
        mode,
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

/// Without ripgrep: `git grep` inside the repository around the working
/// directory, so ignored files stay out as they do with rg, else plain grep.
/// Both use rg's smart case. Neither has rg's exact semantics, so neither
/// reports a complete scan.
fn fallback(
    pattern: &str,
    path: &str,
    glob: Option<&str>,
    context: &str,
    mode: Mode,
    cancelled: &dyn Fn() -> bool,
) -> Result<SearchResult> {
    let ignore_case = !pattern.chars().any(char::is_uppercase);
    let flags = |cmd: &mut Command| {
        if ignore_case {
            cmd.arg("-i");
        }
        match mode {
            Mode::Content => cmd.args(["-n", "-C", context]),
            Mode::Files => cmd.arg("-l"),
            Mode::Count => cmd.arg("-c"),
        };
    };
    if Path::new(path).is_dir() && in_cwd_repo(Path::new(path)) {
        let mut git = Command::new("git");
        // Pin the settings that change the output's shape.
        git.args([
            "-c",
            "grep.fullName=false",
            "-c",
            "grep.column=false",
            "-c",
            "core.quotePath=false",
            "grep",
            "--no-color",
            "--untracked",
            "-I",
            "-E",
        ]);
        flags(&mut git);
        // A NUL after each name keeps any name intact. In line output it also
        // replaces the `:`/`-` that tells a match from context, so with context
        // the names come unquoted (quotePath off) but newline-delimited.
        let nul = mode != Mode::Content || context == "0";
        if nul {
            git.arg("-z");
        }
        git.args(["-e", pattern, "--"]).args(pathspecs(path, glob));
        match spawn_query(&mut git) {
            Ok(child) => {
                let (output, diagnostics, exit_code, termination) =
                    collect_query_cancellable(child, QUERY_TIMEOUT, cancelled, None)?;
                // 128 is git refusing to search at all, as in a repository it
                // won't read; grep can still try.
                if exit_code != Some(128) {
                    let output = if nul && mode == Mode::Content { from_git_nul(output) } else { output };
                    return Ok(unaudited("git-grep", mode, nul, output, diagnostics, exit_code, termination));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("cannot start git grep fallback"),
        }
    }
    let mut grep = Command::new("grep");
    grep.arg("-rEIH");
    flags(&mut grep);
    for d in SKIP_DIRS {
        grep.arg(format!("--exclude-dir={d}"));
    }
    if let Some(g) = glob {
        grep.arg(format!("--include={g}"));
    }
    grep.args(["-e", pattern, "--", path]);
    let child = spawn_query(&mut grep).context("cannot start grep fallback")?;
    let (output, diagnostics, exit_code, termination) =
        collect_query_cancellable(child, QUERY_TIMEOUT, cancelled, None)?;
    Ok(unaudited("grep", mode, false, output, diagnostics, exit_code, termination))
}

/// git grep's `-z` line output, `path\0line\0text`, in the `path:line:text`
/// form the other engines show.
fn from_git_nul(raw: Capture) -> Capture {
    let mut text = String::with_capacity(raw.text.len());
    for line in raw.text.split_inclusive('\n') {
        text.push_str(&line.replacen('\0', ":", 2));
    }
    Capture { text, truncated: raw.truncated }
}

/// `nul`: names in `output` end with a NUL rather than a newline or `:`.
fn unaudited(
    engine: &'static str,
    mode: Mode,
    nul: bool,
    output: Capture,
    diagnostics: Capture,
    exit_code: Option<i32>,
    termination: &'static str,
) -> SearchResult {
    // Both fallbacks' -c counts matching lines, not matches.
    let output = match mode {
        Mode::Content => output,
        Mode::Files => present_files(output, if nul { '\0' } else { '\n' }),
        Mode::Count => present_counts(output, if nul { '\0' } else { ':' }, true),
    };
    SearchResult {
        engine,
        mode,
        scan_complete: false,
        audit_complete: false,
        match_limit_possible: false,
        column_limit_possible: false,
        output,
        diagnostics,
        exit_code,
        termination,
    }
}

/// `git grep` pathspecs for a search path and an rg-style glob. As with rg,
/// the glob is relative to the path, a glob without a slash matches a name at
/// any depth, `{a,b}` offers alternatives, and a leading `!` excludes.
fn pathspecs(path: &str, glob: Option<&str>) -> Vec<String> {
    let Some(glob) = glob else { return vec![path.to_string()] };
    let (exclude, glob) = glob.strip_prefix('!').map_or((false, glob), |g| (true, g));
    let base = path.trim_end_matches('/');
    let magic = if exclude { ":(glob,exclude)" } else { ":(glob)" };
    let mut specs: Vec<String> = alternatives(glob)
        .into_iter()
        .map(|g| {
            let g = if g.contains('/') { g } else { format!("**/{g}") };
            if base.is_empty() || base == "." {
                format!("{magic}{g}")
            } else {
                format!("{magic}{base}/{g}")
            }
        })
        .collect();
    if exclude {
        specs.insert(0, path.to_string());
    }
    specs
}

/// A glob's `{a,b}` groups expanded, which git's globs lack: `*.{rs,py}` is
/// `*.rs` and `*.py`.
fn alternatives(glob: &str) -> Vec<String> {
    let Some(open) = glob.find('{') else { return vec![glob.to_string()] };
    let Some(close) = glob[open..].find('}').map(|i| open + i) else { return vec![glob.to_string()] };
    glob[open + 1..close]
        .split(',')
        .flat_map(|alt| alternatives(&format!("{}{alt}{}", &glob[..open], &glob[close + 1..])))
        .collect()
}

/// The top of the git work tree holding `dir`, found by its `.git` entry.
pub fn repo_root(dir: &Path) -> Option<&Path> {
    dir.ancestors().find(|d| d.join(".git").exists())
}

/// Whether `path` lies in the work tree around the current directory, the
/// one `git grep` searches when run from here.
fn in_cwd_repo(path: &Path) -> bool {
    let (Ok(cwd), Ok(target)) = (std::env::current_dir().and_then(|d| d.canonicalize()), path.canonicalize()) else {
        return false;
    };
    repo_root(&cwd).is_some_and(|root| target.starts_with(root))
}

/// Matching file names, most recently modified first as `glob` lists them.
/// A name cut off by the capture limit is dropped rather than shown half.
fn present_files(raw: Capture, sep: char) -> Capture {
    let mut names: Vec<&str> = raw.text.split(sep).collect();
    if raw.truncated && !raw.text.ends_with(sep) {
        names.pop();
    }
    let mut stamped: Vec<_> = names
        .into_iter()
        .filter(|n| !n.is_empty())
        .map(|n| (fs::metadata(n).and_then(|m| m.modified()).ok(), n))
        .collect();
    stamped.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    let mut text = String::with_capacity(raw.text.len());
    for (_, name) in stamped {
        text.push_str(name);
        text.push('\n');
    }
    Capture { text, truncated: raw.truncated }
}

/// `path:count` for each file with a match, sorted by path, then the total.
/// `sep` divides a path from its count in the engine's output; `lines` says
/// the engine counted matching lines rather than matches.
fn present_counts(raw: Capture, sep: char, lines: bool) -> Capture {
    let mut truncated = raw.truncated;
    let mut rows: Vec<&str> = raw.text.lines().collect();
    if raw.truncated && !raw.text.ends_with('\n') {
        rows.pop();
    }
    let mut counts = Vec::with_capacity(rows.len());
    for line in rows {
        match line.rsplit_once(sep).and_then(|(path, n)| Some((path, n.parse::<u64>().ok()?))) {
            // grep -c names every file it read, matching or not.
            Some((_, 0)) => {}
            Some(count) => counts.push(count),
            None => truncated = true,
        }
    }
    counts.sort_unstable();
    let mut text = String::new();
    for (path, n) in &counts {
        let _ = writeln!(text, "{path}:{n}");
    }
    if !counts.is_empty() {
        let total: u64 = counts.iter().map(|c| c.1).sum();
        let unit = match (lines, total == 1) {
            (true, true) => "matching line",
            (true, false) => "matching lines",
            (false, true) => "match",
            (false, false) => "matches",
        };
        let files = format!("{} file{}", counts.len(), if counts.len() == 1 { "" } else { "s" });
        // A cut-off capture only counted what it kept.
        let _ = if truncated {
            writeln!(text, "partial total: {total} {unit} in the {files} shown")
        } else {
            writeln!(text, "{total} {unit} in {files}")
        };
    }
    Capture { text, truncated }
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
    /// The root is in a git repository, so `.gitignore` rules were applied.
    pub ignores_applied: bool,
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
        let scope = if self.ignores_applied {
            "[scope: build/dependency directories, .gitignore'd paths and nested repositories excluded; hidden files included; descendant directory symlinks skipped; not an atomic snapshot]\n"
        } else {
            "[scope: build/dependency directories excluded; hidden files included; ignore files not applied (not a git repository); descendant directory symlinks skipped; not an atomic snapshot]\n"
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

/// `.gitignore` rules as git applies them inside a repository: each file's
/// rules are relative to its directory, a later match overrides an earlier
/// one, and `!` re-includes. `.git/info/exclude` counts, below every
/// `.gitignore`; global excludes would need git's configuration and are not
/// read.
#[derive(Default)]
struct Ignores {
    active: bool,
    rules: Vec<IgnoreRule>,
}

struct IgnoreRule {
    /// The directory the rule's file sits in, as the walk spells it.
    base: PathBuf,
    /// For a file above the walk's root: the root's path below that file's
    /// directory, so anchored rules still match from where they were written.
    above: String,
    glob: String,
    negate: bool,
    dir_only: bool,
    anchored: bool,
}

impl Ignores {
    /// One ignore file's rules, after (so overriding) those already loaded.
    fn load(&mut self, file: &Path, base: &Path, above: &str) {
        let Ok(text) = fs::read_to_string(file) else { return };
        for line in text.lines().map(str::trim_end) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negate, line) = line.strip_prefix('!').map_or((false, line), |l| (true, l));
            let line = line.strip_prefix('\\').unwrap_or(line);
            let (dir_only, line) = line.strip_suffix('/').map_or((false, line), |l| (true, l));
            // A slash anywhere but the end ties the rule to its directory.
            let anchored = line.contains('/');
            let glob = line.trim_start_matches('/');
            if !glob.is_empty() {
                self.rules.push(IgnoreRule {
                    base: base.to_path_buf(),
                    above: above.to_string(),
                    glob: glob.to_string(),
                    negate,
                    dir_only,
                    anchored,
                });
            }
        }
    }

    fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        let mut ignored = false;
        for rule in &self.rules {
            // Only an ignoring rule can ignore, and only a `!` rule re-include.
            if ignored != rule.negate || (rule.dir_only && !is_dir) {
                continue;
            }
            let hit = if rule.anchored {
                path.strip_prefix(&rule.base).is_ok_and(|rel| {
                    let rel = rel.to_string_lossy();
                    let rel = if rule.above.is_empty() { rel.into_owned() } else { format!("{}/{rel}", rule.above) };
                    wildcard(rule.glob.as_bytes(), rel.as_bytes())
                })
            } else {
                wildcard(rule.glob.as_bytes(), name.as_bytes())
            };
            if hit {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

pub fn files_under(root: &Path) -> Enumeration {
    let mut scan = Enumeration::default();
    let mut ignores = Ignores::default();
    if let Ok(canonical) = root.canonicalize() {
        if let Some(top) = repo_root(&canonical) {
            ignores.active = true;
            let below = |dir: &Path| canonical.strip_prefix(dir).unwrap_or(&canonical).to_string_lossy().into_owned();
            // .git/info/exclude ranks below every .gitignore, so it goes first.
            ignores.load(&top.join(".git").join("info").join("exclude"), root, &below(top));
            // Then the directories from the repository's top down to the root.
            let mut above: Vec<&Path> = canonical.ancestors().skip(1).take_while(|d| d.starts_with(top)).collect();
            above.reverse();
            for dir in above {
                ignores.load(&dir.join(".gitignore"), root, &below(dir));
            }
        }
    }
    scan.ignores_applied = ignores.active;
    enumerate(root, 0, ENTRY_LIMIT, &mut scan, &mut ignores);
    scan
}

fn enumerate(dir: &Path, depth: usize, limit: usize, scan: &mut Enumeration, ignores: &mut Ignores) {
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
    // This directory's rules hold for everything beneath it, and no further.
    let inherited = ignores.rules.len();
    if ignores.active {
        ignores.load(&dir.join(".gitignore"), dir, "");
    }
    for entry in items {
        let path = entry.path();
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                scan.skip(&path, e);
                continue;
            }
        };
        if ignores.ignored(&path, metadata.is_dir()) {
            continue;
        }
        if metadata.is_dir() {
            if SKIP_DIRS.iter().any(|d| entry.file_name() == *d) {
                continue;
            }
            // A nested repository or worktree is its own project, as git sees
            // it, and usually a second copy of files already listed.
            if ignores.active && path.join(".git").exists() {
                continue;
            }
            match entry.file_type() {
                Ok(t) if t.is_symlink() => scan.skip(&path, "directory symlink not followed"),
                Ok(_) => enumerate(&path, depth + 1, limit, scan, ignores),
                Err(e) => scan.skip(&path, e),
            }
        } else if metadata.is_file() {
            scan.files.push(path);
        } else {
            scan.skip(&path, "not a regular file");
        }
    }
    ignores.rules.truncate(inherited);
}

/// Glob matching: `*` and `?` stay within one path segment, `**` crosses them.
pub fn wildcard(p: &[u8], s: &[u8]) -> bool {
    if p.starts_with(b"**/") {
        return (0..=s.len()).any(|i| (i == 0 || s[i - 1] == b'/') && wildcard(&p[3..], &s[i..]));
    }
    if p.starts_with(b"**") {
        return (0..=s.len()).any(|i| wildcard(&p[2..], &s[i..]));
    }
    match p.first() {
        None => s.is_empty(),
        Some(b'*') => {
            for i in 0..=s.len() {
                if wildcard(&p[1..], &s[i..]) {
                    return true;
                }
                if i < s.len() && s[i] == b'/' {
                    break;
                }
            }
            false
        }
        Some(b'?') => !s.is_empty() && s[0] != b'/' && wildcard(&p[1..], &s[1..]),
        Some(b'[') => match class(&p[1..], s.first().copied()) {
            Some((hit, rest)) => hit && wildcard(rest, &s[1..]),
            // A class that never closes is a literal `[`.
            None => s.first() == Some(&b'[') && wildcard(&p[1..], &s[1..]),
        },
        Some(c) => s.first() == Some(c) && wildcard(&p[1..], &s[1..]),
    }
}

/// A `[...]` class, given the pattern just past its `[`: whether `c` is in it
/// (never `/`), and the pattern after its `]`. `!` or `^` first negates, `a-z`
/// is a range, and a `]` first is a member. None when the class never closes.
fn class(p: &[u8], c: Option<u8>) -> Option<(bool, &[u8])> {
    let (negated, body) = match p.first() {
        Some(b'!' | b'^') => (true, &p[1..]),
        _ => (false, p),
    };
    let end = 1 + body.get(1..)?.iter().position(|&b| b == b']')?;
    let (set, rest) = (&body[..end], &body[end + 1..]);
    let Some(c) = c.filter(|&c| c != b'/') else { return Some((false, rest)) };
    let mut found = false;
    let mut i = 0;
    while i < set.len() {
        if i + 2 < set.len() && set[i + 1] == b'-' {
            found |= (set[i]..=set[i + 2]).contains(&c);
            i += 3;
        } else {
            found |= set[i] == c;
            i += 1;
        }
    }
    Some((found != negated, rest))
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
            mode: Mode::Content,
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
            mode: Mode::Content,
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
        let result = search_with(script.to_str().unwrap(), "needle", ".", None, 0, Mode::Content, &|| false);
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
            mode: Mode::Content,
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
        let missing = "/rusty/nonexistent-rg";
        let result = search_with(missing, "PUB fn search", file!(), None, 0, Mode::Content, &|| false).unwrap();
        assert_eq!(result.engine, "grep");
        assert!(!result.scan_complete);
        assert!(result.render().contains("parity unavailable"));
        // Smart case, as with rg: lower case matches any case, upper case is exact.
        let any_case = search_with(missing, "pub fn search", file!(), None, 0, Mode::Content, &|| false).unwrap();
        assert!(!any_case.output.text.is_empty());
        let exact = &"pub fn search".to_uppercase();
        let empty = search_with(missing, exact, file!(), None, 0, Mode::Content, &|| false).unwrap();
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
            mode: Mode::Content,
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
        let before = search_with("/rusty/nonexistent-rg", "needle", ".", None, 0, Mode::Content, &|| true).unwrap();
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
            None,
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
            let (out, _, status, reason) = collect_query_cancellable(child, timeout, &|| cancel, None).unwrap();
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
        enumerate(&dir, 0, 2, &mut scan, &mut Ignores::default());
        assert_eq!(scan.files.len(), 2);
        assert!(scan.incomplete && scan.skipped > 0);
        assert!(scan.render("no files observed", false).contains("remaining entries not scanned"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn enumeration_applies_gitignore_rules_inside_a_repository() {
        let root = std::env::temp_dir().join(format!("rusty-enumeration-ignores-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in [".git/info", "build-out", "src/build-out", "sub", "other", "nested/.git"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let files = [
            (".gitignore", "# comment\n*.log\n!keep.log\n/build-out/\nsub/deep.txt\n*.py[cod]\n!kept.tmp\n"),
            // Every .gitignore outranks info/exclude, whatever the order read.
            (".git/info/exclude", "secret.txt\n*.tmp\n"),
            ("x.pyc", ""),
            ("x.py", ""),
            ("dropped.tmp", ""),
            ("kept.tmp", ""),
            ("sub/.gitignore", "local.rs\n"),
            ("a.log", ""),
            ("keep.log", ""),
            ("secret.txt", ""),
            ("build-out/x.rs", ""),
            ("src/build-out/y.rs", ""),
            ("sub/deep.txt", ""),
            ("sub/local.rs", ""),
            ("sub/kept.rs", ""),
            ("other/local.rs", ""),
            // A worktree or nested repository inside the root is not entered.
            ("nested/n.rs", ""),
        ];
        for (path, text) in files {
            fs::write(root.join(path), text).unwrap();
        }
        let listed = |from: &Path| {
            let scan = files_under(from);
            assert!(scan.ignores_applied && !scan.incomplete);
            let mut names: Vec<String> =
                scan.files.iter().map(|f| f.strip_prefix(from).unwrap().to_string_lossy().into_owned()).collect();
            names.sort();
            names
        };
        assert_eq!(
            listed(&root),
            [
                ".gitignore",
                "keep.log",
                "kept.tmp",
                "other/local.rs",
                "src/build-out/y.rs",
                "sub/.gitignore",
                "sub/kept.rs",
                "x.py"
            ]
        );
        // Rules written above the root still apply, anchored ones included.
        assert_eq!(listed(&root.join("sub")), [".gitignore", "kept.rs"]);
        fs::remove_dir_all(root.join(".git")).unwrap();
        let plain = files_under(&root);
        assert!(!plain.ignores_applied && plain.files.len() == files.len() - 1);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn file_and_count_modes_list_names_and_totals() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rusty-query-modes-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let script = dir.join("rg-fixture");
        fs::write(
            &script,
            r#"#!/bin/sh
case " $* " in *" --max-count "*) exit 64;; esac
case " $* " in
  *" --files-with-matches "*) printf 'b.rs\000a.rs\000' ;;
  *" --count-matches "*) printf 'z.rs\0003\na.rs\0002\n' ;;
  *) exit 65 ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let program = script.to_str().unwrap();
        let files = search_with(program, "needle", ".", None, 0, Mode::Files, &|| false);
        let counts = search_with(program, "needle", ".", None, 0, Mode::Count, &|| false);
        fs::remove_dir_all(dir).unwrap();
        let files = files.unwrap();
        // Neither name exists here, so both lack a time and fall back to name order.
        assert_eq!(files.output.text, "a.rs\nb.rs\n");
        assert!(files.scan_complete && files.render().contains("most recently modified first"));
        let counts = counts.unwrap();
        assert_eq!(counts.output.text, "a.rs:2\nz.rs:3\n5 matches in 2 files\n");
        assert!(counts.scan_complete);
    }

    #[test]
    fn matching_files_come_newest_first_and_a_cut_name_is_dropped() {
        let dir = std::env::temp_dir().join(format!("rusty-query-newest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (old, new) = (dir.join("old.rs"), dir.join("new.rs"));
        for (path, age) in [(&old, 3600), (&new, 60)] {
            let file = fs::File::create(path).unwrap();
            file.set_modified(std::time::SystemTime::now() - Duration::from_secs(age)).unwrap();
        }
        let raw = format!("{}\0{}\0{}/cut", old.display(), new.display(), dir.display());
        let shown = present_files(Capture { text: raw, truncated: true }, '\0');
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(shown.text, format!("{}\n{}\n", new.display(), old.display()));
        assert!(shown.truncated);
        let counts = present_counts(Capture { text: "a.rs:0\nb.rs:1\n".into(), truncated: false }, ':', true);
        assert_eq!(counts.text, "b.rs:1\n1 matching line in 1 file\n");
        // A cut-off capture says its total is partial, and drops the cut row.
        let cut = present_counts(Capture { text: "a.rs\x002\nb.rs\x003\nc.r".into(), truncated: true }, '\0', false);
        assert_eq!(cut.text, "a.rs:2\nb.rs:3\npartial total: 5 matches in the 2 files shown\n");
    }

    #[test]
    fn output_modes_parse_by_name() {
        assert!(Mode::parse(None).is_ok_and(|m| m == Mode::Content));
        assert!(Mode::parse(Some("files_with_matches")).is_ok_and(|m| m == Mode::Files));
        assert!(Mode::parse(Some("lines")).is_err());
    }

    #[test]
    fn fallback_in_a_repository_uses_git_grep_and_leaves_ignored_files_out() {
        if Command::new("git").arg("--version").output().is_err()
            || repo_root(&std::env::current_dir().unwrap()).is_none()
        {
            return;
        }
        // target/ is ignored by the repository's .gitignore.
        let token = format!("ignoredprobe{}", std::process::id());
        // An untracked file with a name git would otherwise quote.
        let odd = PathBuf::from("tests").join(format!("caf\u{e9}-probe-{}.txt", std::process::id()));
        fs::write(&odd, format!("named{token}")).unwrap();
        let named =
            search_with("/rusty/nonexistent-rg", &format!("named{token}"), "tests", None, 0, Mode::Files, &|| false);
        let line =
            search_with("/rusty/nonexistent-rg", &format!("named{token}"), "tests", None, 0, Mode::Content, &|| false);
        fs::remove_file(&odd).unwrap();
        assert_eq!(named.unwrap().output.text, format!("{}\n", odd.display()));
        assert_eq!(line.unwrap().output.text, format!("{}:1:named{token}\n", odd.display()));
        let dir = PathBuf::from("target").join(format!("rusty-fallback-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("probe.rs"), &token).unwrap();
        let hidden = search_with("/rusty/nonexistent-rg", &token, ".", None, 0, Mode::Files, &|| false).unwrap();
        let shown = search_with("/rusty/nonexistent-rg", "fn search_with", "src", None, 0, Mode::Files, &|| false);
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(hidden.engine, "git-grep");
        assert!(hidden.output.text.is_empty(), "{}", hidden.output.text);
        let shown = shown.unwrap();
        assert_eq!(shown.output.text, "src/workspace_query.rs\n");
        assert!(!shown.scan_complete && shown.render().contains(".gitignore applied"));
    }

    #[test]
    fn wildcard_classes_match_as_git_does() {
        assert!(wildcard(b"*.py[cod]", b"x.pyc") && !wildcard(b"*.py[cod]", b"x.py"));
        assert!(wildcard(b"[Bb]in", b"bin") && wildcard(b"[Bb]in", b"Bin") && !wildcard(b"[Bb]in", b"pin"));
        assert!(wildcard(b"*.[a-c]", b"x.b") && !wildcard(b"*.[a-c]", b"x.d"));
        assert!(wildcard(b"[!a]x", b"bx") && wildcard(b"[^a]x", b"bx") && !wildcard(b"[!a]x", b"ax"));
        assert!(wildcard(b"[]]", b"]") && !wildcard(b"a[/]b", b"a/b"));
        assert!(wildcard(b"a[b", b"a[b"), "a class that never closes is literal");
    }

    #[test]
    fn git_grep_pathspecs_follow_the_path_and_glob() {
        assert_eq!(pathspecs(".", None), ["."]);
        assert_eq!(pathspecs(".", Some("*.rs")), [":(glob)**/*.rs"]);
        assert_eq!(pathspecs("src/", Some("src/**/*.rs")), [":(glob)src/src/**/*.rs"]);
        assert_eq!(pathspecs(".", Some("*.{rs,py}")), [":(glob)**/*.rs", ":(glob)**/*.py"]);
        assert_eq!(pathspecs("src", Some("!*.md")), ["src", ":(glob,exclude)src/**/*.md"]);
    }
}
