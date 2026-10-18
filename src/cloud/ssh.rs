//! Attach tools to an existing Linux host through the user's OpenSSH client.
//! No provisioning, credentials upload, command replay, or lifecycle ownership.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use super::hybrid::{self, HybridArgs};
use super::Remote;

#[derive(clap::Args)]
#[command(group = clap::ArgGroup::new("ssh_task").multiple(false))]
pub struct Args {
    /// SSH destination or configured host alias; authenticate with ssh first
    #[arg(long)]
    pub host: String,
    /// Optional OpenSSH config file (otherwise use the normal user config)
    #[arg(long)]
    pub ssh_config: Option<PathBuf>,
    /// Absolute project directory on the Linux host, with rusty on PATH
    #[arg(long)]
    pub workspace: String,
    #[arg(long, default_value = "target/debug/rusty")]
    pub local_binary: PathBuf,
    #[arg(long, group = "ssh_task")]
    pub goal: Option<String>,
    #[arg(long, group = "ssh_task")]
    pub prompt: Option<String>,
    #[arg(long, default_value = "auto", value_parser = ["auto", "careful", "standard", "vibe"])]
    pub mode: String,
    #[arg(long, default_value = "auto", value_parser = ["off", "sub", "swarm", "auto"])]
    pub agents: String,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=8))]
    pub swarm_max: u8,
    #[arg(long)]
    pub model: Option<String>,
    #[arg(long, default_value = "learn", value_parser = ["off", "recall", "learn", "reflect", "deep"])]
    pub memory_mode: String,
    #[arg(long, default_value = "auto", value_parser = ["read-only", "ask", "auto", "yolo"])]
    pub permissions: String,
    #[arg(long)]
    pub stats: bool,
    #[arg(long)]
    pub trajectory: Option<PathBuf>,
    #[arg(long = "continue")]
    pub resume: bool,
    #[arg(long)]
    pub project_id: Option<String>,
}

pub fn attach(args: Args) -> Result<i32> {
    let mut remote = Ssh::new(&args.host)?;
    remote.config = args.ssh_config;
    let remote = std::sync::Arc::new(remote);
    let args = HybridArgs {
        sandbox: args.host,
        workspace: args.workspace,
        local_binary: args.local_binary,
        goal: args.goal,
        prompt: args.prompt,
        mode: args.mode,
        agents: args.agents,
        swarm_max: args.swarm_max,
        model: args.model,
        memory_mode: args.memory_mode,
        permissions: args.permissions,
        stats: args.stats,
        trajectory: args.trajectory,
        resume: args.resume,
        project_id: args.project_id,
    };
    hybrid::attach_backend("started", remote, &args, "ssh", &mut hybrid::run_child)
}

pub struct Ssh {
    host: String,
    config: Option<PathBuf>,
    active: AtomicUsize,
}

impl Ssh {
    pub fn new(host: &str) -> Result<Self> {
        if host.is_empty()
            || host.starts_with('-')
            || !host.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-@[]:".contains(&b))
        {
            bail!("--host must be an SSH hostname, user@host, or configured alias");
        }
        Ok(Self { host: host.into(), config: None, active: AtomicUsize::new(0) })
    }

    fn acquire(&self, deadline: Instant) -> Result<ConnectionSlot<'_>> {
        loop {
            if Instant::now() >= deadline {
                bail!("SSH connection queue timed out; no command started");
            }
            if self.active.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| (n < 4).then_some(n + 1)).is_ok() {
                return Ok(ConnectionSlot(&self.active));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn command(&self, timeout: u64) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.process_group(0);
        // Keep local authentication/config available but do not inherit model
        // keys or arbitrary SendEnv values from the controller.
        cmd.env_clear();
        for key in ["HOME", "PATH", "USER", "LOGNAME", "SSH_AUTH_SOCK"] {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
        cmd.args([
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oConnectTimeout=15",
            "-oForwardAgent=no",
            "-oClearAllForwardings=yes",
            "-oSendEnv=-*",
            "-oServerAliveInterval=15",
            "-oServerAliveCountMax=2",
        ]);
        if let Some(config) = &self.config {
            cmd.arg("-F").arg(config);
        }
        cmd.arg(&self.host).arg(format!("timeout --signal=TERM --kill-after=5s {timeout}s bash -s"));
        cmd
    }
}

impl Remote for Ssh {
    fn id(&self) -> &str {
        &self.host
    }
    fn labels(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn sh(&self, script: &str, timeout: u64) -> Result<(i32, String)> {
        let deadline = Instant::now() + Duration::from_secs(timeout.saturating_add(20));
        let _slot = self.acquire(deadline)?;
        run_command(self.command(timeout), script, deadline)
    }

    fn start(&self, _: &str, _: &str) -> Result<String> {
        bail!("SSH supports attached tools only")
    }
    fn upload(&self, _: &Path, _: &str) -> Result<()> {
        bail!("SSH supports attached tools only")
    }
    fn download(&self, _: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Limit this attachment to four simultaneous SSH handshakes/connections.
#[derive(Debug)]
struct ConnectionSlot<'a>(&'a AtomicUsize);
impl Drop for ConnectionSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Release);
    }
}

fn nonblocking(fd: impl AsRawFd) -> std::io::Result<()> {
    let fd = fd.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Observe without releasing the PID/PGID. Only this Child owner may reap it.
fn exited_unreaped(pid: u32) -> std::io::Result<bool> {
    loop {
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let result = unsafe {
            libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT)
        };
        if result == 0 {
            return Ok(unsafe { info.si_pid() } != 0);
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn finish_child(child: &mut std::process::Child) -> Result<std::process::ExitStatus> {
    // A successful waitid proves ownership without reaping. On ECHILD or any
    // other observation error, never signal a potentially recycled PID/PGID.
    exited_unreaped(child.id())?;
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    Ok(child.wait()?)
}

fn wait_child(child: &mut std::process::Child, deadline: Instant) -> Result<std::process::ExitStatus> {
    loop {
        if exited_unreaped(child.id())? {
            return finish_child(child);
        }
        if Instant::now() >= deadline {
            finish_child(child)?;
            bail!("SSH command timed out; remote outcome is unknown");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn run_command(mut command: Command, script: &str, deadline: Instant) -> Result<(i32, String)> {
    // Auto-reaping or another SIGCHLD handler would invalidate group ownership.
    // Refuse it rather than changing process-wide signal policy.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    if action.sa_sigaction != libc::SIG_DFL || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        bail!("SSH requires default SIGCHLD handling to retain child ownership");
    }
    command.process_group(0);
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start ssh")?;
    let mut input = child.stdin.take().context("missing SSH stdin")?;
    let mut output = child.stdout.take().context("missing SSH stdout")?;
    if let Err(error) = nonblocking(&input).and_then(|_| nonblocking(&output)) {
        let _ = finish_child(&mut child);
        return Err(error.into());
    }
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let stopped = &stop;
        let writer = scope.spawn(move || -> std::io::Result<()> {
            let mut remaining = script.as_bytes();
            while !remaining.is_empty() {
                match input.write(remaining) {
                    Ok(0) => return Err(std::io::ErrorKind::WriteZero.into()),
                    Ok(n) => remaining = &remaining[n..],
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if stopped.load(Ordering::Acquire) {
                            return Err(std::io::ErrorKind::BrokenPipe.into());
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        });
        // Move stdin into the writer so it closes as soon as the script ends.
        let reader = scope.spawn(move || -> std::io::Result<Vec<u8>> {
            let mut bytes = Vec::new();
            let mut buffer = [0u8; 8192];
            while bytes.len() <= super::bridge::MAX_RPC {
                match output.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => bytes.extend_from_slice(&buffer[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(bytes)
        });
        let status = wait_child(&mut child, deadline);
        stop.store(true, Ordering::Release);
        let written = writer.join();
        let bytes = reader.join();
        let code = status?.code().unwrap_or(255);
        if code == 255 {
            bail!("SSH exited 255: connection or remote-command failure; verify authentication and host access; remote outcome is unknown");
        }
        let written = written.map_err(|_| anyhow::anyhow!("SSH input failed"))?;
        if code == 0 {
            if let Err(error) = written {
                // A remote command can exit without consuming the entire script.
                if error.kind() != std::io::ErrorKind::BrokenPipe {
                    return Err(error.into());
                }
            }
        }
        let bytes = bytes.map_err(|_| anyhow::anyhow!("SSH output failed"))??;
        if bytes.len() > super::bridge::MAX_RPC {
            bail!("SSH tool response exceeds 1 MiB");
        }
        Ok((code, String::from_utf8(bytes).context("invalid SSH tool response")?))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_a_single_destination_not_an_option_or_shell_expression() {
        for host in ["box", "user@box.example", "user@[::1]"] {
            assert!(Ssh::new(host).is_ok());
        }
        for host in ["", "-oProxyCommand=bad", "box;id", "box\n", "box space", "$(id)"] {
            assert!(Ssh::new(host).is_err());
        }
        let ssh = Ssh::new("user@box").unwrap();
        let cmd = ssh.command(42);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert!(args.contains(&"-oStrictHostKeyChecking=yes"));
        assert!(args.contains(&"-oForwardAgent=no"));
        assert_eq!(args[args.len() - 2], "user@box");
        assert_eq!(args.last().unwrap(), &"timeout --signal=TERM --kill-after=5s 42s bash -s");
    }
    fn shell(source: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", source]);
        command
    }

    #[test]
    fn exit_status_wins_over_a_broken_stdin_pipe() {
        let script = "x".repeat(2 * 1024 * 1024);
        for code in [0, 124, 127, 255] {
            let started = Instant::now();
            let result = run_command(
                shell(&format!("sleep 30 & printf done; exit {code}")),
                &script,
                started + Duration::from_secs(5),
            );
            assert!(started.elapsed() < Duration::from_secs(3));
            if code == 255 {
                assert!(result.unwrap_err().to_string().contains("SSH exited 255"));
            } else {
                assert_eq!(result.unwrap(), (code, "done".into()));
            }
        }
    }

    #[test]
    fn observation_keeps_child_waitable_until_group_cleanup() {
        let mut command = shell("exit 0");
        command.process_group(0);
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !exited_unreaped(child.id()).unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(exited_unreaped(child.id()).unwrap());
        assert!(finish_child(&mut child).unwrap().success());
        assert_eq!(exited_unreaped(child.id()).unwrap_err().raw_os_error(), Some(libc::ECHILD));
        // Cleanup after reaping must fail before trying to signal the old group.
        assert!(finish_child(&mut child).is_err());
    }

    #[test]
    fn deadline_bounds_blocked_input_and_descendant_output() {
        let started = Instant::now();
        let error =
            run_command(shell("sleep 30 & wait"), &"x".repeat(2 * 1024 * 1024), started + Duration::from_millis(100))
                .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn nine_callers_share_four_connection_slots() {
        let ssh = Ssh::new("box").unwrap();
        let slots: Vec<_> = (0..4).map(|_| ssh.acquire(Instant::now() + Duration::from_secs(5)).unwrap()).collect();
        assert!(ssh.acquire(Instant::now()).unwrap_err().to_string().contains("no command started"));
        drop(slots);
        assert!(ssh.acquire(Instant::now()).is_err(), "an expired request must not start even with free slots");
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let barrier = std::sync::Barrier::new(9);
        std::thread::scope(|scope| {
            for _ in 0..9 {
                scope.spawn(|| {
                    barrier.wait();
                    let _slot = ssh.acquire(Instant::now() + Duration::from_secs(5)).unwrap();
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(current, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(20));
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert!(peak.load(Ordering::SeqCst) <= 4);
        assert_eq!(ssh.active.load(Ordering::SeqCst), 0);
    }
    #[test]
    #[ignore = "requires a disposable OpenSSH server and installed Linux rusty; run by ssh-network CI"]
    fn real_openssh_network() {
        use serde_json::json;
        let mut ssh = Ssh::new("rusty-ci").unwrap();
        ssh.config = Some(std::env::var_os("RUSTY_TEST_SSH_CONFIG").expect("SSH config").into());
        let workspace = std::env::var("RUSTY_TEST_SSH_WORKSPACE").expect("workspace");
        std::thread::scope(|scope| {
            for _ in 0..9 {
                let ssh = &ssh;
                scope.spawn(move || assert_eq!(ssh.sh("sleep 0.1; printf network", 5).unwrap(), (0, "network".into())));
            }
        });
        for (name, args) in [
            ("write_file", json!({"path":"network.txt", "content":"network works\n"})),
            ("read_file", json!({"path":"network.txt"})),
            ("bash", json!({"command":"test \"$(cat network.txt)\" = \"network works\"", "timeout_secs":5})),
        ] {
            let response = super::super::bridge::remote_rpc(
                &ssh,
                &workspace,
                &json!({
                    "op":"execute", "name":name, "args":args,
                    "policy":{"mode":"yolo"}, "approval":"Allow"
                }),
            )
            .unwrap();
            assert_eq!(response["ok"], true, "{name}: {response}");
        }
        assert_eq!(ssh.sh("sleep 10", 1).unwrap().0, 124);
        assert_eq!(ssh.sh("command_that_does_not_exist", 5).unwrap().0, 127);
        // Revoke access in this disposable fixture and ensure it fails closed.
        std::fs::write(std::env::var_os("RUSTY_TEST_SSH_AUTHORIZED_KEYS").unwrap(), "").unwrap();
        assert!(ssh.sh("printf must-not-run", 5).unwrap_err().to_string().contains("255"));
    }
}
