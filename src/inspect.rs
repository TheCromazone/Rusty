//! Bounded, read-only inspection for the native client. No provider/config loading.
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const MAX_BYTES: usize = 16 * 1024 * 1024;
const MAX_GIT_BYTES: usize = 4 * 1024 * 1024;
const MAX_LINE: usize = 64 * 1024;
const PAGE: usize = 200;
const MAX_RESPONSE: usize = 256 * 1024;
const MAX_ENTRIES: usize = 50_000;

fn hash(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn number(p: &Value, key: &str, default: usize) -> Result<usize, String> {
    match p.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| format!("{key} must be a nonnegative integer")),
    }
}
fn page(p: &Value) -> Result<(usize, usize), String> {
    Ok((number(p, "offset", 0)?, number(p, "limit", PAGE)?.clamp(1, PAGE)))
}
fn checked_generation(p: &Value, generation: &str) -> Result<(), String> {
    if let Some(expected) = p.get("expected_generation") {
        if expected.as_str() != Some(generation) {
            return Err("stale generation; refresh snapshot".into());
        }
    }
    Ok(())
}
fn relative(p: &Value) -> Result<&str, String> {
    p.get("path").map_or(Ok(""), |v| v.as_str().ok_or_else(|| "path must be a string".into()))
}
/// Reject all symlink components, including links whose targets remain inside root.
/// The inspector is read-only; it never resolves a link chosen by the client.
fn confined(path: &str) -> Result<(), String> {
    for part in Path::new(path).components() {
        match part {
            Component::Normal(s) if !s.as_encoded_bytes().eq_ignore_ascii_case(b".git") => (),
            Component::CurDir => (),
            _ => return Err("path must stay within the project root and outside Git metadata".into()),
        }
    }
    Ok(())
}
#[cfg(unix)]
fn git_inode(root: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    fs::symlink_metadata(root.join(".git")).is_ok_and(|git| git.dev() == metadata.dev() && git.ino() == metadata.ino())
}
#[cfg(not(unix))]
fn git_inode(_: &Path, _: &fs::Metadata) -> bool {
    false
}
fn resolve(root: &Path, path: &str) -> Result<PathBuf, String> {
    confined(path)?;
    let mut out = root.to_path_buf();
    for part in Path::new(path).components() {
        if let Component::Normal(s) = part {
            out.push(s);
            match fs::symlink_metadata(&out) {
                Ok(m) if m.file_type().is_symlink() => return Err("symlink traversal refused".into()),
                Ok(m) if git_inode(root, &m) => return Err("Git metadata access refused".into()),
                Ok(_) => (),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
                Err(_) => return Err("cannot inspect path".into()),
            }
        }
    }
    Ok(out)
}
#[cfg(unix)]
fn open_scoped(root: &Path, path: &str, directory: bool) -> Result<fs::File, String> {
    confined(path)?;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let mut current = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)
        .map_err(|_| "cannot open project root")?;
    let parts: Vec<_> = Path::new(path).components().filter(|c| *c != Component::CurDir).collect();
    for (i, part) in parts.iter().enumerate() {
        let Component::Normal(name) = part else {
            return Err("invalid relative path".into());
        };
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| "invalid path")?;
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_CLOEXEC
            | libc::O_NONBLOCK
            | if directory || i + 1 < parts.len() { libc::O_DIRECTORY } else { 0 };
        // Each component is opened relative to an already owned directory fd.
        // Renaming an ancestor or swapping a symlink cannot redirect traversal.
        let fd = unsafe { libc::openat(current.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err("cannot open scoped path (symlinks refused)".into());
        }
        current = unsafe { fs::File::from_raw_fd(fd) };
        if git_inode(root, &current.metadata().map_err(|_| "cannot stat scoped path")?) {
            return Err("Git metadata access refused".into());
        }
    }
    Ok(current)
}
fn read_file(root: &Path, path: &str) -> Result<Vec<u8>, String> {
    resolve(root, path)?;
    #[cfg(unix)]
    let mut f = open_scoped(root, path, false)?;
    #[cfg(not(unix))]
    let mut f = fs::File::open(resolve(root, path)?).map_err(|_| "cannot open file")?;
    let before = f.metadata().map_err(|_| "cannot stat file")?;
    if !before.is_file() {
        return Err("path is not a regular file".into());
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut f).take((MAX_BYTES + 1) as u64).read_to_end(&mut bytes).map_err(|_| "cannot read file")?;
    let after = f.metadata().map_err(|_| "cannot stat file")?;
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Err("file changed during inspection; retry".into());
    }
    if bytes.len() > MAX_BYTES {
        return Err("file exceeds 16 MiB inspection limit".into());
    }
    Ok(bytes)
}
#[cfg(unix)]
fn directory_entries(root: &Path, path: &str) -> Result<Vec<Value>, String> {
    use std::os::fd::{AsRawFd, IntoRawFd};
    let directory = open_scoped(root, path, true)?;
    let fd = directory.as_raw_fd();
    // fdopendir takes ownership only on success. The guard closes on all exits.
    let raw = unsafe { libc::fdopendir(fd) };
    if raw.is_null() {
        return Err("cannot read directory".into());
    }
    let _ = directory.into_raw_fd();
    struct Dir(*mut libc::DIR);
    impl Drop for Dir {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let guard = Dir(raw);
    let mut entries = Vec::new();
    loop {
        let entry = unsafe { libc::readdir(guard.0) };
        if entry.is_null() {
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
        if [b".".as_slice(), b".."].contains(&name.to_bytes()) || name.to_bytes().eq_ignore_ascii_case(b".git") {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            return Err("directory exceeds 50000 entry limit".into());
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstatat(fd, name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err("directory changed during inspection; refresh".into());
        }
        let stat = unsafe { stat.assume_init() };
        let entry_name = name.to_str().map_err(|_| "non-UTF-8 filename unsupported")?;
        if fs::symlink_metadata(root.join(path).join(entry_name)).is_ok_and(|m| git_inode(root, &m)) {
            continue;
        }
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFLNK => "symlink",
            libc::S_IFDIR => "directory",
            libc::S_IFREG => "file",
            _ => "special",
        };
        let name = name.to_str().map_err(|_| "non-UTF-8 filename unsupported")?;
        entries.push(json!({"name":name,"kind":kind,"size":stat.st_size.max(0) as u64}));
    }
    Ok(entries)
}
#[cfg(not(unix))]
fn directory_entries(root: &Path, path: &str) -> Result<Vec<Value>, String> {
    let mut entries = Vec::new();
    for e in fs::read_dir(resolve(root, path)?).map_err(|_| "cannot read directory")? {
        let e = e.map_err(|_| "cannot read entry")?;
        if e.file_name().as_encoded_bytes().eq_ignore_ascii_case(b".git") {
            continue;
        }
        if entries.len() >= MAX_ENTRIES {
            return Err("directory too large".into());
        }
        let m = fs::symlink_metadata(e.path()).map_err(|_| "cannot stat entry")?;
        let name = e.file_name().into_string().map_err(|_| "non-UTF-8 filename unsupported")?;
        let kind = if m.file_type().is_symlink() {
            "symlink"
        } else if m.is_dir() {
            "directory"
        } else if m.is_file() {
            "file"
        } else {
            "special"
        };
        entries.push(json!({"name":name,"kind":kind,"size":m.len()}));
    }
    Ok(entries)
}

/// Git gets no shell, pager, external diff or textconv. stdout is capped while
/// being read, and a stuck child is killed after five seconds.
fn git(root: &Path, args: &[&str], allow_one: bool) -> Result<Vec<u8>, String> {
    // Git diff also runs clean/process filters while reading working files.
    // --no-textconv alone does not disable these repository-configured programs.
    let mut overrides = Vec::new();
    if matches!(args.first(), Some(&"diff") | Some(&"status")) {
        let keys = git(
            root,
            &["config", "--null", "--name-only", "--get-regexp", "^filter\\..*\\.(clean|process|required)$"],
            true,
        )?;
        for key in keys.split(|&b| b == 0).filter(|v| !v.is_empty()) {
            let key = std::str::from_utf8(key).map_err(|_| "invalid filter configuration")?;
            overrides.push("-c".to_string());
            overrides.push(format!("{key}={}", if key.ends_with(".required") { "false" } else { "" }));
        }
    }
    let mut child = Command::new("git")
        .args(["--no-pager", "--literal-pathspecs", "-c", "core.fsmonitor=false", "-c", "core.untrackedCache=false"])
        .args(overrides)
        .args(args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .env_remove("GIT_DIR")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "cannot start git")?;
    let stdout = child.stdout.take().ok_or("git stdout unavailable")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout.take((MAX_GIT_BYTES + 1) as u64).read_to_end(&mut bytes);
        let _ = tx.send((result, bytes));
    });
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let result = rx.recv_timeout(Duration::from_secs(5));
    let (io, bytes) = match result {
        Ok(v) => v,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("git inspection timed out".into());
        }
    };
    if bytes.len() > MAX_GIT_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Err("diff exceeds 4 MiB inspection limit".into());
    }
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|_| "cannot wait for git")? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("git inspection timed out".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    io.map_err(|_| "cannot read git output")?;
    if !status.success() && !(allow_one && status.code() == Some(1)) {
        return Err("git inspection failed (repository or revision unavailable)".into());
    }
    Ok(bytes)
}

pub fn handle(root: &Path, method: &str, params: &Value) -> Result<Value, String> {
    if !params.is_object() {
        return Err("params must be an object".into());
    }
    let root = root.canonicalize().map_err(|_| "project root unavailable")?;
    if !root.is_dir() {
        return Err("project root is not a directory".into());
    }
    let response = match method {
        "tree.snapshot" => tree(&root, params),
        "file.read" => file(&root, params),
        "diff.expand" => expand(&root, params),
        "diff.files" => files(&root, params),
        "diff.hunks" => hunks(&root, params),
        _ => Err("unsupported inspection method".into()),
    }?;
    if encoded_len(&response)? > MAX_RESPONSE {
        return Err("inspection response exceeds 256 KiB limit".into());
    }
    Ok(response)
}
fn encoded_len(value: &Value) -> Result<usize, String> {
    serde_json::to_vec(value).map(|v| v.len()).map_err(|e| e.to_string())
}
/// Reserve the largest possible continuation before adding items. Count the
/// actual JSON encoding, including escaping, brackets and separators.
fn bounded_page(
    mut response: Value,
    key: &str,
    items: impl IntoIterator<Item = Value>,
    offset: usize,
    total: usize,
    next_key: &str,
) -> Result<Value, String> {
    response[key] = json!([]);
    response[next_key] = json!(usize::MAX);
    let mut used = encoded_len(&response)?;
    if used >= MAX_RESPONSE {
        return Err("inspection metadata exceeds response limit".into());
    }
    let mut selected = Vec::new();
    for mut item in items {
        let comma = usize::from(!selected.is_empty());
        let available = MAX_RESPONSE.saturating_sub(used + comma);
        let mut size = encoded_len(&item)?;
        if size > available {
            if !selected.is_empty() {
                break;
            }
            truncate_row(&mut item, available)?;
            size = encoded_len(&item)?;
        }
        used += size + comma;
        selected.push(item);
    }
    let next = offset.saturating_add(selected.len());
    response[key] = json!(selected);
    response[next_key] = json!((next < total).then_some(next));
    Ok(response)
}
/// A single escaped line must still advance its page. Truncation is explicit,
/// and intraline offsets are discarded when they no longer describe the text.
fn truncate_row(row: &mut Value, budget: usize) -> Result<(), String> {
    let text = row["text"].as_str().ok_or("inspection item exceeds response limit")?.to_string();
    row["text"] = json!("");
    row["truncated"] = json!(true);
    if row.get("intraline").is_some() {
        row["intraline"] = Value::Null;
    }
    let overhead = encoded_len(row)?;
    if overhead >= budget {
        return Err("inspection row metadata exceeds response limit".into());
    }
    let mut remaining = budget - overhead;
    let mut end = 0;
    for ch in text.chars() {
        let cost = match ch {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{08}' | '\u{0c}' => 2,
            '\u{00}'..='\u{1f}' => 6,
            _ => ch.len_utf8(),
        };
        if cost > remaining {
            break;
        }
        remaining -= cost;
        end += ch.len_utf8();
    }
    row["text"] = json!(&text[..end]);
    Ok(())
}

fn tree(root: &Path, p: &Value) -> Result<Value, String> {
    resolve(root, relative(p)?)?;
    let mut entries = directory_entries(root, relative(p)?)?;
    entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    let generation = hash(&serde_json::to_vec(&entries).map_err(|e| e.to_string())?);
    checked_generation(p, &generation)?;
    let (offset, limit) = page(p)?;
    bounded_page(
        json!({"path":relative(p)?,"generation":generation,"total":entries.len()}),
        "entries",
        entries.iter().skip(offset).take(limit).cloned(),
        offset,
        entries.len(),
        "next_offset",
    )
}
fn file(root: &Path, p: &Value) -> Result<Value, String> {
    let bytes = read_file(root, relative(p)?)?;
    let generation = hash(&bytes);
    checked_generation(p, &generation)?;
    let binary = bytes.contains(&0) || std::str::from_utf8(&bytes).is_err();
    if binary {
        return Ok(json!({"path":relative(p)?,"generation":generation,"binary":true,"size":bytes.len(),"lines":[]}));
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| "invalid UTF-8")?;
    let start = if p.get("line_offset").is_some() {
        if p.get("start_line").is_some() {
            return Err("use start_line or line_offset, not both".into());
        }
        number(p, "line_offset", 0)?.checked_add(1).ok_or("line_offset overflow")?
    } else {
        number(p, "start_line", 1)?.max(1)
    };
    let limit = if p.get("line_limit").is_some() {
        if p.get("limit").is_some() {
            return Err("use limit or line_limit, not both".into());
        }
        number(p, "line_limit", PAGE)?.clamp(1, PAGE)
    } else {
        number(p, "limit", PAGE)?.clamp(1, PAGE)
    };
    let mut lines = Vec::new();
    for (i, line) in text.split_inclusive('\n').enumerate() {
        let number = i + 1;
        if number >= start && lines.len() < limit {
            let mut end = line.len().min(MAX_LINE);
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            lines.push(json!({"number":number,"text":&line[..end],"truncated":end<line.len()}));
        }
    }
    // Count independently of the requested output page.
    let total = text.bytes().filter(|&b| b == b'\n').count() + usize::from(!text.is_empty() && !text.ends_with('\n'));
    bounded_page(
        json!({"path":relative(p)?,"generation":generation,"binary":false,"size":bytes.len(),"total_lines":total,"final_newline":text.ends_with('\n')}),
        "lines",
        lines,
        start,
        total.saturating_add(1),
        "next_line",
    )
}
#[derive(Clone)]
struct Changed {
    path: String,
    old_path: Option<String>,
    status: String,
    kind: &'static str,
}
fn changed(root: &Path) -> Result<(Vec<Changed>, String), String> {
    let top = git(root, &["rev-parse", "--show-toplevel"], false)?;
    let top = std::str::from_utf8(&top).map_err(|_| "non-UTF-8 repository root unsupported")?;
    // show-toplevel terminates with one newline; preserve newlines in path names.
    let top =
        Path::new(top.strip_suffix('\n').unwrap_or(top)).canonicalize().map_err(|_| "repository root unavailable")?;
    if top != root {
        return Err("diff inspection requires the Git repository root".into());
    }
    let bytes =
        git(root, &["status", "--porcelain=v1", "-z", "--untracked-files=all", "--ignore-submodules=all"], false)?;
    let mut stamp = bytes.clone();
    let mut fields = bytes.split(|&b| b == 0).filter(|b| !b.is_empty());
    let mut entries = Vec::new();
    while let Some(field) = fields.next() {
        if field.len() < 4 {
            return Err("invalid git status output".into());
        }
        let status = std::str::from_utf8(&field[..2]).map_err(|_| "invalid git status")?.to_string();
        let path = std::str::from_utf8(&field[3..]).map_err(|_| "non-UTF-8 filename unsupported")?.to_string();
        confined(&path)?;
        let old_path = if status.contains(['R', 'C']) {
            let old = fields.next().ok_or("missing rename source")?;
            let old = std::str::from_utf8(old).map_err(|_| "non-UTF-8 filename unsupported")?.to_string();
            confined(&old)?;
            Some(old)
        } else {
            None
        };
        if let Ok(meta) = fs::symlink_metadata(root.join(&path)) {
            stamp.extend_from_slice(&meta.len().to_le_bytes());
            if let Ok(time) =
                meta.modified().and_then(|v| v.duration_since(std::time::UNIX_EPOCH).map_err(std::io::Error::other))
            {
                stamp.extend_from_slice(&time.as_nanos().to_le_bytes());
            }
        }
        let kind = if fs::symlink_metadata(root.join(&path)).is_ok_and(|m| m.file_type().is_symlink()) {
            "symlink"
        } else {
            "file"
        };
        entries.push(Changed { path, old_path, status, kind });
        if entries.len() > MAX_ENTRIES {
            return Err("too many changed files".into());
        }
    }
    Ok((entries, hash(&stamp)))
}
fn files(root: &Path, p: &Value) -> Result<Value, String> {
    let (entries, generation) = changed(root)?;
    checked_generation(p, &generation)?;
    let (offset, limit) = page(p)?;
    let values = entries
        .iter()
        .skip(offset)
        .take(limit)
        .map(|e| json!({"path":e.path,"old_path":e.old_path,"status":e.status,"kind":e.kind}));
    bounded_page(
        json!({"generation":generation,"total":entries.len()}),
        "files",
        values,
        offset,
        entries.len(),
        "next_offset",
    )
}

fn patch(root: &Path, path: &str) -> Result<Vec<u8>, String> {
    let (entries, _) = changed(root)?;
    let entry = entries.iter().find(|e| e.path == path).ok_or("path has no working-tree changes")?;
    resolve(root, path)?;
    if entry.status == "??" || git(root, &["rev-parse", "--verify", "HEAD"], false).is_err() {
        return git(
            root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                "--unified=3",
                "--no-index",
                "--",
                "/dev/null",
                path,
            ],
            true,
        );
    }
    let mut args = vec![
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
        "--unified=3",
        "--find-renames=1%",
        "--ignore-submodules=all",
        "HEAD",
        "--",
        path,
    ];
    if let Some(old) = entry.old_path.as_deref() {
        args.push(old);
    }
    git(root, &args, false)
}
/// A single changed interval, aligned to Unicode scalar boundaries. This is
/// deliberately linear rather than quadratic LCS; disjoint edits coalesce.
fn spans(a: &str, b: &str) -> (Value, Value) {
    if a.len() > MAX_LINE || b.len() > MAX_LINE {
        return (Value::Null, Value::Null);
    }
    let mut prefix = 0;
    for (x, y) in a.chars().zip(b.chars()) {
        if x != y {
            break;
        }
        prefix += x.len_utf8();
    }
    let mut suffix = 0;
    for (x, y) in a[prefix..].chars().rev().zip(b[prefix..].chars().rev()) {
        if x != y {
            break;
        }
        suffix += x.len_utf8();
    }
    (json!([{"start":prefix,"end":a.len()-suffix}]), json!([{"start":prefix,"end":b.len()-suffix}]))
}
fn range(s: &str) -> Result<(usize, usize), String> {
    let mut p = s.get(1..).ok_or("invalid hunk range")?.split(',');
    let start = p.next().ok_or("invalid hunk range")?.parse().map_err(|_| "invalid hunk range")?;
    let count = p.next().map_or(Ok(1), |v| v.parse().map_err(|_| "invalid hunk range"))?;
    Ok((start, count))
}
fn parse_patch(bytes: &[u8]) -> Result<(Vec<Value>, bool), String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "non-UTF-8 patch unsupported")?;
    let mut hunks: Vec<Value> = Vec::new();
    let mut old = 0;
    let mut new = 0;
    let mut binary = false;
    let mut active = false;
    let mut total_rows = 0;
    for line in text.split_terminator('\n') {
        if line.starts_with("diff --git ") {
            active = false;
        }
        if line.starts_with("Binary files ") || line == "GIT binary patch" {
            binary = true;
        }
        if line.starts_with("@@ ") {
            let mut parts = line.split_whitespace();
            parts.next();
            let (a, ac) = range(parts.next().ok_or("invalid hunk")?)?;
            let (b, bc) = range(parts.next().ok_or("invalid hunk")?)?;
            active = true;
            old = a;
            new = b;
            if hunks.len() >= 10_000 {
                return Err("diff exceeds 10000 hunk inspection limit".into());
            }
            hunks.push(json!({"old_start":a,"old_count":ac,"new_start":b,"new_count":bc,"rows":[]}));
            continue;
        }
        if !active {
            continue;
        }
        let Some(hunk) = hunks.last_mut() else { continue };
        let rows = hunk["rows"].as_array_mut().ok_or("invalid rows")?;
        if line == "\\ No newline at end of file" {
            if let Some(row) = rows.last_mut() {
                row["no_newline"] = json!(true);
            }
            continue;
        }
        let (kind, old_line, new_line) = match line.as_bytes().first() {
            Some(b' ') => {
                let r = ("context", Some(old), Some(new));
                old += 1;
                new += 1;
                r
            }
            Some(b'-') => {
                let r = ("deletion", Some(old), None);
                old += 1;
                r
            }
            Some(b'+') => {
                let r = ("addition", None, Some(new));
                new += 1;
                r
            }
            _ => continue,
        };
        let content = &line[1..];
        let mut end = content.len().min(MAX_LINE);
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        total_rows += 1;
        if total_rows > 100_000 {
            return Err("diff exceeds 100000 row inspection limit".into());
        }
        rows.push(json!({"kind":kind,"old_line":old_line,"new_line":new_line,"text":&content[..end],"truncated":end<content.len(),"no_newline":false,"intraline":Value::Null}));
    }
    for hunk in &mut hunks {
        let rows = hunk["rows"].as_array_mut().ok_or("invalid rows")?;
        let mut i = 0;
        while i < rows.len() {
            if rows[i]["kind"] != "deletion" {
                i += 1;
                continue;
            }
            let start = i;
            while i < rows.len() && rows[i]["kind"] == "deletion" {
                i += 1;
            }
            let added = i;
            while i < rows.len() && rows[i]["kind"] == "addition" {
                i += 1;
            }
            for n in 0..(added - start).min(i - added) {
                let a = start + n;
                let b = added + n;
                if rows[a]["truncated"] == true || rows[b]["truncated"] == true {
                    continue;
                }
                let (sa, sb) = spans(
                    rows[a]["text"].as_str().ok_or("invalid text")?,
                    rows[b]["text"].as_str().ok_or("invalid text")?,
                );
                rows[a]["intraline"] = sa;
                rows[b]["intraline"] = sb;
            }
        }
    }
    Ok((hunks, binary))
}
fn unsupported_link(root: &Path, path: &str) -> Result<Option<Value>, String> {
    confined(path)?;
    Ok(fs::symlink_metadata(root.join(path)).ok().filter(|m| m.file_type().is_symlink()).map(|_| {
        json!({"path":path,"kind":"symlink","unsupported":"symlink","reason":"symlink diff is not supported; target is never followed",
            "hunks":[],"rows":[],"total":0,"next_offset":null,"next_cursor":null})
    }))
}
fn hunks(root: &Path, p: &Value) -> Result<Value, String> {
    let path = relative(p)?;
    if let Some(response) = unsupported_link(root, path)? {
        return Ok(response);
    }
    let bytes = patch(root, path)?;
    let generation = hash(&bytes);
    checked_generation(p, &generation)?;
    let (hunks, binary) = parse_patch(&bytes)?;
    let total = hunks.len();
    let (mut offset, limit) = page(p)?;
    if p.get("hunk_index").is_some() {
        if p.get("offset").is_some() {
            return Err("use hunk_index or offset, not both".into());
        }
        offset = number(p, "hunk_index", 0)?;
    }
    let row_offset = number(p, "row_offset", 0)?;
    let row_limit = number(p, "row_limit", PAGE)?.clamp(1, PAGE);
    let mut response = json!({"path":path,"generation":generation,"binary":binary,"hunks":[],"total":total,
        "next_offset":usize::MAX,"next_cursor":{"hunk_index":usize::MAX,"row_offset":usize::MAX},"intraline_algorithm":"unicode-scalar-prefix-suffix"});
    let mut used = encoded_len(&response)?;
    if used >= MAX_RESPONSE {
        return Err("inspection metadata exceeds response limit".into());
    }
    let mut values = Vec::new();
    let mut next_hunk = offset;
    let mut next_row = 0;
    for (index, mut h) in hunks.into_iter().enumerate().skip(offset).take(limit.min(10)) {
        let row_offset = if index == offset { row_offset } else { 0 };
        h["hunk_index"] = json!(index);
        let rows = h["rows"].take().as_array().ok_or("invalid rows")?.clone();
        let row_total = rows.len();
        h["rows"] = json!([]);
        h["total_rows"] = json!(row_total);
        h["next_row_offset"] = json!(usize::MAX);
        let overhead = encoded_len(&h)? + usize::from(!values.is_empty());
        // Do not emit an empty, non-advancing page for a later hunk when an
        // earlier hunk used the shared budget. next_offset points back to it.
        let first_size = rows.get(row_offset).map(encoded_len).transpose()?.unwrap_or(0);
        if !values.is_empty() && used.saturating_add(overhead).saturating_add(first_size) > MAX_RESPONSE {
            break;
        }
        if used.saturating_add(overhead) >= MAX_RESPONSE {
            return Err("hunk metadata exceeds response limit".into());
        }
        used += overhead;
        let mut selected = Vec::new();
        for mut row in rows.into_iter().skip(row_offset).take(row_limit) {
            let comma = usize::from(!selected.is_empty());
            let available = MAX_RESPONSE.saturating_sub(used + comma);
            let mut size = encoded_len(&row)?;
            if size > available {
                if !selected.is_empty() {
                    break;
                }
                truncate_row(&mut row, available)?;
                size = encoded_len(&row)?;
            }
            used += size + comma;
            selected.push(row);
        }
        let next = row_offset.saturating_add(selected.len());
        h["rows"] = json!(selected);
        h["next_row_offset"] = json!((next < row_total).then_some(next));
        values.push(h);
        if next < row_total {
            next_hunk = index;
            next_row = next;
            break;
        }
        next_hunk = index.saturating_add(1);
    }
    response["hunks"] = json!(values);
    response["next_offset"] = json!((next_hunk < total).then_some(next_hunk));
    response["next_cursor"] =
        if next_hunk < total { json!({"hunk_index":next_hunk,"row_offset":next_row}) } else { Value::Null };
    Ok(response)
}

/// Expand working-tree context, preserving old/new line alignment. The caller
/// supplies the diff generation, not a file generation, to reject stale hunks.
fn expand(root: &Path, p: &Value) -> Result<Value, String> {
    if let Some(response) = unsupported_link(root, relative(p)?)? {
        return Ok(response);
    }
    let bytes = patch(root, relative(p)?)?;
    let generation = hash(&bytes);
    checked_generation(p, &generation)?;
    let (hunks, binary) = parse_patch(&bytes)?;
    if binary {
        return Ok(json!({"path":relative(p)?,"generation":generation,"binary":true,"rows":[]}));
    }
    let mut params = p.clone();
    params.as_object_mut().ok_or("params must be object")?.remove("expected_generation");
    let mut content = file(root, &params)?;
    // Verify that content and patch still describe the same working tree.
    if hash(&patch(root, relative(p)?)?) != generation {
        return Err("diff changed during expansion; retry".into());
    }
    let lines = content["lines"].as_array().ok_or("invalid file lines")?;
    let mut rows = Vec::new();
    for line in lines {
        let n = line["number"].as_u64().ok_or("invalid line number")? as i64;
        let mut delta = 0i64;
        let mut matched = None;
        for h in &hunks {
            let start = h["new_start"].as_i64().ok_or("invalid hunk")?;
            let count = h["new_count"].as_i64().ok_or("invalid hunk")?;
            if n < start {
                break;
            }
            if n < start + count {
                matched = h["rows"]
                    .as_array()
                    .ok_or("invalid rows")?
                    .iter()
                    .find(|r| r["new_line"].as_i64() == Some(n))
                    .cloned();
                break;
            }
            delta += count - h["old_count"].as_i64().ok_or("invalid hunk")?;
        }
        let old = matched.as_ref().map_or(json!(n - delta), |r| r["old_line"].clone());
        let kind = matched.as_ref().map_or(json!("context"), |r| r["kind"].clone());
        let text = line["text"].as_str().ok_or("invalid line text")?;
        rows.push(json!({"old_line":old,"new_line":n,"kind":kind,"text":text.strip_suffix('\n').unwrap_or(text),"truncated":line["truncated"],"no_newline":!text.ends_with('\n') && line["truncated"]!=true,"intraline":if line["truncated"] == true { Value::Null } else { matched.as_ref().map_or(Value::Null,|r|r["intraline"].clone()) }}));
    }
    content["content_generation"] = content["generation"].clone();
    content["generation"] = json!(generation);
    content.as_object_mut().ok_or("invalid content")?.remove("lines");
    if rows.is_empty() {
        content["rows"] = json!([]);
        return Ok(content);
    }
    let start = rows.first().and_then(|r| r["new_line"].as_u64()).unwrap_or(1) as usize;
    let total = content["total_lines"].as_u64().unwrap_or(0) as usize;
    bounded_page(content, "rows", rows, start, total.saturating_add(1), "next_line")
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Repo(PathBuf);
    impl Repo {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rusty-inspect-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            let repo = Self(path);
            repo.git(&["init", "-q"]);
            repo.git(&["config", "user.name", "Test"]);
            repo.git(&["config", "user.email", "test@example.invalid"]);
            repo
        }
        fn git(&self, args: &[&str]) {
            assert!(Command::new("git")
                .args(args)
                .current_dir(&self.0)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .unwrap()
                .success());
        }
        fn write(&self, path: &str, text: impl AsRef<[u8]>) {
            fs::write(self.0.join(path), text).unwrap();
        }
        fn baseline(&self) {
            self.git(&["add", "."]);
            self.git(&["commit", "-qm", "baseline"]);
        }
        fn call(&self, method: &str, p: Value) -> Value {
            handle(&self.0, method, &p).unwrap()
        }
    }
    impl Drop for Repo {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn tree_paging_stale_and_scope() {
        let r = Repo::new();
        r.write("a", "a");
        r.write("b", "b");
        let first = r.call("tree.snapshot", json!({"limit":1}));
        assert_eq!(first["entries"][0]["name"], "a");
        assert_eq!(first["next_offset"], 1);
        r.write("c", "c");
        assert!(handle(&r.0, "tree.snapshot", &json!({"expected_generation":first["generation"]})).is_err());
        let absolute = r.0.join("a").display().to_string();
        for path in ["../outside", absolute.as_str(), ".git/config"] {
            assert!(handle(&r.0, "file.read", &json!({"path":path})).is_err());
        }
        #[cfg(unix)]
        {
            let outside = Repo::new();
            outside.write("outside.txt", "out of scope");
            std::os::unix::fs::symlink(&outside.0, r.0.join("link")).unwrap();
            assert!(handle(&r.0, "file.read", &json!({"path":"link/outside.txt"})).is_err());
        }
    }
    #[test]
    fn file_lines_preserve_crlf_unicode_and_missing_newline() {
        let r = Repo::new();
        r.write("a", "α\r\n👩‍💻é\nlast");
        let page = r.call("file.read", json!({"path":"a","start_line":2,"limit":1}));
        assert_eq!(page["lines"][0]["text"], "👩‍💻é\n");
        assert_eq!(page["lines"][0]["number"], 2);
        assert_eq!(page["total_lines"], 3);
        assert_eq!(page["next_line"], 3);
        assert_eq!(page["final_newline"], false);
        r.write("binary", [0, 255, 1]);
        assert_eq!(r.call("file.read", json!({"path":"binary"}))["binary"], true);
        r.write("long", "界".repeat(MAX_LINE));
        let page = r.call("file.read", json!({"path":"long"}));
        assert_eq!(page["lines"][0]["truncated"], true);
        assert!(page["lines"][0]["text"].as_str().unwrap().len() <= MAX_LINE);
    }
    #[test]
    fn git_changes_renames_untracked_binary_and_no_newline() {
        let r = Repo::new();
        r.write("old", "hello α world\nunchanged\nsecond\n");
        r.write("b", [0, 1, 2]);
        r.baseline();
        r.git(&["mv", "old", "new"]);
        r.write("new", "hello β world\nunchanged\nsecond");
        r.write("b", [0, 1, 3]);
        r.write("--odd name", "new\n");
        let list = r.call("diff.files", json!({}));
        let files = list["files"].as_array().unwrap();
        assert!(files.iter().any(|v| v["path"] == "new" && v["old_path"] == "old"));
        assert!(files.iter().any(|v| v["path"] == "--odd name" && v["status"] == "??"));
        let patch = r.call("diff.hunks", json!({"path":"new"}));
        let rows = patch["hunks"][0]["rows"].as_array().unwrap();
        assert!(rows.iter().any(|v| v["no_newline"] == true));
        assert!(rows
            .iter()
            .any(|v| v["text"] == "hello β world" && v["intraline"][0]["start"] == 6 && v["intraline"][0]["end"] == 8));
        assert_eq!(r.call("diff.hunks", json!({"path":"b"}))["binary"], true);
        let untracked = r.call("diff.hunks", json!({"path":"--odd name"}));
        assert_eq!(untracked["hunks"][0]["rows"][0]["kind"], "addition");
        let old = patch["generation"].clone();
        r.write("new", "changed\n");
        assert!(handle(&r.0, "diff.hunks", &json!({"path":"new","expected_generation":old})).is_err());
    }
    #[test]
    fn bounded_rows_and_intraline_scalar_boundaries() {
        let r = Repo::new();
        r.write("a", "base\n");
        r.baseline();
        r.write("a", (0..1000).map(|i| format!("row {i}\n")).collect::<String>());
        let p = r.call("diff.hunks", json!({"path":"a","row_limit":3}));
        assert_eq!(p["hunks"][0]["rows"].as_array().unwrap().len(), 3);
        assert_eq!(p["hunks"][0]["next_row_offset"], 3);
        let (a, b) = spans("👩‍💻 é", "👩‍💻 ê");
        assert_eq!(a[0]["start"], 13);
        assert_eq!(b[0]["end"], 15);
        let (a, b) = spans("same", "same");
        assert_eq!(a[0]["start"], a[0]["end"]);
        assert_eq!(b[0]["start"], b[0]["end"]);
        assert_eq!(r.call("diff.expand", json!({"path":"a","start_line":500,"limit":1}))["rows"][0]["new_line"], 500);
    }
    #[test]
    fn unborn_repo_and_deleted_file() {
        let r = Repo::new();
        r.write("a", "first\n");
        r.git(&["add", "a"]);
        assert_eq!(r.call("diff.hunks", json!({"path":"a"}))["hunks"][0]["rows"][0]["kind"], "addition");
        r.baseline();
        fs::remove_file(r.0.join("a")).unwrap();
        assert_eq!(r.call("diff.hunks", json!({"path":"a"}))["hunks"][0]["rows"][0]["kind"], "deletion");
    }
    #[test]
    fn repo_diff_drivers_cannot_execute_and_literal_paths_stay_scoped() {
        let r = Repo::new();
        r.write(":(glob)*", "one\n");
        r.write("other", "same\n");
        r.baseline();
        r.write(":(glob)*", "two\n");
        r.write("other", "changed\n");
        let marker = r.0.join("driver-ran");
        let command = format!("touch {}", marker.display());
        r.git(&["config", "diff.external", &command]);
        r.git(&["config", "diff.bad.textconv", &command]);
        r.write(".gitattributes", "* diff=bad\n");
        let result = r.call("diff.hunks", json!({"path":":(glob)*"}));
        assert_eq!(result["total"], 1);
        assert!(!marker.exists());
        assert_eq!(result["hunks"][0]["rows"][1]["text"], "two");
        let alias = r.call("file.read", json!({"path":"other","line_offset":0,"line_limit":1}));
        assert_eq!(alias["lines"][0]["number"], 1);
    }
    #[test]
    fn diff_refuses_subdirectory_root_and_file_size_caps() {
        let r = Repo::new();
        r.write("outside", "first\n");
        r.baseline();
        r.write("outside", "second\n");
        fs::create_dir(r.0.join("sub")).unwrap();
        assert!(handle(&r.0.join("sub"), "diff.files", &json!({})).unwrap_err().contains("repository root"));
        let f = fs::File::create(r.0.join("huge")).unwrap();
        f.set_len(MAX_BYTES as u64 + 1).unwrap();
        assert!(handle(&r.0, "file.read", &json!({"path":"huge"})).unwrap_err().contains("16 MiB"));
    }
    #[test]
    fn diff_does_not_run_clean_or_process_filters() {
        let r = Repo::new();
        r.write("a", "before\n");
        r.baseline();
        r.write("a", "after\n");
        let marker = r.0.join("filter-ran");
        let command = format!("touch {}; cat", marker.display());
        r.git(&["config", "filter.evil.clean", &command]);
        r.git(&["config", "filter.evil.required", "true"]);
        r.write(".gitattributes", "a filter=evil\n");
        r.call("diff.hunks", json!({"path":"a"}));
        assert!(!marker.exists());
        r.git(&["config", "filter.evil.process", &command]);
        r.call("diff.hunks", json!({"path":"a"}));
        assert!(!marker.exists());
    }
    #[test]
    fn expanded_context_maps_both_sides_and_rejects_stale_diff() {
        let r = Repo::new();
        let original = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        r.write("a", &original);
        r.baseline();
        r.write("a", format!("inserted\n{original}"));
        let diff = r.call("diff.hunks", json!({"path":"a"}));
        let context = r.call(
            "diff.expand",
            json!({"path":"a","start_line":15,"limit":1,"expected_generation":diff["generation"]}),
        );
        assert_eq!(context["rows"][0]["new_line"], 15);
        assert_eq!(context["rows"][0]["old_line"], 14);
        assert_eq!(context["rows"][0]["text"], "line 14");
        r.write("a", "changed again\n");
        assert!(handle(&r.0, "diff.expand", &json!({"path":"a","expected_generation":diff["generation"]})).is_err());
    }
    #[test]
    fn escaped_file_and_expansion_pages_respect_encoded_cap_and_advance() {
        let r = Repo::new();
        r.write("a", "baseline\n");
        r.baseline();
        let long = format!("{}\n", "\u{01}".repeat(MAX_LINE - 1));
        r.write("a", long.repeat(4));
        for method in ["file.read", "diff.expand"] {
            let key = if method == "file.read" { "lines" } else { "rows" };
            let mut start = 1;
            let mut seen = Vec::new();
            loop {
                let result = r.call(method, json!({"path":"a","start_line":start}));
                assert!(encoded_len(&result).unwrap() <= MAX_RESPONSE);
                let rows = result[key].as_array().unwrap();
                assert!(!rows.is_empty());
                for row in rows {
                    seen.push(row[if key == "lines" { "number" } else { "new_line" }].as_u64().unwrap());
                    assert_eq!(row["truncated"], true);
                    if key == "rows" {
                        assert!(row["intraline"].is_null());
                    }
                }
                let Some(next) = result["next_line"].as_u64() else { break };
                assert!(next > start);
                start = next;
            }
            assert_eq!(seen, vec![1, 2, 3, 4]);
        }
    }
    #[test]
    fn expansion_discards_intraline_when_file_page_truncates_escaped_text() {
        let r = Repo::new();
        let prefix = "\u{01}".repeat(MAX_LINE - 2);
        r.write("a", format!("{prefix}x\n"));
        r.baseline();
        r.write("a", format!("{prefix}y\n"));
        let (parsed, _) = parse_patch(&patch(&r.0.canonicalize().unwrap(), "a").unwrap()).unwrap();
        // The original parsed line fits MAX_LINE; only JSON encoding truncates it.
        assert_eq!(parsed[0]["rows"][1]["truncated"], false);
        assert!(!parsed[0]["rows"][1]["intraline"].is_null());
        let expanded = r.call("diff.expand", json!({"path":"a","limit":1}));
        let row = &expanded["rows"][0];
        assert_eq!(row["truncated"], true);
        assert!(row["intraline"].is_null());
        assert!(row["text"].as_str().unwrap().len() < prefix.len());
    }
    #[test]
    fn large_separated_hunks_and_escaped_rows_traverse_without_stalling() {
        for escaped in [false, true] {
            let r = Repo::new();
            let old = "x".repeat(32 * 1024);
            let new = if escaped { "\u{01}".repeat(MAX_LINE) } else { "y".repeat(32 * 1024) };
            let original = (0..30).map(|i| format!("{i} {old}\n")).collect::<String>();
            r.write("a", &original);
            r.baseline();
            let modified =
                (0..30).map(|i| format!("{i} {}\n", if i % 10 == 4 { &new } else { &old })).collect::<String>();
            r.write("a", modified);
            let mut cursor = json!({"hunk_index":0,"row_offset":0});
            let mut seen = [0usize; 3];
            loop {
                let page = r.call(
                    "diff.hunks",
                    json!({"path":"a","hunk_index":cursor["hunk_index"],"row_offset":cursor["row_offset"]}),
                );
                assert!(encoded_len(&page).unwrap() <= MAX_RESPONSE);
                let hunks = page["hunks"].as_array().unwrap();
                assert!(!hunks.is_empty());
                for h in hunks {
                    let index = h["hunk_index"].as_u64().unwrap() as usize;
                    let rows = h["rows"].as_array().unwrap();
                    assert!(!rows.is_empty());
                    seen[index] += rows.len();
                    if h["next_row_offset"].is_null() {
                        assert_eq!(seen[index], h["total_rows"].as_u64().unwrap() as usize);
                    }
                }
                if page["next_cursor"].is_null() {
                    assert!(page["next_offset"].is_null());
                    break;
                }
                let next = page["next_cursor"].clone();
                assert_eq!(page["next_offset"], next["hunk_index"]);
                assert!(
                    next["hunk_index"].as_u64() > cursor["hunk_index"].as_u64()
                        || (next["hunk_index"] == cursor["hunk_index"]
                            && next["row_offset"].as_u64() > cursor["row_offset"].as_u64())
                );
                cursor = next;
            }
            assert!(seen.iter().all(|&count| count == 8));
        }
    }
    #[test]
    fn escaped_tree_and_changed_file_pages_are_bounded() {
        let r = Repo::new();
        let segment = "\u{01}".repeat(160);
        let mut parent = PathBuf::new();
        for _ in 0..4 {
            parent.push(&segment);
            fs::create_dir(r.0.join(&parent)).unwrap();
        }
        for i in 0..60 {
            r.write(parent.join(format!("{i}{segment}")).to_str().unwrap(), "a");
        }
        for (method, params, key) in
            [("tree.snapshot", json!({"path":parent}), "entries"), ("diff.files", json!({}), "files")]
        {
            let mut offset = 0;
            loop {
                let mut p = params.clone();
                p["offset"] = json!(offset);
                let response = r.call(method, p);
                assert!(encoded_len(&response).unwrap() <= MAX_RESPONSE);
                assert!(!response[key].as_array().unwrap().is_empty());
                let Some(next) = response["next_offset"].as_u64() else { break };
                assert!(next > offset);
                offset = next;
            }
        }
    }
    #[test]
    fn git_metadata_case_and_worktree_file_aliases_are_refused() {
        let r = Repo::new();
        for path in [".git", ".GIT", ".Git/config", ".GIT/config"] {
            for method in ["file.read", "tree.snapshot", "diff.hunks", "diff.expand"] {
                assert!(handle(&r.0, method, &json!({"path":path})).is_err());
            }
        }
        fs::remove_dir_all(r.0.join(".git")).unwrap();
        r.write(".git", "gitdir: private-metadata\n");
        for path in [".git", ".GIT", ".Git"] {
            assert!(handle(&r.0, "file.read", &json!({"path":path})).is_err());
        }
        fs::hard_link(r.0.join(".git"), r.0.join("metadata-alias")).unwrap();
        assert!(handle(&r.0, "file.read", &json!({"path":"metadata-alias"})).is_err());
        let tree = r.call("tree.snapshot", json!({}));
        assert!(tree["entries"].as_array().unwrap().is_empty());
    }
    #[cfg(unix)]
    #[test]
    fn changed_symlinks_are_listed_without_breaking_normal_diffs() {
        use std::os::unix::fs::symlink;
        let r = Repo::new();
        r.write("a", "before\n");
        symlink("a", r.0.join("tracked-link")).unwrap();
        r.baseline();
        r.write("a", "after\n");
        fs::remove_file(r.0.join("tracked-link")).unwrap();
        symlink("../outside", r.0.join("tracked-link")).unwrap();
        symlink("a", r.0.join("untracked-link")).unwrap();
        let files = r.call("diff.files", json!({}));
        for name in ["tracked-link", "untracked-link"] {
            assert!(files["files"].as_array().unwrap().iter().any(|f| f["path"] == name && f["kind"] == "symlink"));
            let diff = r.call("diff.hunks", json!({"path":name}));
            assert_eq!(diff["unsupported"], "symlink");
            assert_eq!(r.call("diff.expand", json!({"path":name}))["unsupported"], "symlink");
        }
        assert_eq!(r.call("diff.hunks", json!({"path":"a"}))["total"], 1);
    }
    #[test]
    fn row_cursor_resumes_only_initial_hunk_and_completes_following_hunks() {
        let r = Repo::new();
        let original = (0..40).map(|i| format!("line{i}\n")).collect::<String>();
        r.write("a", &original);
        r.baseline();
        r.write("a", original.replace("line5\n", "changed5\n").replace("line30\n", "changed30\n"));
        let first = r.call("diff.hunks", json!({"path":"a","row_limit":2}));
        assert_eq!(first["next_offset"], 0);
        assert_eq!(first["next_cursor"], json!({"hunk_index":0,"row_offset":2}));
        let rest = r.call("diff.hunks", json!({"path":"a","row_offset":2}));
        assert_eq!(rest["hunks"][0]["rows"].as_array().unwrap().len(), 6);
        assert_eq!(rest["hunks"][1]["rows"].as_array().unwrap().len(), 8);
        assert_eq!(rest["hunks"][1]["rows"][0]["new_line"], 28);
        assert!(rest["next_offset"].is_null());
        assert!(rest["next_cursor"].is_null());
    }
}
