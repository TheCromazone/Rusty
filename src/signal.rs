//! Ctrl-C handling. The first press stops the current turn; a second press
//! before the turn notices quits the program.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

thread_local! { static RUN_CANCEL: std::cell::RefCell<Option<std::sync::Arc<AtomicBool>>> = const {std::cell::RefCell::new(None)}; }
static FOCUSED_CANCEL: std::sync::Mutex<Option<std::sync::Weak<AtomicBool>>> = std::sync::Mutex::new(None);
pub fn focus_cancel(token: Option<std::sync::Arc<AtomicBool>>) {
    *FOCUSED_CANCEL.lock().unwrap() = token.as_ref().map(std::sync::Arc::downgrade);
}
fn cancel_focused() -> bool {
    let token = FOCUSED_CANCEL.lock().ok().and_then(|v| v.as_ref().and_then(std::sync::Weak::upgrade));
    if let Some(token) = token {
        !token.swap(true, Ordering::SeqCst)
    } else {
        false
    }
}
pub struct CancelScope(Option<std::sync::Arc<AtomicBool>>);
pub fn cancel_scope(token: std::sync::Arc<AtomicBool>) -> CancelScope {
    CancelScope(RUN_CANCEL.with(|c| c.replace(Some(token))))
}
impl Drop for CancelScope {
    fn drop(&mut self) {
        RUN_CANCEL.with(|c| c.replace(self.0.take()));
    }
}

/// Write end of the self-pipe the SIGINT handler wakes the handler thread through.
static WAKE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-1);

extern "C" fn on_sigint(_: libc::c_int) {
    // Only write(2) here: it is async-signal-safe. A full pipe means a press
    // is already waiting to be handled.
    let fd = WAKE.load(Ordering::Relaxed);
    unsafe { libc::write(fd, [0u8].as_ptr().cast(), 1) };
}

fn on_press() {
    if cancel_focused() {
        return;
    }
    if INTERRUPTED.swap(true, Ordering::SeqCst) {
        restore_terminal();
        eprintln!("\nbye");
        std::process::exit(130);
    }
}

/// Handles Ctrl-C on a thread of its own, so the handler may lock and print.
/// It replaces whatever SIGINT did before, even when the shell ignored it
/// for a background job.
pub fn install() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let mut fds = [0; 2];
        if libc::pipe(fds.as_mut_ptr()) != 0 {
            return;
        }
        let [read, write] = fds;
        libc::fcntl(read, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(write, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(write, libc::F_SETFL, libc::fcntl(write, libc::F_GETFL) | libc::O_NONBLOCK);
        WAKE.store(write, Ordering::Relaxed);
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = on_sigint as extern "C" fn(libc::c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(libc::SIGINT, &action, std::ptr::null_mut()) != 0 {
            return;
        }
        let _ = std::thread::Builder::new().name("ctrl-c".into()).spawn(move || {
            let mut byte = 0u8;
            loop {
                match libc::read(read, (&mut byte as *mut u8).cast(), 1) {
                    1 => on_press(),
                    -1 if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
                    _ => return,
                }
            }
        });
    });
}

pub fn interrupted() -> bool {
    RUN_CANCEL
        .with(|c| c.borrow().as_ref().map(|v| v.load(Ordering::SeqCst)))
        .unwrap_or_else(|| INTERRUPTED.load(Ordering::SeqCst))
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

#[derive(Default)]
struct Draft {
    bytes: Vec<u8>,
    escape: Vec<u8>,
    paste: bool,
}
impl Draft {
    fn feed(&mut self, bytes: &[u8], queue: &mut String) {
        for &b in bytes {
            if !self.escape.is_empty() {
                self.escape.push(b);
                if self.escape == b"\x1b[200~" {
                    self.paste = true;
                    self.escape.clear();
                } else if self.escape == b"\x1b[201~" {
                    self.paste = false;
                    self.escape.clear();
                } else if self.escape.len() > 16 || (self.escape.len() > 2 && (b.is_ascii_alphabetic() || b == b'~')) {
                    self.escape.clear();
                }
                continue;
            }
            match b {
                27 => self.escape.push(27),
                b'\n' | b'\r' if self.paste => self.bytes.push(b'\n'),
                b'\n' | b'\r' => {
                    if !self.bytes.is_empty() {
                        queue.push_str(&String::from_utf8_lossy(&self.bytes));
                        queue.push('\n');
                        self.bytes.clear();
                    }
                }
                8 | 127 => while self.bytes.pop().is_some_and(|b| b & 0xc0 == 0x80) {},
                21 => self.bytes.clear(),
                b if b >= 32 => self.bytes.push(b),
                _ => {}
            }
        }
    }
}
static DRAFT: std::sync::Mutex<Draft> =
    std::sync::Mutex::new(Draft { bytes: Vec::new(), escape: Vec::new(), paste: false });
static TYPING: AtomicBool = AtomicBool::new(false);

fn typing_terminal() {
    if let Some(Some(t)) = COOKED.get() {
        let mut mode = *t;
        mode.c_lflag &= !(libc::ICANON | libc::ECHO);
        mode.c_lflag |= libc::NOFLSH;
        mode.c_cc[libc::VMIN] = 1;
        mode.c_cc[libc::VTIME] = 0;
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &mode);
        }
        use std::io::Write;
        print!("\x1b[?2004h");
        let _ = std::io::stdout().flush();
    }
}

pub struct Typing;
pub fn typing() -> Typing {
    TYPING.store(true, Ordering::SeqCst);
    typing_terminal();
    Typing
}
impl Drop for Typing {
    fn drop(&mut self) {
        for _ in 0..128 {
            if !read_keys() {
                break;
            }
        }
        TYPING.store(false, Ordering::SeqCst);
        restore_terminal();
    }
}

pub fn take_draft() -> String {
    let mut draft = DRAFT.lock().unwrap();
    draft.escape.clear();
    draft.paste = false;
    String::from_utf8_lossy(&std::mem::take(&mut draft.bytes)).into_owned()
}

pub fn input_preview() -> Option<String> {
    if !TYPING.load(Ordering::SeqCst) {
        return None;
    }
    let count = QUEUED.lock().ok()?.lines().filter(|l| !l.trim().is_empty()).count();
    let draft = DRAFT.lock().ok()?;
    let text = rusty::privacy::redact(&String::from_utf8_lossy(&draft.bytes)).0.replace('\n', " ↵ ");
    use unicode_width::UnicodeWidthChar;
    let mut cells = 0;
    let tail: String = text
        .chars()
        .rev()
        .take_while(|c| {
            cells += c.width().unwrap_or(0);
            cells <= crate::ui::width().saturating_sub(20).max(1)
        })
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let text = if tail.len() < text.len() { format!("…{tail}") } else { text };
    Some(if text.is_empty() {
        if count == 0 {
            return None;
        } else {
            format!("› {count} queued · type to add more")
        }
    } else {
        format!("› {text} · Enter queues")
    })
}

/// Called while a turn runs. Ctrl-C normally arrives as SIGINT, but if the
/// terminal ever hands it over as a raw byte this catches it too. Anything
/// else typed during a turn is queued and becomes the next message.
pub fn poll_keys() {
    read_keys();
    crate::display::refresh_input();
    crate::aside::poll();
}

fn read_keys() -> bool {
    unsafe {
        if libc::isatty(0) != 1 {
            return false;
        }
        let mut p = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        if libc::poll(&mut p, 1, 0) <= 0 || p.revents & libc::POLLIN == 0 {
            return false;
        }
        let mut buf = [0u8; 512];
        let n = libc::read(0, buf.as_mut_ptr().cast(), buf.len());
        if n <= 0 {
            return false;
        }
        let bytes = &buf[..n as usize];
        let interrupted = bytes.contains(&3);
        let filtered: Vec<u8> = bytes.iter().copied().filter(|b| *b != 3).collect();
        let bytes = filtered.as_slice();
        if let Ok(mut q) = QUEUED.lock() {
            if TYPING.load(Ordering::SeqCst) {
                if let Ok(mut draft) = DRAFT.lock() {
                    draft.feed(bytes, &mut q);
                }
            } else {
                q.push_str(&String::from_utf8_lossy(bytes));
            }
        }
        if interrupted {
            on_press();
        }
    }
    true
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

/// The entry `take_queued` would return next, left in the queue.
pub fn peek_queued() -> Option<String> {
    next_queued(&QUEUED.lock().ok()?).map(|(text, _)| text)
}

fn pop_queued(q: &mut String) -> Option<String> {
    let Some((text, end)) = next_queued(q) else {
        q.clear(); // only blank lines were left
        return None;
    };
    q.drain(..end);
    Some(text)
}

/// The next queued entry and how many bytes of the queue it takes, as if it
/// had been typed at the prompt: a `/command` line on its own, and the lines
/// up to the next command together as one message, so a pasted block stays
/// one turn instead of one turn per line.
fn next_queued(q: &str) -> Option<(String, usize)> {
    let mut lines: Vec<&str> = Vec::new();
    let mut end = 0;
    for line in q.split_inclusive('\n') {
        let text = line.trim();
        if crate::slash::command_line(text) {
            if lines.is_empty() {
                return Some((text.to_string(), end + line.len()));
            }
            break;
        }
        if !text.is_empty() || !lines.is_empty() {
            lines.push(line.trim_end());
        }
        end += line.len();
    }
    let text = lines.join("\n").trim().to_string();
    (!text.is_empty()).then_some((text, end))
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
        use std::io::Write;
        print!("\x1b[?2004l");
        let _ = std::io::stdout().flush();
    }
}

/// Reads one approval answer in the terminal's own cooked line editing.
/// A second rustyline editor here would replace the REPL's process-wide
/// resize handler, and dropping it leaves that handler writing to a closed
/// pipe, so the next terminal resize aborted rusty. Ctrl-C (SIGINT in cooked
/// mode) comes back as `ErrorKind::Interrupted`.
pub fn approval_line(prompt: &str) -> std::io::Result<String> {
    use std::io::{BufRead, Write};
    struct ResumeTyping;
    impl Drop for ResumeTyping {
        fn drop(&mut self) {
            if TYPING.load(Ordering::SeqCst) {
                typing_terminal();
            }
        }
    }
    let _resume = ResumeTyping;
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
    fn bracketed_multiline_paste_is_one_unsent_draft() {
        let mut draft = super::Draft::default();
        let mut queued = String::new();
        let text = "/var/folders/Screenshot.png please inspect\n\nsecond line café 🌲";
        for byte in format!("\x1b[200~{text}\x1b[201~").bytes() {
            draft.feed(&[byte], &mut queued);
        }
        assert!(queued.is_empty());
        assert_eq!(String::from_utf8(draft.bytes.clone()).unwrap(), text);
        draft.feed(b"\r", &mut queued);
        assert_eq!(super::pop_queued(&mut queued).as_deref(), Some(text));
        assert!(!crate::slash::command_line(text));
        assert!(crate::slash::command_line("/compact focus"));
    }

    #[test]
    fn busy_draft_needs_enter_and_edits_utf8_without_escape_leaks() {
        let mut draft = super::Draft::default();
        let mut queued = String::new();
        draft.feed("next 界".as_bytes(), &mut queued);
        assert!(queued.is_empty());
        draft.feed(b"\x7f!\x1b[D\x1b[C", &mut queued);
        assert_eq!(String::from_utf8_lossy(&draft.bytes), "next !");
        draft.feed(b"\r", &mut queued);
        assert_eq!(queued, "next !\n");
        assert!(draft.bytes.is_empty());
        draft.feed(b"discard\x15keep", &mut queued);
        assert_eq!(String::from_utf8_lossy(&draft.bytes), "keep");
    }

    #[test]
    fn queued_commands_keep_their_message_boundaries() {
        let mut q = "\n/context\n/permissions read-only\nnext question\n".to_string();
        for want in ["/context", "/permissions read-only", "next question"] {
            assert_eq!(super::pop_queued(&mut q).as_deref(), Some(want));
        }
        assert!(super::pop_queued(&mut q).is_none());
    }

    #[test]
    fn a_pasted_block_stays_one_message_between_commands() {
        let mut q = "\nsecond message\r\nwith a pasted line\n/compact\n\nthen this\n/exit\nhalf typ".to_string();
        for want in ["second message\nwith a pasted line", "/compact", "then this", "/exit", "half typ"] {
            assert_eq!(super::next_queued(&q).map(|(text, _)| text).as_deref(), Some(want));
            assert_eq!(super::pop_queued(&mut q).as_deref(), Some(want));
        }
        assert!(super::pop_queued(&mut q).is_none());
        assert!(super::next_queued("\n  \n").is_none());
    }
}
