//! A tracked run of rusty inside a Daytona sandbox: start, follow, export,
//! clean up. Every step works through the `Remote` trait, so the same code
//! runs against a real sandbox or a local stand-in.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::digest::{sha256_hex, Sha256};
use super::image::{BASE_IMAGE, PACKAGES};
use super::shell::{join, quote};
use super::{Remote, Sandboxes};

/// Paths inside the sandbox; the shell expands $HOME.
pub const WORK: &str = "$HOME/work";
pub const RUN: &str = "$HOME/rusty-run";
pub const EXPORTS: &[&str] = &[
    "patch.diff",
    "new-files.txt",
    "new-files.tgz",
    "out.log",
    "stderr.log",
    "trajectory.json",
    "session.tgz",
    "memory.json.gz",
    "exit",
];
/// Without these the sandbox is never deleted.
const ESSENTIAL: &[&str] = &["patch.diff", "out.log", "stderr.log", "trajectory.json", "exit"];
const KEY_PREFIXES: &[&str] = &["NVIDIA_API_KEY", "RUSTY_"];
/// Settings that describe this machine, never the sandbox.
const LOCAL_ONLY: &[&str] = &["RUSTY_HOME", "RUSTY_TOOLS", "RUSTY_TOOL_BRIDGE_URL", "RUSTY_TOOL_BRIDGE_TOKEN"];

/// `cloud-runs/<run-id>/run.json`, readable by the earlier Python launcher.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct RunState {
    pub id: String,
    pub repo: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub mode: String,
    pub agents: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_input: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started: Option<String>,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cmd_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log_offset: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exported: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_dir: Option<String>,
    /// Anything else a newer or older launcher wrote.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

impl RunState {
    pub fn memory(&self) -> &str {
        self.memory_mode.as_deref().unwrap_or("learn")
    }

    fn remembers(&self) -> bool {
        crate::advisor::Level::parse(self.memory()).is_some_and(crate::advisor::Level::advises)
    }

    /// `RUSTY_PROJECT_ID=... ` for commands that need the memory scope.
    fn project_prefix(&self) -> String {
        let id = self.project_id.as_deref().filter(|p| !p.is_empty()).unwrap_or(&self.repo);
        format!("RUSTY_PROJECT_ID={} ", quote(id))
    }
}

/// Writes a private file (0600) in a private directory (0700), refusing to
/// follow a symlink.
pub fn private_write(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        crate::privacy::private_dir(parent)?;
    }
    let mut f = crate::privacy::private_file(path, false)?;
    f.write_all(data)?;
    Ok(())
}

/// Where run state and exports live (`cloud-runs/` in the current directory).
#[derive(Clone, Debug)]
pub struct Runs {
    pub root: PathBuf,
}

impl Default for Runs {
    fn default() -> Self {
        Self { root: PathBuf::from("cloud-runs") }
    }
}

impl Runs {
    pub fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    pub fn save(&self, state: &RunState) -> Result<()> {
        let text = serde_json::to_string_pretty(state)? + "\n";
        private_write(&self.dir(&state.id).join("run.json"), text.as_bytes())
    }

    pub fn load(&self, id: &str) -> Result<RunState> {
        if id.is_empty() || id.contains('/') || id.starts_with('.') {
            bail!("invalid run id {id:?}");
        }
        let path = self.dir(id).join("run.json");
        let text = std::fs::read_to_string(&path)
            .map_err(|_| anyhow::anyhow!("no run {id} in {}/ (see `list`)", self.root.display()))?;
        serde_json::from_str(&text).with_context(|| format!("reading {}", path.display()))
    }

    /// Every saved run, oldest id first.
    pub fn all(&self) -> Vec<RunState> {
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(&self.root)
            .map(|d| d.filter_map(|e| e.ok().map(|e| e.path().join("run.json"))).filter(|p| p.is_file()).collect())
            .unwrap_or_default();
        dirs.sort();
        dirs.iter().filter_map(|p| serde_json::from_str(&std::fs::read_to_string(p).ok()?).ok()).collect()
    }
}

/// Model keys and rusty settings for the sandbox. They travel only as the
/// sandbox's environment variables, never on a command line.
pub fn model_env(vars: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
    vars.into_iter()
        .filter(|(k, _)| KEY_PREFIXES.iter().any(|p| k.starts_with(p)) && !LOCAL_ONLY.contains(&k.as_str()))
        .collect()
}

// ---------------------------------------------------------------- snapshot

pub fn is_snapshot_name(name: &str) -> bool {
    name.len() == 18
        && name.starts_with("rusty-")
        && name[6..].bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `rusty-<12 hex>` from everything that shapes the environment: the base
/// image, resources, region, packages, the binary and its memory companion.
pub fn snapshot_name(binary: &Path, cpu: u8, memory: u8, target: &str) -> Result<String> {
    let mut h = Sha256::default();
    h.update(BASE_IMAGE.as_bytes());
    h.update(format!("{cpu}:{memory}").as_bytes());
    h.update(target.as_bytes());
    h.update(PACKAGES.join(" ").as_bytes());
    super::digest::hash_file(binary, |bytes| h.update(bytes))
        .with_context(|| format!("reading {}", binary.display()))?;
    let advisor = binary.with_file_name("rusty-memoryd");
    if advisor.is_file() {
        super::digest::hash_file(&advisor, |bytes| h.update(bytes))?;
    }
    Ok(format!("rusty-{}", &h.hex()[..12]))
}

pub fn find_binary(arg: Option<&Path>) -> Result<PathBuf> {
    let binary = arg.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("out/rusty"));
    if !binary.is_file() {
        bail!(
            "{} not found. Build the static Linux binary first:\n  docker build --target bin -o out .",
            binary.display()
        );
    }
    Ok(binary)
}

// --------------------------------------------------------------------- run

/// Checks HTTPS reachability of the model endpoint from the sandbox without
/// sending model credentials. An unauthenticated 401 proves transport, not
/// credential validity.
pub fn check_endpoint(remote: &dyn Remote) -> Result<()> {
    let script = r#"url="${RUSTY_BASE_URL:-https://integrate.api.nvidia.com/v1}"
while [ "${url%/}" != "$url" ]; do url="${url%/}"; done
code=$(curl -sS -L -o /dev/null --max-time 10 -w '%{http_code}' "$url/models" 2>/dev/null) || exit 1
case "$code" in 2??|401) exit 0 ;; *) exit 1 ;; esac"#;
    let (code, _) = remote.sh(script, 15)?;
    if code != 0 {
        bail!(
            "model endpoint is unreachable from this sandbox; no model calls started. \
             For NVIDIA on Daytona Tier 1/2, request an organization network exception \
             or upgrade to Tier 3: https://www.daytona.io/docs/en/network-limits/"
        );
    }
    Ok(())
}

/// Clones the project, records where it started, and starts rusty.
pub fn setup_and_start(remote: &dyn Remote, runs: &Runs, state: &mut RunState, task: &[String]) -> Result<()> {
    let checkout = match state.git_ref.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => format!(" && git checkout -q {}", quote(r)),
        None => String::new(),
    };
    let (code, out) = remote.sh(
        &format!(
            "rm -rf {WORK} {RUN} && mkdir -p {RUN} && git clone -q {} {WORK} && cd {WORK}{checkout} && git rev-parse HEAD",
            quote(&state.repo)
        ),
        900,
    )?;
    if code != 0 {
        bail!("clone failed:\n{out}");
    }
    state.base_commit = Some(out.trim().lines().last().unwrap_or_default().to_string());
    let memory = state.memory().to_string();
    let prefix = state.project_prefix();
    if state.remembers() {
        let (code, _) = remote.sh("command -v rusty-memoryd", 30)?;
        if code != 0 {
            bail!("snapshot lacks rusty-memoryd; rebuild and prepare it");
        }
    }
    if let Some(input) = state.memory_input.clone().filter(|i| !i.is_empty()) {
        remote.upload(Path::new(&input), &format!("{RUN}/memory-input.json.gz"))?;
        let (code, _) =
            remote.sh(&format!("cd {WORK} && {prefix}rusty-memoryd import {RUN}/memory-input.json.gz"), 30)?;
        if code != 0 {
            bail!("memory import failed; check explicit project identity and snapshot scope");
        }
    }
    let mut words: Vec<String> =
        ["rusty", "--yolo", "--stats", "--mode", &state.mode, "--agents", &state.agents, "--memory", &memory]
            .map(String::from)
            .to_vec();
    if let Some(model) = state.model.as_deref().filter(|m| !m.is_empty()) {
        words.extend(["--model".into(), model.into()]);
    }
    // The trajectory path stays unquoted so the sandbox shell expands $HOME.
    let rusty = format!("{} --trajectory {RUN}/trajectory.json {}", join(&words), join(task));
    state.command = Some(rusty.trim_end().to_string());
    state.cmd_id = Some(remote.start(
        &format!(
            "cd {WORK} && {prefix}NO_COLOR=1 {} </dev/null >{RUN}/out.log 2>{RUN}/stderr.log; echo $? >{RUN}/exit",
            rusty.trim_end()
        ),
        "rusty",
    )?);
    state.status = "running".into();
    runs.save(state)
}

/// rusty's exit status, once its receipt exists.
pub fn exit_code(remote: &dyn Remote) -> Result<Option<i32>> {
    let (code, out) = remote.sh(&format!("cat {RUN}/exit 2>/dev/null"), 60)?;
    let out = out.trim();
    let digits = out.strip_prefix('-').unwrap_or(out);
    Ok((code == 0 && !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| out.parse().ok())
        .flatten())
}

/// Set by SIGINT while a run is followed.
pub static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Streams new output until rusty exits. An interrupt stops following only:
/// the offset is saved and the run keeps going. Returns None when stopped.
pub fn follow(
    remote: &dyn Remote,
    runs: &Runs,
    state: &mut RunState,
    poll: Duration,
    out: &mut dyn Write,
    stop: &dyn Fn() -> bool,
) -> Result<Option<i32>> {
    let mut offset = state.log_offset.unwrap_or(0);
    let mut drain = |offset: &mut u64| -> Result<()> {
        let (_, chunk) = remote.sh(&format!("tail -c +{} {RUN}/out.log 2>/dev/null", *offset + 1), 60)?;
        if !chunk.is_empty() {
            out.write_all(chunk.as_bytes())?;
            out.flush()?;
            *offset += chunk.len() as u64;
        }
        Ok(())
    };
    loop {
        drain(&mut offset)?;
        if let Some(code) = exit_code(remote)? {
            // Output written between the read and the exit check.
            drain(&mut offset)?;
            state.log_offset = Some(offset);
            return Ok(Some(code));
        }
        let deadline = std::time::Instant::now() + poll;
        while std::time::Instant::now() < deadline {
            if stop() {
                state.log_offset = Some(offset);
                runs.save(state)?;
                eprintln!(
                    "\n☾ still running in {}. Pick it up with:\n  rusty-cloud logs {}\n  rusty-cloud stop {}   (exports first)",
                    state.sandbox.as_deref().unwrap_or("its sandbox"),
                    state.id,
                    state.id
                );
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(20).min(poll));
        }
    }
}

/// Stops on the first SIGINT since the last call.
pub fn interrupted() -> bool {
    INTERRUPTED.swap(false, Ordering::SeqCst)
}

/// Downloads the patch, new files, logs and session state. True if every
/// essential export (and the memory snapshot, when enabled) arrived.
pub fn export(remote: &dyn Remote, runs: &Runs, state: &mut RunState) -> Result<bool> {
    let mut memory_ok = true;
    if state.remembers() {
        let (code, _) = remote.sh(
            &format!(
                "cd {WORK} && rm -f {RUN}/memory.json.gz && {}rusty-memoryd export {RUN}/memory.json.gz",
                state.project_prefix()
            ),
            30,
        )?;
        memory_ok = code == 0;
    }
    let base = quote(state.base_commit.as_deref().context("the run has no starting commit")?);
    let (code, out) = remote.sh(
        &format!(
            "cd {WORK} && git add -A && git diff --cached --binary {base} >{RUN}/patch.diff && \
             git diff --cached --name-only --diff-filter=A {base} >{RUN}/new-files.txt && \
             COPYFILE_DISABLE=1 tar czf {RUN}/new-files.tgz -T {RUN}/new-files.txt && \
             {{ COPYFILE_DISABLE=1 tar czf {RUN}/session.tgz -C $HOME --exclude='.env' --exclude='*.env' \
             --exclude='.config/rusty/memory' .config/rusty 2>/dev/null || true; }}"
        ),
        600,
    )?;
    if code != 0 {
        eprintln!("☾ export failed in the sandbox:\n{out}");
        return Ok(false);
    }
    let dest = runs.dir(&state.id);
    let mut manifest = serde_json::Map::new();
    for name in EXPORTS {
        let Some(data) = remote.download(&format!("{RUN}/{name}")) else { continue };
        private_write(&dest.join(name), &data)?;
        manifest.insert((*name).into(), json!({"bytes": data.len(), "sha256": sha256_hex(&data)}));
    }
    private_write(&dest.join("manifest.json"), (serde_json::to_string_pretty(&manifest)? + "\n").as_bytes())?;
    let mut ok = ESSENTIAL.iter().all(|n| manifest.contains_key(*n)) && memory_ok;
    if state.remembers() {
        ok = ok && manifest.contains_key("memory.json.gz");
    }
    state.exported = Some(ok);
    state.export_dir = Some(dest.display().to_string());
    runs.save(state)?;
    Ok(ok)
}

/// Exports, then deletes the sandbox only after a confirmed exit and a
/// complete export. Returns rusty's exit code (1 when unknown).
pub fn finish(
    sandboxes: &dyn Sandboxes,
    remote: &dyn Remote,
    runs: &Runs,
    state: &mut RunState,
    keep: bool,
) -> Result<i32> {
    let code = exit_code(remote)?;
    state.exit_code = code;
    state.status = if code.is_some() { "finished" } else { "running" }.into();
    runs.save(state)?;
    let ok = export(remote, runs, state)?;
    let dest = runs.dir(&state.id);
    if ok {
        eprintln!("\n☾ exported to {}/ (apply with: git apply {}/patch.diff)", dest.display(), dest.display());
    }
    let sandbox = state.sandbox.clone().unwrap_or_default();
    if keep || !ok {
        let reason = if keep { "kept (--keep)" } else { "kept because the export failed" };
        eprintln!("☾ sandbox {sandbox} {reason}");
        return Ok(code.unwrap_or(1));
    }
    let Some(code) = code else {
        eprintln!("☾ sandbox kept: no confirmed exit receipt");
        return Ok(1);
    };
    delete_sandbox(sandboxes, remote, runs, state)?;
    Ok(code)
}

/// Deletes the sandbox only if its labels say this run created it.
pub fn delete_sandbox(sandboxes: &dyn Sandboxes, remote: &dyn Remote, runs: &Runs, state: &mut RunState) -> Result<()> {
    let labels = remote.labels();
    let sandbox = state.sandbox.clone().unwrap_or_else(|| remote.id().to_string());
    if labels.get("app").map(String::as_str) == Some("rusty")
        && labels.get("rusty-run").map(String::as_str) == Some(state.id.as_str())
    {
        sandboxes.delete_sandbox(remote.id())?;
        state.status = "deleted".into();
        runs.save(state)?;
        eprintln!("☾ sandbox {sandbox} deleted");
    } else {
        eprintln!("☾ not deleting {sandbox}: its labels don't match run {}", state.id);
    }
    Ok(())
}

// ------------------------------------------------------------------- time

/// Local time through strftime(3).
pub fn local_time(format: &str) -> String {
    let fmt = std::ffi::CString::new(format).unwrap_or_default();
    let mut buf = [0u8; 64];
    // SAFETY: localtime_r and strftime write only into the buffers given.
    let n = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        libc::strftime(buf.as_mut_ptr().cast(), buf.len(), fmt.as_ptr(), &tm)
    };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_names_and_env() {
        assert!(is_snapshot_name("rusty-0123456789ab"));
        assert!(!is_snapshot_name("rusty-0123456789aB"));
        assert!(!is_snapshot_name("rusty-0123456789abc"));
        assert!(!is_snapshot_name("other-0123456789ab"));
        let env = model_env(
            [
                ("NVIDIA_API_KEY", "model-canary"),
                ("RUSTY_TOOL_BRIDGE_URL", "http://127.0.0.1:1"),
                ("RUSTY_TOOL_BRIDGE_TOKEN", "bridge-canary"),
                ("RUSTY_TOOLS", "daytona"),
                ("RUSTY_HOME", "local-home"),
                ("RUSTY_MODEL", "m"),
                ("DAYTONA_API_KEY", "never"),
                ("PATH", "/bin"),
            ]
            .map(|(k, v)| (k.to_string(), v.to_string())),
        );
        assert_eq!(
            env,
            BTreeMap::from([("NVIDIA_API_KEY".into(), "model-canary".into()), ("RUSTY_MODEL".into(), "m".into())])
        );
    }

    /// Same names as the earlier Python launcher, so prepared snapshots are
    /// still found (values computed with it).
    #[test]
    fn snapshot_names_match_earlier_launcher() {
        let dir = std::env::temp_dir().join(format!("rusty-snapname-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let binary = dir.join("rusty");
        std::fs::write(&binary, "one").unwrap();
        assert_eq!(snapshot_name(&binary, 2, 4, "").unwrap(), "rusty-ce16d0bc93af");
        assert_eq!(snapshot_name(&binary, 4, 8, "").unwrap(), "rusty-7e51cc994e83");
        std::fs::write(dir.join("rusty-memoryd"), "advisor").unwrap();
        assert_eq!(snapshot_name(&binary, 2, 4, "").unwrap(), "rusty-a7fd7f6bed21");
        assert_ne!(snapshot_name(&binary, 2, 4, "eu").unwrap(), "rusty-a7fd7f6bed21");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn state_files_from_the_python_launcher_still_load() {
        let old = r#"{"id": "20250101-000000-abcd", "repo": "https://x", "ref": null, "snapshot": "rusty-0123456789ab",
            "target": null, "mode": "auto", "agents": "off", "model": null, "memory_mode": "legacy", "memory_input": null,
            "project_id": "https://x", "task": "t", "started": "2025", "status": "running", "sandbox": "sbx",
            "base_commit": "abc", "command": "rusty", "cmd_id": "c", "log_offset": 12, "future": 1}"#;
        let s: RunState = serde_json::from_str(old).unwrap();
        assert_eq!(s.log_offset, Some(12));
        assert_eq!(s.memory(), "legacy");
        assert_eq!(serde_json::to_value(&s).unwrap()["future"], 1);
        assert!(local_time("%Y").len() == 4);
    }
}
