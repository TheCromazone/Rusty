//! Ctrl-C handling. The first press stops the current turn; a second press
//! before the turn notices quits the program.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn install() {
    let _ = ctrlc::set_handler(|| {
        if INTERRUPTED.swap(true, Ordering::SeqCst) {
            eprintln!("\nbye");
            std::process::exit(130);
        }
    });
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

pub fn reset() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

/// Sleeps for `secs`, waking early if the user presses Ctrl-C.
pub fn sleep(secs: u64) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < end && !interrupted() {
        poll_keys();
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Marks the current turn as interrupted (e.g. Ctrl-C at an approval prompt).
pub fn trip() {
    INTERRUPTED.store(true, Ordering::SeqCst);
}

static QUEUED: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// Called while a turn runs. Ctrl-C normally arrives as SIGINT, but if the
/// terminal ever hands it over as a raw byte this catches it too. Anything
/// else typed during a turn is queued and becomes the next message.
pub fn poll_keys() {
    read_keys();
    crate::aside::poll();
}

fn read_keys() {
    unsafe {
        if libc::isatty(0) != 1 {
            return;
        }
        let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        if libc::poll(&mut p, 1, 0) <= 0 || p.revents & libc::POLLIN == 0 {
            return;
        }
        let mut buf = [0u8; 512];
        let n = libc::read(0, buf.as_mut_ptr().cast(), buf.len());
        if n <= 0 {
            return;
        }
        let bytes = &buf[..n as usize];
        if bytes.contains(&3) {
            if INTERRUPTED.swap(true, Ordering::SeqCst) {
                eprintln!("\nbye");
                std::process::exit(130);
            }
            return;
        }
        if let Ok(mut q) = QUEUED.lock() {
            q.push_str(&String::from_utf8_lossy(bytes));
        }
    }
}

/// A message typed while the last turn was running, if any.
pub fn take_queued() -> Option<String> {
    let mut q = QUEUED.lock().ok()?;
    pop_queued(&mut q)
}

/// Remove one runtime command while preserving all other type-ahead input.
pub fn take_command(command: &str) -> Option<String> {
    let mut q = QUEUED.lock().ok()?;
    let mut offset = 0;
    for line in q.split_inclusive('\n') {
        let text = line.trim();
        if text == command || text.strip_prefix(command).is_some_and(|rest| rest.starts_with(char::is_whitespace)) {
            let result = text.to_string();
            let end = offset + line.len();
            q.drain(offset..end);
            return Some(result);
        }
        offset += line.len();
    }
    None
}

fn pop_queued(q: &mut String) -> Option<String> {
    while !q.is_empty() {
        let n = q.find('\n').map(|i| i + 1).unwrap_or(q.len());
        let text = q.drain(..n).collect::<String>().trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}

/// The terminal mode at startup (cooked, with Ctrl-C generating SIGINT).
static COOKED: OnceLock<Option<libc::termios>> = OnceLock::new();

/// Remembers the terminal's normal mode. Call once at startup.
pub fn save_terminal() {
    COOKED.get_or_init(|| unsafe {
        let mut t: libc::termios = std::mem::zeroed();
        (libc::isatty(0) == 1 && libc::tcgetattr(0, &mut t) == 0).then_some(t)
    });
}

/// Puts the terminal back in its normal mode before a turn runs, so Ctrl-C
/// raises SIGINT even if the line editor left raw mode behind.
pub fn restore_terminal() {
    if let Some(Some(t)) = COOKED.get() {
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, t);
        }
    }
}

/// Reads one approval answer in the terminal's own cooked line editing.
/// A second rustyline editor here would replace the REPL's process-wide
/// resize handler, and dropping it leaves that handler writing to a closed
/// pipe, so the next terminal resize aborted rusty. Ctrl-C (SIGINT in cooked
/// mode) comes back as `ErrorKind::Interrupted`.
pub fn approval_line(prompt: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    restore_terminal();
    print!("{prompt}");
    std::io::stdout().flush()?;
    loop {
        if interrupted() {
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        // SAFETY: poll reads one pollfd for up to 50 ms.
        let ready = unsafe { libc::poll(&mut p, 1, 50) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if ready > 0 {
            let mut line = String::new();
            if std::io::stdin().lock().read_line(&mut line)? == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            return Ok(line);
        }
    }
}

#[cfg(test)]
mod queue_tests {
    #[test]
    fn queued_commands_keep_their_message_boundaries() {
        let mut q = "\n/context\n/permissions read-only\nnext question\n".to_string();
        for want in ["/context", "/permissions read-only", "next question"] {
            assert_eq!(super::pop_queued(&mut q).as_deref(), Some(want));
        }
        assert!(super::pop_queued(&mut q).is_none());
    }
}
