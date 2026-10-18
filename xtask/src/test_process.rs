//! Bounded captures for Cargo and libtest; no reader threads outlive a child.
use anyhow::{Context, Result};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    OnceLock,
};
use std::time::{Duration, Instant};

static CANCELLED: AtomicBool = AtomicBool::new(false);
extern "C" fn cancel(_: libc::c_int) {
    CANCELLED.store(true, Ordering::SeqCst);
}
pub fn cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}
fn install_signals() -> Result<()> {
    static INSTALLED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let result = INSTALLED.get_or_init(|| unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = cancel as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        for sig in [libc::SIGINT, libc::SIGTERM] {
            if libc::sigaction(sig, &action, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error().to_string());
            }
        }
        Ok(())
    });
    result.as_ref().map_err(|error| anyhow::anyhow!("installing capture cancellation: {error}"))?;
    Ok(())
}

pub struct Capture {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub problem: Option<String>,
    pub cleanup_error: Option<String>,
    pub cleanup_notes: Vec<String>,
    pub pid: u32,
}
impl Capture {
    pub fn success(&self) -> bool {
        self.status.success() && self.problem.is_none() && self.cleanup_error.is_none()
    }
}

struct Owned {
    child: Child,
    may_signal: bool,
    reap_started: bool,
}
impl Owned {
    fn new(child: Child) -> Self {
        Self { child, may_signal: true, reap_started: false }
    }
    fn exited(&mut self) -> Result<bool> {
        anyhow::ensure!(!self.reap_started, "child ownership already transferred to wait");
        // WNOWAIT reserves the PID through the final cancellation signal.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } < 0
        {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.may_signal = false;
                self.reap_started = true;
            }
            return Err(error).context("observing owned child exit");
        }
        let pid = unsafe { info.si_pid() };
        anyhow::ensure!(pid == 0 || pid == self.child.id() as libc::pid_t, "unexpected child exit identity");
        Ok(pid != 0)
    }
    fn signal(&mut self, sig: libc::c_int, notes: &mut Vec<String>) {
        if !self.may_signal || self.reap_started {
            return;
        }
        if unsafe { libc::kill(-(self.child.id() as i32), sig) } < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                self.may_signal = false;
            } else {
                notes.push(format!("signal {sig}: {error}"));
            }
        }
    }
    fn stop(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.may_signal || self.reap_started {
            return notes;
        }
        if let Err(error) = self.exited() {
            notes.push(format!("child ownership before termination: {error}"));
            return notes;
        }
        self.signal(libc::SIGTERM, &mut notes);
        let end = Instant::now() + Duration::from_secs(2);
        while self.may_signal && Instant::now() < end {
            if matches!(group_present(self.child.id()), Ok(false)) {
                self.may_signal = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if self.may_signal && !self.reap_started {
            if let Err(error) = self.exited() {
                notes.push(format!("child ownership before final kill: {error}"));
                return notes;
            }
            match group_present(self.child.id()) {
                Ok(true) => self.signal(libc::SIGKILL, &mut notes),
                Ok(false) => self.may_signal = false,
                Err(error) => notes.push(format!("group observation before final kill: {error}")),
            }
        }
        notes
    }
    fn reap(&mut self) -> Result<ExitStatus> {
        anyhow::ensure!(!self.reap_started, "child wait already started");
        if !self.exited()? {
            // Still an unreaped direct child, even if group observation was
            // denied. A direct kill does not rely on an unverified group.
            self.child.kill().context("stopping owned direct child")?;
            let end = Instant::now() + Duration::from_secs(2);
            while !self.exited()? {
                anyhow::ensure!(Instant::now() < end, "owned child exit remains unverified after kill");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        self.may_signal = false;
        self.reap_started = true;
        Ok(self.child.wait()?)
    }
    fn finish(mut self) -> Result<ExitStatus> {
        self.reap()
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.reap_started {
            let _ = self.stop();
            if let Err(error) = self.reap() {
                eprintln!("owned child {} cleanup failed: {error}", self.child.id());
            }
            if !matches!(group_absent(self.child.id()), Ok(true)) {
                eprintln!("cleanup of owned process group {} remains unverified", self.child.id());
            }
        }
    }
}

fn group_present(pid: u32) -> Result<bool> {
    if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(false)
    } else {
        Err(error.into())
    }
}
fn group_absent(pid: u32) -> Result<bool> {
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        let observation = group_present(pid);
        if matches!(observation, Ok(false)) {
            return Ok(true);
        }
        if Instant::now() >= end {
            return observation.map(|_| false);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn nonblocking(pipe: &impl AsRawFd) -> Result<()> {
    let fd = pipe.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error()).context("setting capture pipe nonblocking");
    }
    Ok(())
}
fn drain<R: Read>(pipe: &mut Option<R>, bytes: &mut Vec<u8>, limit: usize) -> Result<()> {
    let Some(reader) = pipe else { return Ok(()) };
    let mut buffer = [0; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => {
                *pipe = None;
                return Ok(());
            }
            Ok(n) => {
                let remaining = limit.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buffer[..n.min(remaining)]);
                anyhow::ensure!(n <= remaining, "captured output exceeds limit");
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
}

/// Each child owns a new process group. Fixtures that deliberately leave that
/// group must supply their own cleanup; they cannot inherit these output pipes.
pub fn capture(command: &mut Command, limit: usize, timeout: Duration) -> Result<Capture> {
    install_signals()?;
    let mut disposition: libc::sigaction = unsafe { std::mem::zeroed() };
    if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut disposition) } < 0 {
        return Err(std::io::Error::last_os_error()).context("checking child ownership disposition");
    }
    anyhow::ensure!(
        disposition.sa_sigaction == libc::SIG_DFL,
        "capture requires exclusive child waiting with default SIGCHLD"
    );
    anyhow::ensure!(!cancelled(), "capture interrupted before launch");
    let end = Instant::now() + timeout;
    let mut owned = Owned::new(
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn()?,
    );
    let pid = owned.child.id();
    let mut stdout = owned.child.stdout.take();
    let mut stderr = owned.child.stderr.take();
    nonblocking(stdout.as_ref().unwrap())?;
    nonblocking(stderr.as_ref().unwrap())?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut exited = None;
    let mut status = false;
    let problem = loop {
        if cancelled() {
            break Some("capture interrupted".into());
        }
        if Instant::now() >= end {
            break Some("child exceeded capture deadline".into());
        }
        if let Err(error) = drain(&mut stdout, &mut out, limit.saturating_sub(err.len()))
            .and_then(|_| drain(&mut stderr, &mut err, limit.saturating_sub(out.len())))
        {
            break Some(error.to_string());
        }
        if !status {
            status = owned.exited()?;
            if status {
                exited = Some(Instant::now());
            }
        }
        // Completion observed after cancellation or deadline is incomplete,
        // even if the child exited successfully while this thread was delayed.
        if cancelled() {
            break Some("capture interrupted".into());
        }
        if Instant::now() >= end {
            break Some("child exceeded capture deadline".into());
        }
        if status && stdout.is_none() && stderr.is_none() {
            break None;
        }
        if exited.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(1)) {
            break Some("child exited with output pipes still open".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let cleanup_notes = if problem.is_some() { owned.stop() } else { Vec::new() };
    let status = owned.finish()?;
    // Observation only after reap: never signal a potentially recycled ID.
    let cleanup_error = match group_absent(pid) {
        Ok(true) => None,
        Ok(false) => Some(format!("owned process group {pid} remains after reap window")),
        Err(error) => Some(format!("process group {pid} absence unverified: {error}")),
    };
    Ok(Capture { status, stdout: out, stderr: err, problem, cleanup_error, cleanup_notes, pid })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shell(script: &str, limit: usize, timeout: Duration) -> Capture {
        capture(Command::new("/bin/sh").args(["-c", script]), limit, timeout).unwrap()
    }
    fn reaped(pid: u32) {
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1, "owned leader remains");
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
        assert!(group_absent(pid).unwrap(), "owned group remains");
    }
    struct Temp(std::path::PathBuf);
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn signal_capture_child() {
        let Some(root) = std::env::var_os("RUSTY_CAPTURE_SIGNAL_PROBE") else { return };
        let root = std::path::PathBuf::from(root);
        let result = capture(
            Command::new("/bin/sh")
                .args(["-c", r#"printf '%s' "$$" > "$RUSTY_CAPTURE_READY.next"; /bin/mv "$RUSTY_CAPTURE_READY.next" "$RUSTY_CAPTURE_READY"; exec /bin/sleep 60"#])
                .env("RUSTY_CAPTURE_READY", root.join("ready")),
            4096,
            Duration::from_secs(120),
        )
        .unwrap();
        assert_eq!(result.problem.as_deref(), Some("capture interrupted"));
        reaped(result.pid);
    }
    #[test]
    fn signals_cancel_and_reap_nested_capture_groups() {
        for sig in [libc::SIGINT, libc::SIGTERM] {
            let root = Temp(std::env::temp_dir().join(format!("rusty-capture-signal-{}-{sig}", std::process::id())));
            std::fs::create_dir(&root.0).unwrap();
            let log = std::fs::File::create(root.0.join("log")).unwrap();
            let mut helper = Owned::new(
                Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "test_process::tests::signal_capture_child",
                        "--nocapture",
                        "--test-threads",
                        "1",
                    ])
                    .env("RUSTY_CAPTURE_SIGNAL_PROBE", &root.0)
                    .stdin(Stdio::null())
                    .stdout(log.try_clone().unwrap())
                    .stderr(log)
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
            let end = Instant::now() + Duration::from_secs(60);
            while !root.0.join("ready").exists() {
                assert!(!helper.exited().unwrap(), "signal helper exited before readiness");
                assert!(Instant::now() < end, "signal helper did not publish readiness");
                std::thread::sleep(Duration::from_millis(5));
            }
            let captured_pid = std::fs::read_to_string(root.0.join("ready")).unwrap().parse().unwrap();
            assert_eq!(unsafe { libc::kill(helper.child.id() as i32, sig) }, 0);
            let end = Instant::now() + Duration::from_secs(60);
            while !helper.exited().unwrap() {
                assert!(Instant::now() < end, "signal helper did not finish cleanup");
                std::thread::sleep(Duration::from_millis(5));
            }
            let helper_pid = helper.child.id();
            let status = helper.finish().unwrap();
            let mut text = String::new();
            std::fs::File::open(root.0.join("log")).unwrap().take(64 * 1024).read_to_string(&mut text).unwrap();
            assert!(status.success(), "{text}");
            assert!(group_absent(helper_pid).unwrap(), "helper group remains after completion");
            reaped(captured_pid);
        }
    }
    #[test]
    #[allow(clippy::zombie_processes, reason = "the outer capture must detect and clean up this held-pipe descendant")]
    fn held_pipe_child() {
        let Some(path) = std::env::var_os("RUSTY_CAPTURE_HELD_PIPE") else { return };
        // Spawn waits for exec readiness. Deliberately keep inherited pipes
        // open past this helper's exit, for the parent capture to clean up.
        let child = Command::new("/bin/sleep").arg("60").spawn().unwrap();
        std::fs::write(path, child.id().to_string()).unwrap();
    }
    #[test]
    fn exited_harness_cannot_hide_a_descendant_holding_output() {
        let root = Temp(std::env::temp_dir().join(format!("rusty-capture-held-{}", std::process::id())));
        std::fs::create_dir(&root.0).unwrap();
        let result = capture(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "test_process::tests::held_pipe_child", "--nocapture", "--test-threads", "1"])
                .env("RUSTY_CAPTURE_HELD_PIPE", root.0.join("pid")),
            4096,
            Duration::from_secs(60),
        )
        .unwrap();
        assert!(result.status.success(), "helper must exit zero to exercise held pipes");
        assert!(result.problem.as_deref().is_some_and(|s| s.contains("pipes still open")));
        assert!(result.cleanup_error.is_none(), "{:?}", result.cleanup_error);
        let descendant = std::fs::read_to_string(root.0.join("pid")).unwrap().parse().unwrap();
        reaped(result.pid);
        reaped(descendant);
    }
    #[test]
    fn exit_observation_preserves_ownership_until_consuming_finish() {
        let mut owned = Owned::new(Command::new("/bin/sh").args(["-c", "exit 0"]).process_group(0).spawn().unwrap());
        let pid = owned.child.id();
        let end = Instant::now() + Duration::from_secs(60);
        while !owned.exited().unwrap() {
            assert!(Instant::now() < end, "owned child did not exit");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(owned.exited().unwrap(), "repeat WNOWAIT observation must remain available");
        assert!(owned.finish().unwrap().success());
        // The consumed owner has no remaining method that can signal pid.
        reaped(pid);
    }
    #[test]
    fn output_limit_kills_and_reaps_the_owned_writer() {
        let result = shell("while :; do printf '0123456789'; done", 4096, Duration::from_secs(60));
        assert!(!result.success());
        assert!(result.problem.unwrap().contains("output exceeds"));
        assert!(result.stdout.len() + result.stderr.len() <= 4096);
        reaped(result.pid);
    }
    #[test]
    fn already_expired_deadline_cannot_accept_a_successful_command() {
        let result = capture(Command::new("/bin/sh").args(["-c", "exit 0"]), 4096, Duration::ZERO).unwrap();
        assert!(!result.success());
        assert_eq!(result.problem.as_deref(), Some("child exceeded capture deadline"));
        assert!(result.cleanup_error.is_none());
        reaped(result.pid);
    }
    #[test]
    fn deadline_kills_and_reaps_the_owned_child() {
        let result = shell("exec /bin/sleep 60", 4096, Duration::from_millis(50));
        assert!(!result.success());
        assert!(result.problem.unwrap().contains("deadline"));
        reaped(result.pid);
    }
    #[test]
    fn ordinary_exit_is_not_assisted_and_preserves_both_streams() {
        let result = shell("printf output; printf diagnostic >&2", 4096, Duration::from_secs(60));
        assert!(result.success());
        assert_eq!(result.stdout, b"output");
        assert_eq!(result.stderr, b"diagnostic");
        reaped(result.pid);
    }
}
