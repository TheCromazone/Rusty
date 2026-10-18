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
    pub pid: u32,
}
impl Capture {
    pub fn success(&self) -> bool {
        self.status.success() && self.problem.is_none() && self.cleanup_error.is_none()
    }
}

struct Owned(Child, bool);
impl Owned {
    fn stop(&mut self) {
        // Let nested captures handle cancellation before the final kill.
        unsafe { libc::kill(-(self.0.id() as i32), libc::SIGTERM) };
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            let _ = self.0.try_wait();
            if !group_present(self.0.id()) || Instant::now() >= end {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if group_present(self.0.id()) {
            unsafe { libc::kill(-(self.0.id() as i32), libc::SIGKILL) };
        }
        let _ = self.0.kill();
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        if self.1 {
            self.stop();
        }
        let _ = self.0.wait();
        if self.1 {
            let _ = group_absent(self.0.id());
        }
    }
}
fn group_present(pid: u32) -> bool {
    if unsafe { libc::kill(-(pid as i32), 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
fn group_absent(pid: u32) -> bool {
    let end = Instant::now() + Duration::from_secs(2);
    while group_present(pid) {
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    true
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
    anyhow::ensure!(!cancelled(), "capture interrupted before launch");
    let mut owned = Owned(
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0).spawn()?,
        true,
    );
    let pid = owned.0.id();
    let mut stdout = owned.0.stdout.take();
    let mut stderr = owned.0.stderr.take();
    nonblocking(stdout.as_ref().unwrap())?;
    nonblocking(stderr.as_ref().unwrap())?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let end = Instant::now() + timeout;
    let mut exited = None;
    let mut status = None;
    let problem = loop {
        if cancelled() {
            break Some("capture interrupted".into());
        }
        if let Err(error) = drain(&mut stdout, &mut out, limit.saturating_sub(err.len()))
            .and_then(|_| drain(&mut stderr, &mut err, limit.saturating_sub(out.len())))
        {
            break Some(error.to_string());
        }
        if status.is_none() {
            status = owned.0.try_wait()?;
            if status.is_some() {
                exited = Some(Instant::now());
            }
        }
        if status.is_some() && stdout.is_none() && stderr.is_none() {
            break group_present(pid).then(|| "owned descendants remained after child exit".into());
        }
        if Instant::now() >= end {
            break Some("child exceeded capture deadline".into());
        }
        if exited.is_some_and(|at: Instant| at.elapsed() >= Duration::from_secs(1)) {
            break Some("child exited with output pipes still open".into());
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if problem.is_some() {
        owned.stop();
    }
    let status = owned.0.wait()?;
    let absent = group_absent(pid);
    let cleanup_error = (!absent).then(|| format!("owned process group {pid} remains after kill/reap window"));
    owned.1 = !absent;
    Ok(Capture { status, stdout: out, stderr: err, problem, cleanup_error, pid })
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
        assert!(group_absent(pid), "owned group remains");
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
                .args(["-c", r#"printf '%s' "$$" > "$RUSTY_CAPTURE_READY"; exec /bin/sleep 60"#])
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
            let mut helper = Owned(
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
                true,
            );
            let end = Instant::now() + Duration::from_secs(60);
            while !root.0.join("ready").exists() {
                assert!(matches!(helper.0.try_wait(), Ok(None)), "signal helper exited before readiness");
                assert!(Instant::now() < end, "signal helper did not publish readiness");
                std::thread::sleep(Duration::from_millis(5));
            }
            let captured_pid = std::fs::read_to_string(root.0.join("ready")).unwrap().parse().unwrap();
            assert_eq!(unsafe { libc::kill(helper.0.id() as i32, sig) }, 0);
            let end = Instant::now() + Duration::from_secs(60);
            let status = loop {
                if let Some(status) = helper.0.try_wait().unwrap() {
                    break status;
                }
                assert!(Instant::now() < end, "signal helper did not finish cleanup");
                std::thread::sleep(Duration::from_millis(5));
            };
            helper.1 = group_present(helper.0.id());
            let mut text = String::new();
            std::fs::File::open(root.0.join("log")).unwrap().take(64 * 1024).read_to_string(&mut text).unwrap();
            assert!(status.success(), "{text}");
            assert!(!helper.1, "helper group remains after completion");
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
    fn output_limit_kills_and_reaps_the_owned_writer() {
        let result = shell("while :; do printf '0123456789'; done", 4096, Duration::from_secs(60));
        assert!(!result.success());
        assert!(result.problem.unwrap().contains("output exceeds"));
        assert!(result.stdout.len() + result.stderr.len() <= 4096);
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
