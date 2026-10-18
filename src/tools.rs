//! The agent's tools: read, write, edit, list, search and run shell commands.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::signal;
use crate::ui::{self, truncate};

use rusty::workspace_query::{self, SKIP_DIRS};

const MAX_OUTPUT: usize = 30_000;

pub fn definitions() -> Value {
    json!([
        tool("read_file", "Read a text file. Returns numbered lines. For big files, outline or search first and read only the range you need.", json!({
            "path": {"type": "string", "description": "File path"},
            "offset": {"type": "integer", "description": "1-based line to start at (default 1)"},
            "limit": {"type": "integer", "description": "Max lines to return (default 800)"}
        }), &["path"]),
        tool("write_file", "Create or overwrite a file with the given content. Creates parent directories.", json!({
            "path": {"type": "string"},
            "content": {"type": "string", "description": "Full new file content"}
        }), &["path", "content"]),
        tool("edit_file", "Replace an exact string in a file. old_string must match exactly once unless replace_all is true. Read the file first.", json!({
            "path": {"type": "string"},
            "old_string": {"type": "string", "description": "Exact text to replace, including whitespace"},
            "new_string": {"type": "string"},
            "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
        }), &["path", "old_string", "new_string"]),
        tool("list_files", "List files and directories recursively (skips .git, target, node_modules, ...).", json!({
            "path": {"type": "string", "description": "Directory (default .)"},
            "max_depth": {"type": "integer", "description": "Default 3"}
        }), &[]),
        tool("search", "Search file contents with ripgrep smart-case regex and default ignore rules. Reports scan completeness separately from output truncation. Grep fallback has reduced semantics and is marked incomplete. Retrieval is not acceptance evidence.", json!({
            "pattern": {"type": "string"},
            "path": {"type": "string", "description": "Directory or file (default .)"},
            "glob": {"type": "string", "description": "Only search files matching this glob, e.g. *.rs"},
            "context": {"type": "integer", "description": "Lines of context around each match (default 0)"}
        }), &["pattern"]),
        tool("glob", "Find files by path pattern: *.rs, src/**/*.ts, **/test_*.py. Names only, no contents. Skips build directories; includes hidden files; does not apply ignore files. Reports incomplete enumeration and skipped paths.", json!({
            "pattern": {"type": "string"},
            "path": {"type": "string", "description": "Directory to search (default .)"}
        }), &["pattern"]),
        tool("outline", "List the definitions (functions, types, classes, impls) in a file or directory with line numbers. Use it to understand structure before reading.", json!({
            "path": {"type": "string"}
        }), &["path"]),
        tool("bash", "Run a shell command in the working directory and return exit code, stdout and stderr. Use for builds, tests, git, etc. Non-interactive only. Each call starts a fresh shell in the working directory: cd, variables and exports do not carry over, so chain them, as in `cd sub && make`.", json!({
            "command": {"type": "string"},
            "timeout_secs": {"type": "integer", "description": "Default 120, max 600"}
        }), &["command"]),
    ])
}

pub fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": { "type": "object", "properties": properties, "required": required }
        }
    })
}

/// Whether a shell command does nothing but print text (`echo`, `printf`,
/// `exit 0`) it already knows: no variables, redirects, substitutions or
/// pipes into something else.
pub fn only_prints(cmd: &str) -> bool {
    if cmd.trim().is_empty() || cmd.contains(['>', '`', '$']) {
        return false;
    }
    cmd.split(['\n', ';', '&', '|'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .all(|seg| matches!(seg.split_whitespace().next(), Some("echo" | "printf" | "exit" | "true" | ":")))
}

/// One-line description of a call, shown as its header.
pub fn summary(name: &str, args: &Value) -> String {
    let s = |k: &str| args[k].as_str().unwrap_or("").to_string();
    let text = match name {
        "bash" | "bash_start" => {
            // Hide a redundant `cd <project> &&` so the command itself shows.
            let cmd = s("command");
            let cwd = std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default();
            let cmd = ["cd {cwd} && ", "cd \"{cwd}\" && ", "cd {cwd}; "]
                .iter()
                .find_map(|p| cmd.strip_prefix(&p.replace("{cwd}", &cwd)).map(str::to_string))
                .unwrap_or(cmd);
            // A script shows its first command and how much follows, so a run of
            // multi-line scripts doesn't read as the same bare `cd` over and over.
            let mut lines = cmd.lines().map(str::trim).filter(|l| !l.is_empty());
            let first = lines.next().unwrap_or("").to_string();
            match lines.count() {
                0 => first,
                n => format!("{}  (+{n} line{})", truncate(&first, 100), if n == 1 { "" } else { "s" }),
            }
        }
        "search" => format!("/{}/ in {}", s("pattern"), args["path"].as_str().filter(|s| !s.is_empty()).unwrap_or(".")),
        "glob" => s("pattern"),
        "outline" => s("path"),
        "swarm" => format!("{} tasks", args["tasks"].as_array().map_or(0, Vec::len)),
        "list_files" => args["path"].as_str().filter(|s| !s.is_empty()).unwrap_or(".").to_string(),
        "task" => s("description"),
        "bash_wait" | "bash_cancel" => s("id"),
        "remember" => format!("{}: {}", s("kind"), s("text")),
        "recall" => s("query"),
        "forget" => s("id"),
        "plan" => format!("{} items", args["items"].as_array().map_or(0, Vec::len)),
        "goal_done" | "loop_next" => String::new(),
        _ => s("path"),
    };
    truncate(&text, 120).to_string()
}

/// A longer preview shown when asking the user to approve a call.
pub fn preview(name: &str, args: &Value) -> String {
    let mut out = String::new();
    match name {
        "bash" | "bash_start" => {
            // A short one-liner is already on the tool line above.
            let cmd = args["command"].as_str().unwrap_or("");
            if !cmd.contains('\n') && cmd.chars().count() <= 80 {
                return out;
            }
            for l in cmd.lines() {
                let _ = writeln!(out, "    {} {}", ui::dim("$"), l);
            }
        }
        "write_file" => {
            let content = args["content"].as_str().unwrap_or("");
            let n = content.lines().count();
            for l in content.lines().take(15) {
                let _ = writeln!(out, "    {}", ui::ok(&format!("+ {l}")));
            }
            if n > 15 {
                let _ = writeln!(out, "    {}", ui::dim(&format!("… {} more lines", n - 15)));
            }
        }
        "edit_file" => {
            for (key, sign) in [("old_string", "-"), ("new_string", "+")] {
                let text = args[key].as_str().unwrap_or("");
                for l in text.lines().take(20) {
                    let line = format!("{sign} {l}");
                    let painted = if sign == "-" { ui::err(&line) } else { ui::ok(&line) };
                    let _ = writeln!(out, "    {painted}");
                }
            }
        }
        _ => {}
    }
    out
}

pub fn execute(name: &str, args: &Value) -> Result<String> {
    match name {
        "read_file" => read_file(args),
        "glob" => glob(args),
        "outline" => outline(args),
        "write_file" => write_file(args),
        "edit_file" => edit_file(args),
        "list_files" => list_files(args),
        "search" => search(args),
        "bash" => bash(args),
        other => bail!("unknown tool `{other}`"),
    }
}

fn arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args[key].as_str().ok_or_else(|| anyhow!("missing string argument `{key}`"))
}

fn read_file(args: &Value) -> Result<String> {
    let path = arg(args, "path")?;
    let text = fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = args["limit"].as_u64().unwrap_or(800).max(1) as usize;
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Ok("(empty file)".into());
    }
    let mut out = String::new();
    for (i, l) in lines.iter().enumerate().skip(offset - 1).take(limit) {
        let _ = writeln!(out, "{:>6}\t{}", i + 1, truncate(l, 2000));
    }
    let shown_to = (offset - 1 + limit).min(lines.len());
    if shown_to < lines.len() {
        let _ = writeln!(out, "… {} more lines (use offset={})", lines.len() - shown_to, shown_to + 1);
    }
    Ok(cap(out))
}

fn write_file(args: &Value) -> Result<String> {
    let path = arg(args, "path")?;
    let content = arg(args, "content")?;
    let before = fs::read_to_string(path).ok();
    let warning = guard(path, before.as_deref(), content)?;
    if let Some(parent) = Path::new(path).parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, content).with_context(|| format!("cannot write {path}"))?;
    Ok(format!("wrote {} lines to {path}{warning}", content.lines().count()))
}

/// Refuses a change that breaks a file that parsed before, leaving it as it
/// was; a file that was already broken (or new) is written with a warning.
fn guard(path: &str, before: Option<&str>, after: &str) -> Result<String> {
    let Some(err) = syntax_error(path, after) else { return Ok(String::new()) };
    if before.is_some_and(|b| syntax_error(path, b).is_none()) {
        bail!("rejected: {path} would no longer parse ({err}). The file is unchanged; fix the change and retry.");
    }
    Ok(format!("\nwarning: {path} does not parse: {err}"))
}

/// A parse error for the languages rusty can check cheaply: JSON in process,
/// Python and shell with their own parsers when installed. None means fine
/// or not checkable.
pub fn syntax_error(path: &str, text: &str) -> Option<String> {
    let ext = Path::new(path).extension()?.to_str()?;
    let check: &[&str] = match ext {
        "json" => {
            return serde_json::from_str::<serde::de::IgnoredAny>(text).err().map(|e| e.to_string());
        }
        "py" => &["python3", "-c", "import ast,sys; ast.parse(sys.stdin.read(), sys.argv[1])"],
        "sh" | "bash" => &["bash", "-n"],
        _ => return None,
    };
    let mut cmd = std::process::Command::new(check[0]);
    cmd.args(&check[1..]);
    if ext == "py" {
        cmd.arg(path);
    }
    let mut child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input = text.to_string();
    std::thread::spawn(move || {
        use std::io::Write as _;
        let _ = stdin.write_all(input.as_bytes());
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return None,
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                return None;
            }
        }
    }
    let mut err = String::new();
    std::io::Read::read_to_string(&mut child.stderr.take()?, &mut err).ok()?;
    let message = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("syntax error").trim();
    // Python names the line on a separate traceback line.
    let at = err.lines().rev().find_map(|l| l.split_once("\", line ").map(|(_, n)| n.trim().to_string()));
    let text = match at {
        Some(n) => format!("line {n}: {message}"),
        None => message.to_string(),
    };
    Some(text.chars().take(300).collect())
}

fn edit_file(args: &Value) -> Result<String> {
    let path = arg(args, "path")?;
    let old = arg(args, "old_string")?;
    let new = arg(args, "new_string")?;
    if old.is_empty() {
        bail!("old_string is empty; use write_file to create files");
    }
    let text = fs::read_to_string(path).with_context(|| format!("cannot read {path}"))?;
    let count = text.matches(old).count();
    let replace_all = args["replace_all"].as_bool().unwrap_or(false);
    let updated = match count {
        0 => bail!("old_string not found in {path}; read the file and copy the text exactly"),
        1 => text.replacen(old, new, 1),
        _ if replace_all => text.replace(old, new),
        n => bail!("old_string matches {n} times in {path}; add context to make it unique or set replace_all"),
    };
    let warning = guard(path, Some(&text), &updated)?;
    fs::write(path, updated)?;
    Ok(format!("edited {path} ({} replacement{}){warning}", count, if count == 1 { "" } else { "s" }))
}

fn list_files(args: &Value) -> Result<String> {
    let root = args["path"].as_str().filter(|s| !s.is_empty()).unwrap_or(".");
    let requested_depth = args["max_depth"].as_u64().unwrap_or(3) as usize;
    let max_depth = requested_depth.min(128);
    let mut entries = Vec::new();
    let mut scan = workspace_query::Enumeration::default();
    if requested_depth > max_depth {
        scan.skip(Path::new(root), "requested listing depth exceeds safety limit of 128");
    }
    walk(Path::new(root), Path::new(root), 0, max_depth, &mut entries, &mut scan);
    let total = entries.len();
    entries.truncate(500);
    let mut out = format!(
        "[requested depth={requested_depth} effective depth={max_depth}; deeper paths outside scope]\n{}",
        entries.join("\n")
    );
    if total > 500 {
        let _ = write!(out, "\n… {} more entries (narrow the path or depth)", total - 500);
    }
    let truncated = total > 500 || out.len() > workspace_query::OUTPUT_BYTES;
    Ok(scan.render(truncate(&out, workspace_query::OUTPUT_BYTES), truncated))
}

fn walk(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    out: &mut Vec<String>,
    scan: &mut workspace_query::Enumeration,
) {
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) => {
            scan.skip(dir, e);
            return;
        }
    };
    let mut items = Vec::new();
    for (i, entry) in rd.take(2001).enumerate() {
        if i == 2000 {
            scan.skip(dir, "listing budget reached; remaining entries not enumerated");
            break;
        }
        match entry {
            Ok(entry) => items.push(entry),
            Err(e) => scan.skip(dir, e),
        }
    }
    items.sort_by_key(|e| e.file_name());
    for e in items {
        if out.len() >= 2000 {
            scan.skip(dir, "listing budget reached; remaining entries/subtrees not scanned");
            break;
        }
        let name = e.file_name().to_string_lossy().to_string();
        let path = e.path();
        let rel = path.strip_prefix(root).unwrap_or(&path).display().to_string();
        let metadata = match fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                scan.skip(&path, e);
                continue;
            }
        };
        if metadata.is_dir() {
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            out.push(format!("{rel}/"));
            if depth + 1 < max_depth {
                match e.file_type() {
                    Ok(t) if t.is_symlink() => scan.skip(&path, "directory symlink not followed"),
                    Ok(_) => walk(root, &path, depth + 1, max_depth, out, scan),
                    Err(e) => scan.skip(&path, e),
                }
            }
        } else {
            out.push(rel);
        }
    }
}

fn search(args: &Value) -> Result<String> {
    let pattern = arg(args, "pattern")?;
    let path = args["path"].as_str().filter(|s| !s.is_empty()).unwrap_or(".");
    Ok(workspace_query::search_cancellable(
        pattern,
        path,
        args["glob"].as_str(),
        args["context"].as_u64().unwrap_or(0),
        signal::interrupted,
    )?
    .render())
}

fn bash(args: &Value) -> Result<String> {
    let command = arg(args, "command")?;
    let timeout = Duration::from_secs(args["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 600));
    let (status, stdout, stderr) = run(command, timeout)?;
    let mut out = match status {
        Some(code) => format!("exit code: {}\n", code.map_or("signal".into(), |c| c.to_string())),
        None if signal::interrupted() => "interrupted by the user (killed)\n".to_string(),
        None => format!("timed out after {}s (killed)\n", timeout.as_secs()),
    };
    if !stdout.is_empty() {
        let _ = write!(out, "stdout:\n{stdout}");
    }
    if !stderr.is_empty() {
        let _ = write!(out, "\nstderr:\n{stderr}");
    }
    if stderr.contains("mcp__") && stderr.contains("not found") {
        out.push_str(
            "\nnote: mcp__ tools are tool calls, not shell commands, so bash can't run them. Call the tool \
             directly, then use what it returned in your command.",
        );
    }
    if only_changes_directory(command) {
        out.push_str(
            "\nnote: nothing else ran. Each bash call starts a fresh shell in the working directory, \
             so this cd ended with the call. Chain it with what should run there: cd DIR && command",
        );
    }
    Ok(cap(out))
}

/// Whether a command does nothing but change directory, which a fresh shell
/// per call throws away.
fn only_changes_directory(cmd: &str) -> bool {
    let segs: Vec<&str> = cmd.split(['\n', ';', '&']).map(str::trim).filter(|s| !s.is_empty()).collect();
    !segs.is_empty()
        && segs.iter().all(|seg| matches!(seg.split_whitespace().next(), Some("cd" | "pushd" | "popd")))
        && !cmd.contains(['|', '>', '`', '$'])
}

/// Runs a shell command with a time limit. Returns (exit status, stdout,
/// stderr); the status is None when the command was killed, and
/// Some(None) when it died from a signal. Also used by the infra harness
/// for its own snapshot and verification commands.
pub fn run(command: &str, timeout: Duration) -> Result<(Option<Option<i32>>, String, String)> {
    run_owned(command, timeout, None)
}

/// The activity owner may cancel its shell without interrupting unrelated work.
pub(crate) fn run_cancellable(
    command: &str,
    timeout: Duration,
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<(Option<Option<i32>>, String, String)> {
    run_owned(command, timeout, Some(cancel))
}

fn run_owned(
    command: &str,
    timeout: Duration,
    cancel: Option<&std::sync::atomic::AtomicBool>,
) -> Result<(Option<Option<i32>>, String, String)> {
    use std::sync::atomic::Ordering;
    let cancelled = || cancel.is_some_and(|c| c.load(Ordering::Relaxed));
    if cancelled() || signal::interrupted() {
        return Ok((None, String::new(), String::new()));
    }
    let mut cmd = Command::new("bash");
    cmd.arg("-c").arg(command).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    // Secrets the model saw only as ${RUSTY_SECRET_n} handles.
    cmd.envs(rusty::privacy::secret_env());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = OwnedShell(cmd.spawn().context("failed to spawn bash")?);

    // Drain pipes on threads so a chatty command can't deadlock on a full pipe.
    let so = child.0.stdout.take().unwrap();
    let se = child.0.stderr.take().unwrap();
    let t_out = ShellReader::new(so)?;
    let t_err = ShellReader::new(se)?;

    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.0.try_wait()? {
            break Some(s.code());
        }
        // Only the coordinator reads terminal input. Background owners must
        // not race its poll/read and accidentally block on the same keystroke.
        if cancel.is_none() {
            signal::poll_keys();
        }
        if start.elapsed() > timeout || signal::interrupted() || cancelled() {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    // Kill descendants even when the shell exited first, before joining pipe
    // readers. A background child must not keep verification alive forever.
    drop(child);
    let (out, out_complete) = t_out.finish();
    let (err, err_complete) = t_err.finish();
    let stdout = String::from_utf8_lossy(&out).to_string();
    let mut stderr = String::from_utf8_lossy(&err).to_string();
    let complete = out_complete && err_complete;
    if !complete {
        stderr.push_str("\n[output capture incomplete: inherited pipe remained open; readers stopped]\n");
    }
    // An exited launcher alone cannot establish successful verification.
    Ok((if complete { status } else { None }, stdout, stderr))
}

// Reader ownership is independent of descendant process-group containment.
// A setsid descendant can retain a pipe; it must not retain our thread or FD.
struct ShellReader {
    eof: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<Vec<u8>>>,
}
impl ShellReader {
    fn new<T: Read + std::os::fd::AsRawFd + Send + 'static>(pipe: T) -> Result<Self> {
        use std::os::fd::AsRawFd;
        struct Cancellable<T> {
            eof: std::sync::Arc<std::sync::atomic::AtomicBool>,
            pipe: T,
            stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        }
        impl<T: Read + AsRawFd> Read for Cancellable<T> {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                loop {
                    if self.stop.load(std::sync::atomic::Ordering::Acquire) {
                        return Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "shell reader stopped"));
                    }
                    match self.pipe.read(bytes) {
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            let mut fd = libc::pollfd { fd: self.pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                            let rc = unsafe { libc::poll(&mut fd, 1, 25) };
                            if rc < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                        Ok(0) => {
                            self.eof.store(true, std::sync::atomic::Ordering::Release);
                            return Ok(0);
                        }
                        result => return result,
                    }
                }
            }
        }
        let fd = pipe.as_raw_fd();
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error()).context("setting shell output nonblocking");
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let eof = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let input = Cancellable { pipe, stop: stop.clone(), eof: eof.clone() };
        let thread = Some(std::thread::spawn(move || drain(input)));
        Ok(Self { stop, thread, eof })
    }

    fn finish(mut self) -> (Vec<u8>, bool) {
        let until = Instant::now() + Duration::from_millis(100);
        let thread = self.thread.as_ref().unwrap();
        while !thread.is_finished() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        let complete = thread.is_finished() && self.eof.load(std::sync::atomic::Ordering::Acquire);
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        match self.thread.take().unwrap().join() {
            Ok(output) => (output, complete),
            Err(_) => (Vec::new(), false),
        }
    }
}
impl Drop for ShellReader {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct OwnedShell(std::process::Child);

impl Drop for OwnedShell {
    fn drop(&mut self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn drain(mut pipe: impl Read) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buffer = [0; 8192];
    let mut truncated = false;
    while let Ok(n) = pipe.read(&mut buffer) {
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buffer[..n]);
        if out.len() > MAX_OUTPUT {
            out.drain(MAX_OUTPUT / 2..out.len() - MAX_OUTPUT / 2);
            truncated = true;
        }
    }
    if truncated {
        out.splice(MAX_OUTPUT / 2..MAX_OUTPUT / 2, b"\n[output truncated]\n".iter().copied());
    }
    out
}

fn glob(args: &Value) -> Result<String> {
    let pattern = arg(args, "pattern")?;
    let root = args["path"].as_str().filter(|s| !s.is_empty()).unwrap_or(".");
    let scan = workspace_query::files_under(Path::new(root));
    let hits: Vec<String> = scan
        .files
        .iter()
        .filter_map(|p| {
            let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().trim_start_matches("./").to_string();
            let target =
                if pattern.contains('/') { rel.clone() } else { rel.rsplit('/').next().unwrap_or(&rel).to_string() };
            wildcard(pattern.as_bytes(), target.as_bytes()).then_some(rel)
        })
        .collect();
    if hits.is_empty() {
        return Ok(scan.render(
            if scan.incomplete { "no files observed; enumeration incomplete" } else { "no files match" },
            false,
        ));
    }
    let mut out = hits.iter().take(300).cloned().collect::<Vec<_>>().join("\n");
    if hits.len() > 300 {
        let _ = write!(out, "\n… {} more", hits.len() - 300);
    }
    let truncated = hits.len() > 300 || out.len() > workspace_query::OUTPUT_BYTES;
    Ok(scan.render(truncate(&out, workspace_query::OUTPUT_BYTES), truncated))
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
        Some(c) => s.first() == Some(c) && wildcard(&p[1..], &s[1..]),
    }
}

const SOURCE_EXT: &[&str] = &[
    "rs", "py", "js", "jsx", "ts", "tsx", "go", "java", "kt", "swift", "c", "h", "cc", "cpp", "hpp", "rb", "php", "cs",
    "scala", "ex", "exs", "lua", "sh", "zig",
];

const DEF_PREFIXES: &[&str] = &[
    "pub fn ",
    "fn ",
    "pub(crate) fn ",
    "async fn ",
    "pub async fn ",
    "pub struct ",
    "struct ",
    "pub enum ",
    "enum ",
    "pub trait ",
    "trait ",
    "impl ",
    "impl<",
    "pub mod ",
    "mod ",
    "macro_rules!",
    "pub type ",
    "pub const ",
    "def ",
    "async def ",
    "class ",
    "function ",
    "async function ",
    "export function ",
    "export async function ",
    "export default ",
    "export class ",
    "export const ",
    "export interface ",
    "export type ",
    "interface ",
    "func ",
    "public ",
    "private ",
    "protected ",
    "module ",
];

fn outline(args: &Value) -> Result<String> {
    let path = Path::new(arg(args, "path")?);
    if path.is_file() {
        let o = outline_file(path)?;
        let truncated = o.len() > workspace_query::OUTPUT_BYTES;
        return Ok(workspace_query::Enumeration::default().render(
            if o.is_empty() { "(no definitions found)" } else { truncate(&o, workspace_query::OUTPUT_BYTES) },
            truncated,
        ));
    }
    let mut scan = workspace_query::files_under(path);
    let files: Vec<_> = scan
        .files
        .iter()
        .filter(|f| f.extension().and_then(|e| e.to_str()).is_some_and(|e| SOURCE_EXT.contains(&e)))
        .cloned()
        .collect();
    let mut out = String::new();
    for (i, f) in files.iter().enumerate() {
        if i >= 60 || out.len() >= workspace_query::OUTPUT_BYTES {
            scan.skip(f, format!("outline budget reached; {} eligible files not read", files.len() - i));
            break;
        }
        match outline_file(f) {
            Ok(o) if !o.is_empty() => {
                let _ = writeln!(out, "## {}\n{o}", f.display());
            }
            Ok(_) => {}
            Err(e) => scan.skip(f, e),
        }
    }
    let truncated = out.len() > workspace_query::OUTPUT_BYTES;
    let body = if out.is_empty() {
        if scan.incomplete {
            "(no definitions observed; outline incomplete)"
        } else {
            "(no definitions found)"
        }
    } else {
        truncate(&out, workspace_query::OUTPUT_BYTES)
    };
    Ok(scan.render(body, truncated))
}

fn outline_file(path: &Path) -> Result<String> {
    let text = fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut out = String::new();
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent > 8 || !DEF_PREFIXES.iter().any(|p| trimmed.starts_with(p)) {
            continue;
        }
        // Skip `public`/`private` lines that are fields rather than methods.
        if (trimmed.starts_with("public ") || trimmed.starts_with("private ") || trimmed.starts_with("protected "))
            && !trimmed.contains('(')
        {
            continue;
        }
        let sig = trimmed.trim_end_matches(['{', ':', ' ']);
        let _ = writeln!(out, "{:>5}: {}{}", i + 1, " ".repeat(indent.min(8)), sig);
    }
    Ok(out)
}

/// Keeps the head and tail of long outputs so the model sees both ends.
pub fn cap(s: String) -> String {
    if s.len() <= MAX_OUTPUT {
        return s;
    }
    let head = truncate(&s, MAX_OUTPUT / 2);
    let mut start = s.len() - MAX_OUTPUT / 2;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("{head}\n… [{} bytes omitted] …\n{}", s.len() - head.len() - (s.len() - start), &s[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_tools_honor_run_scoped_cancellation() {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let _scope = signal::cancel_scope(cancel);
        let out = search(&json!({"pattern":"needle"})).unwrap();
        assert!(out.contains("termination=cancelled") && out.contains("scan_complete=false"));
    }

    #[test]
    fn query_tools_report_scope_skips_and_mutations() {
        let dir = std::env::temp_dir().join(format!("rusty-query-tools-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let args = json!({"path": dir, "pattern": "**/*.rs"});
        assert!(glob(&args).unwrap().contains("no files match"));
        fs::write(dir.join("first.rs"), "fn first() {}\n").unwrap();
        assert!(glob(&args).unwrap().contains("first.rs"));
        fs::rename(dir.join("first.rs"), dir.join("second.rs")).unwrap();
        let renamed = glob(&args).unwrap();
        assert!(renamed.contains("second.rs") && !renamed.contains("first.rs"));
        fs::write(dir.join("bad.rs"), [0xff]).unwrap();
        let outline = outline(&json!({"path":dir})).unwrap();
        assert!(outline.contains("scan_complete=false") && outline.contains("bad.rs"));
        assert!(outline.contains("fn first()"));
        let missing = json!({"path": dir.join("missing"), "pattern": "*.rs"});
        let g = glob(&missing).unwrap();
        assert!(g.contains("scan_complete=false") && !g.contains("no files match"));
        let l = list_files(&missing).unwrap();
        assert!(l.contains("scan_complete=false") && l.contains("missing"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn outline_unread_files_are_incomplete_not_merely_display_truncated() {
        let dir = std::env::temp_dir().join(format!("rusty-outline-budget-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        for i in 0..61 {
            fs::write(dir.join(format!("{i:03}.rs")), "fn f() {}\n").unwrap();
        }
        let out = outline(&json!({"path":dir})).unwrap();
        assert!(out.contains("scan_complete=false output_truncated=false"));
        assert!(out.contains("1 eligible files not read"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn edits_that_break_a_parsing_file_are_rejected() {
        let dir = std::env::temp_dir().join(format!("rusty-guard-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let json = dir.join("c.json");
        fs::write(&json, "{\"a\": 1}").unwrap();
        let p = json.to_str().unwrap();
        let err = edit_file(&json!({"path": p, "old_string": "1}", "new_string": "1,"})).unwrap_err().to_string();
        assert!(err.starts_with("rejected:") && err.contains("unchanged"), "{err}");
        assert_eq!(fs::read_to_string(&json).unwrap(), "{\"a\": 1}");
        assert!(edit_file(&json!({"path": p, "old_string": "1", "new_string": "2"})).unwrap().starts_with("edited"));
        // Already broken: the write goes through with a warning.
        fs::write(&json, "{").unwrap();
        let ok = write_file(&json!({"path": p, "content": "{\"b\":"})).unwrap();
        assert!(ok.contains("warning:") && ok.contains("does not parse"), "{ok}");
        if which("python3") {
            let py = dir.join("m.py");
            fs::write(&py, "def f():\n    return 1\n").unwrap();
            let err = edit_file(&json!({"path": py.to_str().unwrap(), "old_string": "f():", "new_string": "f(:"}))
                .unwrap_err()
                .to_string();
            assert!(err.contains("line 1") && err.contains("SyntaxError"), "{err}");
        }
        if which("bash") {
            let sh = dir.join("s.sh");
            let r = write_file(&json!({"path": sh.to_str().unwrap(), "content": "if true; then\n"})).unwrap();
            assert!(r.contains("warning:"), "a new broken script is written with a warning: {r}");
        }
        assert_eq!(syntax_error("notes.txt", "{{{"), None, "unknown types are never checked");
        let _ = fs::remove_dir_all(&dir);
    }

    fn which(tool: &str) -> bool {
        std::process::Command::new(tool).arg("--version").output().is_ok()
    }

    #[test]
    fn output_is_drained_with_bounded_head_and_tail() {
        let mut input = std::io::Cursor::new([b"start".as_slice(), &vec![b'x'; 1_000_000], b"end"].concat());
        let out = drain(&mut input);
        assert_eq!(input.position(), 1_000_008);
        assert!(out.starts_with(b"start") && out.ends_with(b"end"));
        assert!(out.len() < MAX_OUTPUT + 100);
    }

    #[test]
    #[cfg(unix)]
    fn exited_shell_and_timeout_cannot_leave_descendants_holding_pipes() {
        for (command, expected) in [("sleep 30 & printf done", Some(Some(0))), ("sleep 30 & wait", None)] {
            let started = Instant::now();
            let (status, _, _) = run(command, Duration::from_millis(100)).unwrap();
            assert_eq!(status, expected);
            assert!(started.elapsed() < Duration::from_secs(3));
        }
    }

    #[test]
    fn completion_contract_shell_releases_readers_held_by_escaped_processes() {
        use std::sync::{atomic::AtomicBool, Arc};
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                if let Ok(pid) = fs::read_to_string(self.0.join("pid")).unwrap_or_default().parse::<i32>() {
                    unsafe {
                        libc::kill(pid, libc::SIGKILL);
                    }
                }
                let _ = fs::remove_dir_all(&self.0);
            }
        }
        for mode in ["exit", "timeout", "cancel"] {
            let fixture =
                Fixture(std::env::temp_dir().join(format!("rusty-shell-escaped-{}-{mode}", std::process::id())));
            fs::create_dir_all(&fixture.0).unwrap();
            let pidfile = fixture.0.join("pid");
            let script = fixture.0.join("holder.py");
            fs::write(
                &script,
                r#"import os,sys,time
r,w=os.pipe()
if os.fork()==0:
    os.close(r)
    os.setsid()
    os.write(w,b'x')
    os.close(w)
    # Cancellation may kill the launcher as soon as readiness is visible.
    # Finish the handshake first, then publish a complete PID atomically.
    with open(sys.argv[1]+'.tmp','w') as f: f.write(str(os.getpid()))
    os.replace(sys.argv[1]+'.tmp',sys.argv[1])
    time.sleep(30)
    os._exit(0)
os.close(w)
os.read(r,1)
while not os.path.exists(sys.argv[1]): time.sleep(0.001)
print('ready',flush=True)
if sys.argv[2]!='exit': time.sleep(30)
os._exit(0)
"#,
            )
            .unwrap();
            let command = format!("python3 '{}' '{}' {mode}", script.display(), pidfile.display());
            let cancel = Arc::new(AtomicBool::new(false));
            let flag = cancel.clone();
            let ready = pidfile.clone();
            let notifier = std::thread::spawn(move || {
                if mode == "cancel" {
                    let until = Instant::now() + Duration::from_secs(10);
                    while !ready.exists() && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            });
            let started = Instant::now();
            let timeout = if mode == "timeout" { Duration::from_secs(2) } else { Duration::from_secs(10) };
            let (status, _, stderr) = run_cancellable(&command, timeout, &cancel).unwrap();
            notifier.join().unwrap();
            assert!(started.elapsed() < Duration::from_secs(8), "{mode} retained a reader");
            assert_eq!(status, None, "incomplete capture cannot establish success");
            assert!(stderr.contains("output capture incomplete"), "{mode}: {stderr}");
            let pid = fs::read_to_string(&pidfile).unwrap().parse::<i32>().unwrap();
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                0,
                "escaped process should still hold pipes during the assertion"
            );
        }
    }

    #[test]
    fn an_mcp_tool_run_from_bash_gets_pointed_back_to_the_tool() {
        let out = bash(&json!({"command": "mcp__simcloud__db_credentials --env prod"})).unwrap();
        assert!(out.contains("exit code: 127") && out.contains("tool calls, not shell commands"), "{out}");
        assert!(!bash(&json!({"command": "nosuchcommand"})).unwrap().contains("mcp__"));
    }

    #[test]
    fn a_lone_cd_is_recognised() {
        assert!(only_changes_directory("cd test_rotate"));
        assert!(only_changes_directory("cd /tmp/x && cd sub"));
        for cmd in ["cd x && ls", "ls", "cd $(mktemp -d)", "cd x; ./rotate.sh . 2", ""] {
            assert!(!only_changes_directory(cmd), "{cmd}");
        }
    }

    #[test]
    fn a_command_that_only_prints_is_recognised() {
        assert!(only_prints("echo \"Task completed: all tests pass.\" && exit 0"));
        assert!(only_prints("echo done\nprintf '%s\\n' ok"));
        for cmd in [
            "",
            "echo hi > notes.txt",
            "echo $(rm -rf x)",
            "echo `id`",
            "echo $PATH",
            "echo hi | sh",
            "echo a && python3 test.py",
            "cat rotate.sh",
        ] {
            assert!(!only_prints(cmd), "{cmd}");
        }
    }

    #[test]
    fn a_script_header_says_how_much_follows_its_first_line() {
        let bash = |c: &str| summary("bash", &serde_json::json!({ "command": c }));
        assert_eq!(bash("cargo test"), "cargo test");
        assert_eq!(bash("cd /tmp/x\nrm -f todo.json\n\npython3 todo.py add a"), "cd /tmp/x  (+2 lines)");
        assert_eq!(bash("cd /tmp/x\nls\n"), "cd /tmp/x  (+1 line)");
        assert!(bash(&format!("{}\nls", "x".repeat(300))).ends_with("(+1 line)"));
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard(b"*.rs", b"main.rs"));
        assert!(!wildcard(b"*.rs", b"src/main.rs"));
        assert!(wildcard(b"src/**/*.rs", b"src/a/b/c.rs"));
        assert!(wildcard(b"src/**/*.rs", b"src/c.rs"));
        assert!(wildcard(b"**/test_?.py", b"x/y/test_a.py"));
        assert!(!wildcard(b"test_?.py", b"test_ab.py"));
    }

    #[test]
    fn outline_finds_definitions() {
        let dir = std::env::temp_dir().join(format!("rusty-outline-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let f = dir.join("m.py");
        fs::write(&f, "import os\n\nclass Cache:\n    def get(self, k):\n        return 1\n\ndef main():\n    pass\n")
            .unwrap();
        let o = outline_file(&f).unwrap();
        assert!(o.contains("3: class Cache"));
        assert!(o.contains("4:     def get(self, k)"));
        assert!(o.contains("7: def main()"));
        let _ = fs::remove_dir_all(dir);
    }
}
