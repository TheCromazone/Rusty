//! Private native-app transport. Each process owns one existing session lease.
//! No UI toolkit, listener or provider logic is introduced here.
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SESSION: OnceLock<Arc<Shared>> = OnceLock::new();
pub fn activate() {
    ACTIVE.store(true, Ordering::Release);
}
pub fn active() -> bool {
    ACTIVE.load(Ordering::Acquire)
}

pub struct Transport {
    input: File,
    output: File,
    instance: String,
}
fn duplicate(fd: RawFd) -> Result<File> {
    // CLOEXEC prevents tools from inheriting authority to the app protocol.
    let n = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 4) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(n) })
}
impl Transport {
    pub fn open(stdio: bool) -> Result<Self> {
        let (input, output) = if stdio {
            (duplicate(0)?, duplicate(1)?)
        } else {
            let pair = (duplicate(3)?, duplicate(3)?);
            // The inherited socket itself must not reach shell/tool children.
            unsafe {
                libc::fcntl(3, libc::F_SETFD, libc::FD_CLOEXEC);
            }
            pair
        };
        {
            let null = File::open("/dev/null")?;
            if unsafe { libc::dup2(null.as_raw_fd(), 0) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        // Preserve the dedicated channel even when old terminal code prints.
        if unsafe { libc::dup2(2, 1) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let instance = identity()?;
        let inspect_only = std::env::args().take_while(|arg| arg != "--").any(|arg| arg == "--inspect-worker");
        let protocol_capabilities: &[&str] = if inspect_only {
            &["inspect", "snapshot"]
        } else {
            &["message.submit", "run.cancel", "approval.decide", "snapshot", "shutdown", "recoverable_controls"]
        };
        let mut transport = Self { input, output, instance };
        crate::protocol::write_frame(
            &mut transport.output,
            &crate::protocol::Event {
                protocol: crate::protocol::VERSION,
                instance_id: transport.instance.clone(),
                seq: 1,
                kind: "hello".into(),
                payload: json!({"protocol":crate::protocol::VERSION,
                "schema_hash":crate::protocol::schema_hash(),"worker_instance_id":transport.instance,
                "rusty_version":env!("CARGO_PKG_VERSION"),"phase":"starting","inspect_only":inspect_only,
                "protocol_capabilities":protocol_capabilities}),
            },
        )?;
        Ok(transport)
    }
    pub fn startup_failed(&mut self, code: &str, message: &str) -> Result<()> {
        crate::protocol::write_frame(
            &mut self.output,
            &crate::protocol::Event {
                protocol: crate::protocol::VERSION,
                instance_id: self.instance.clone(),
                seq: 2,
                kind: "startup.failed".into(),
                payload: json!({"code":code,"message":crate::ui::truncate(&crate::infra::redact(message).0,8192)}),
            },
        )?;
        Ok(())
    }
}

fn identity() -> Result<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 16];
    ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow!("worker identity unavailable"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn digest(value: &Value) -> String {
    ring::digest::digest(&ring::digest::SHA256, value.to_string().as_bytes())
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

const EVENT_BYTES: usize = 16 * 1024 * 1024;

struct ByteBudget {
    used: std::sync::atomic::AtomicUsize,
    limit: usize,
}
struct ByteReservation {
    budget: Arc<ByteBudget>,
    bytes: usize,
}
impl ByteReservation {
    fn acquire(budget: &Arc<ByteBudget>, bytes: usize) -> Option<Self> {
        let mut used = budget.used.load(Ordering::Acquire);
        loop {
            let total = used.checked_add(bytes).filter(|total| *total <= budget.limit)?;
            match budget.used.compare_exchange_weak(used, total, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some(Self { budget: budget.clone(), bytes }),
                Err(current) => used = current,
            }
        }
    }
    fn shrink(&mut self, bytes: usize) {
        self.budget.used.fetch_sub(self.bytes - bytes, Ordering::AcqRel);
        self.bytes = bytes;
    }
}
impl Drop for ByteReservation {
    fn drop(&mut self) {
        self.budget.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}
struct EncodedEvent {
    bytes: Vec<u8>,
    _reservation: ByteReservation,
}
// The sole producer serializer is bounded before allocation, not after it.
// One maximum-frame reservation also covers bytes being encoded; once complete,
// retained Vec capacity remains charged (wire length includes its newline).
// This bounds queued/in-progress frame backing storage, not caller-built Value
// payloads or the allocator's transient reallocation overhead.
impl EncodedEvent {
    fn encode(event: &crate::protocol::Event, budget: &Arc<ByteBudget>) -> Result<Option<Self>> {
        let Some(mut reservation) = ByteReservation::acquire(budget, crate::protocol::MAX_FRAME_BYTES) else {
            return Ok(None);
        };
        struct Frame(Vec<u8>);
        impl std::io::Write for Frame {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if bytes.len() > (crate::protocol::MAX_FRAME_BYTES - 1).saturating_sub(self.0.len()) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "worker event exceeds protocol frame limit",
                    ));
                }
                let needed = self.0.len() + bytes.len();
                if needed > self.0.capacity() {
                    // Reserve explicitly in powers of two: a single large JSON
                    // string followed by its closing quotes must not cause Vec
                    // to double a non-power-of-two capacity past the frame cap.
                    let capacity = needed.next_power_of_two().min(crate::protocol::MAX_FRAME_BYTES);
                    self.0.reserve_exact(capacity - self.0.len());
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut frame = Frame(Vec::new());
        serde_json::to_writer(&mut frame, event)?;
        frame.0.push(b'\n');
        reservation.shrink(frame.0.capacity());
        Ok(Some(Self { bytes: frame.0, _reservation: reservation }))
    }
}
#[derive(Default)]
struct LostEvents {
    count: u64,
    replies: u64,
    first_seq: u64,
    last_seq: u64,
    reply_ids: Vec<String>,
}
impl LostEvents {
    fn record(&mut self, event: &crate::protocol::Event) {
        if self.count == 0 {
            self.first_seq = event.seq;
        }
        self.count = self.count.saturating_add(1);
        self.last_seq = event.seq;
        if event.kind == "reply" {
            self.replies = self.replies.saturating_add(1);
            if self.reply_ids.len() < 16 {
                if let Some(id) = event.payload["request_id"].as_str() {
                    self.reply_ids.push(crate::ui::truncate(id, 128).to_string());
                }
            }
        }
    }
    fn payload(&self) -> Value {
        json!({"reason":"events or replies dropped; fetch authoritative snapshot and query submission receipts; do not infer a dropped reply succeeded", "lost_event_count":self.count,"lost_reply_count":self.replies,"first_lost_seq":self.first_seq,"last_lost_seq":self.last_seq,"lost_reply_ids":self.reply_ids,"lost_reply_ids_truncated":self.replies > self.reply_ids.len() as u64})
    }
}
struct Events {
    tx: Mutex<Option<mpsc::SyncSender<EncodedEvent>>>,
    writer: Mutex<Option<std::thread::JoinHandle<()>>>,
    drained: Mutex<Option<mpsc::Receiver<()>>>,
    draining: Arc<AtomicBool>,
    budget: Arc<ByteBudget>,
    instance: String,
    sequence: Arc<AtomicU64>,
    order: Arc<Mutex<()>>,
    lost: Arc<AtomicBool>,
    losses: Arc<Mutex<LostEvents>>,
    closed: Arc<AtomicBool>,
}
// Caller holds the producer order lock. A writer calls this only after
// rechecking that its queue is empty under that same lock. Future producers
// then receive larger sequences and cannot overtake this directly written frame.
fn resync_frame(
    instance: &str,
    sequence: &AtomicU64,
    lost: &AtomicBool,
    losses: &Mutex<LostEvents>,
    budget: &Arc<ByteBudget>,
) -> Result<Option<EncodedEvent>> {
    if !lost.load(Ordering::Acquire) {
        return Ok(None);
    }
    let frame = crate::protocol::Event {
        protocol: crate::protocol::VERSION,
        instance_id: instance.into(),
        seq: sequence.fetch_add(1, Ordering::AcqRel) + 1,
        kind: "resync_required".into(),
        payload: losses.lock().unwrap().payload(),
    };
    EncodedEvent::encode(&frame, budget)
}
impl Events {
    fn channel(instance: String, count: usize, limit: usize) -> (Self, mpsc::Receiver<EncodedEvent>) {
        let (tx, rx) = mpsc::sync_channel(count);
        (
            Self {
                tx: Mutex::new(Some(tx)),
                writer: Mutex::new(None),
                drained: Mutex::new(None),
                draining: Arc::new(AtomicBool::new(false)),
                budget: Arc::new(ByteBudget { used: std::sync::atomic::AtomicUsize::new(0), limit }),
                instance,
                sequence: Arc::new(AtomicU64::new(1)),
                order: Arc::new(Mutex::new(())),
                lost: Arc::new(AtomicBool::new(false)),
                losses: Arc::new(Mutex::new(LostEvents::default())),
                closed: Arc::new(AtomicBool::new(false)),
            },
            rx,
        )
    }
    fn new(output: File, instance: String) -> Self {
        let (events, rx) = Self::channel(instance.clone(), 256, EVENT_BYTES);
        let (done, drained) = mpsc::channel();
        let ended = events.closed.clone();
        let stop = events.draining.clone();
        let budget = events.budget.clone();
        let sequence = events.sequence.clone();
        let order = events.order.clone();
        let lost = events.lost.clone();
        let losses = events.losses.clone();
        let writer = std::thread::spawn(move || {
            let mut deadline = None;
            let result = (|| -> Result<()> {
                loop {
                    if stop.load(Ordering::Acquire) {
                        deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(2));
                    }
                    let event = match rx.recv_timeout(Duration::from_millis(50)) {
                        Ok(event) => Some(event),
                        Err(_) => {
                            let _order = order.lock().unwrap();
                            // A producer may have queued since the timeout.
                            match rx.try_recv() {
                                Ok(event) => Some(event),
                                Err(error) => {
                                    if let Some(marker) = resync_frame(&instance, &sequence, &lost, &losses, &budget)? {
                                        lost.store(false, Ordering::Release);
                                        *losses.lock().unwrap() = LostEvents::default();
                                        Some(marker)
                                    } else if matches!(error, mpsc::TryRecvError::Disconnected) {
                                        return Ok(());
                                    } else {
                                        None
                                    }
                                }
                            }
                        }
                    };
                    if let Some(event) = event {
                        write_event(&output, &event, &stop, &mut deadline)?;
                    }
                }
            })();
            if result.is_err() {
                ended.store(true, Ordering::Release);
                if active() {
                    crate::signal::trip();
                }
            }
            // Release all queued reservations before acknowledging termination.
            drop(rx);
            let _ = done.send(());
        });
        *events.writer.lock().unwrap() = Some(writer);
        *events.drained.lock().unwrap() = Some(drained);
        events
    }
    fn finish(&self) -> bool {
        {
            let _order = self.order.lock().unwrap();
            self.draining.store(true, Ordering::Release);
            self.tx.lock().unwrap().take();
        }
        if let Some(done) = self.drained.lock().unwrap().take() {
            if done.recv_timeout(Duration::from_secs(3)).is_err() {
                self.fail();
                return false;
            }
        }
        if let Some(writer) = self.writer.lock().unwrap().take() {
            if writer.join().is_err() {
                self.fail();
            }
        }
        !self.closed.load(Ordering::Acquire)
    }
    fn send(&self, event: EncodedEvent) -> std::result::Result<(), mpsc::TrySendError<EncodedEvent>> {
        match self.tx.lock().unwrap().as_ref() {
            Some(tx) => tx.try_send(event),
            None => Err(mpsc::TrySendError::Disconnected(event)),
        }
    }
    fn frame(&self, kind: &str, payload: Value) -> crate::protocol::Event {
        crate::protocol::Event {
            protocol: crate::protocol::VERSION,
            instance_id: self.instance.clone(),
            seq: self.sequence.fetch_add(1, Ordering::AcqRel) + 1,
            kind: kind.into(),
            payload,
        }
    }
    fn fail(&self) {
        self.closed.store(true, Ordering::Release);
        if active() {
            crate::signal::trip();
        }
    }
    fn emit(&self, kind: &str, payload: Value) {
        let _order = self.order.lock().unwrap();
        if self.closed.load(Ordering::Acquire) || self.draining.load(Ordering::Acquire) {
            return;
        }
        match resync_frame(&self.instance, &self.sequence, &self.lost, &self.losses, &self.budget) {
            Ok(Some(marker)) => {
                if self.send(marker).is_ok() {
                    self.lost.store(false, Ordering::Release);
                    *self.losses.lock().unwrap() = LostEvents::default();
                }
            }
            Ok(None) => {}
            Err(_) => {
                self.fail();
                return;
            }
        }
        let event = self.frame(kind, payload);
        match EncodedEvent::encode(&event, &self.budget) {
            Ok(Some(frame)) => match self.send(frame) {
                Ok(()) => {}
                Err(mpsc::TrySendError::Full(_)) => {
                    self.losses.lock().unwrap().record(&event);
                    self.lost.store(true, Ordering::Release);
                }
                Err(_) => self.fail(),
            },
            Ok(None) => {
                self.losses.lock().unwrap().record(&event);
                self.lost.store(true, Ordering::Release);
            }
            Err(_) => self.fail(),
        }
    }
}

// Socket sends are nonblocking per-call; pipe flags affect only the dedicated
// output descriptor. The input side of an fd-3 socket remains blocking.
fn write_event(output: &File, event: &EncodedEvent, stop: &AtomicBool, deadline: &mut Option<Instant>) -> Result<()> {
    #[cfg(debug_assertions)]
    {
        let decoded: crate::protocol::Event = serde_json::from_slice(&event.bytes[..event.bytes.len() - 1])?;
        decoded.validate_payload().map_err(|error| anyhow!("invalid worker event {}: {error}", decoded.kind))?;
    }
    let bytes = &event.bytes;
    let fd = output.as_raw_fd();
    let mut offset = 0;
    while offset < bytes.len() {
        if stop.load(Ordering::Acquire) {
            deadline.get_or_insert_with(|| Instant::now() + Duration::from_secs(2));
        }
        if deadline.is_some_and(|until| Instant::now() >= until) {
            return Err(anyhow!("worker output drain timed out"));
        }
        let remaining = &bytes[offset..];
        let mut n = unsafe { libc::send(fd, remaining.as_ptr().cast(), remaining.len(), libc::MSG_DONTWAIT) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOTSOCK) {
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
            if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            n = unsafe { libc::write(fd, remaining.as_ptr().cast(), remaining.len()) };
        }
        if n > 0 {
            offset += n as usize;
            continue;
        }
        if n == 0 {
            return Err(anyhow!("worker output closed"));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        if error.kind() != std::io::ErrorKind::WouldBlock {
            return Err(error.into());
        }
        let mut poll = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
        unsafe {
            libc::poll(&mut poll, 1, 50);
        }
    }
    Ok(())
}

fn reply(events: &Events, id: &str, result: std::result::Result<Value, String>) {
    match result {
        Ok(value) => events.emit("reply", json!({"request_id":id,"ok":true,"result":value})),
        Err(error) => events.emit("reply", json!({"request_id":id,"ok":false,"error":error})),
    }
}

pub fn inspect(transport: Transport, root: &Path) -> Result<i32> {
    let instance = transport.instance.clone();
    // Inspection is synchronous and backpressured: it never starts a model or tool.
    let mut output = transport.output;
    let mut seq = 1;
    let mut emit = |kind: &str, payload: Value| -> Result<()> {
        seq += 1;
        crate::protocol::write_frame(
            &mut output,
            &crate::protocol::Event {
                protocol: crate::protocol::VERSION,
                instance_id: instance.clone(),
                seq,
                kind: kind.into(),
                payload,
            },
        )?;
        Ok(())
    };
    let mut input = BufReader::new(transport.input);
    while let Some(request) = crate::protocol::read_request(&mut input)? {
        let result = match request.method.as_str() {
            "inspect" => {
                let op =
                    request.params["operation"].as_str().ok_or_else(|| anyhow!("inspection operation required"))?;
                crate::inspect::handle(root, op, &request.params["arguments"])
            }
            "snapshot" => Ok(json!({"read_only":true,"root":root})),
            _ => Err("inspect worker accepts only inspect and snapshot".into()),
        };
        match result {
            Ok(value) => emit("reply", json!({"request_id":request.request_id,"ok":true,"result":value}))?,
            Err(error) => emit("reply", json!({"request_id":request.request_id,"ok":false,"error":error}))?,
        }
    }
    Ok(0)
}

#[derive(Clone, Serialize, Deserialize)]
struct Receipt {
    #[serde(default)]
    ordinal: u64,
    fingerprint: String,
    state: String,
    run_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ToolFlight>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ToolFlight {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    activity_id: Option<String>,
    tool: String,
    summary: String,
    requester: String,
    started_at: u64,
}
struct Pending {
    id: String,
    digest: String,
    run_id: String,
    expires: Instant,
    typed: bool,
    operation: Value,
    answer: Option<bool>,
}
struct State {
    run_id: Option<String>,
    phase: String,
    pending: Option<Pending>,
    history: Value,
    live_text: String,
    live_reasoning: String,
    last_tool: Value,
    pending_text: String,
    pending_reasoning: String,
    last_delta: Instant,
}
struct Shared {
    receipts: Arc<Mutex<BTreeMap<String, Receipt>>>,
    receipt_path: std::path::PathBuf,
    session_id: String,
    events: Events,
    state: Mutex<State>,
    wake: Condvar,
    disconnected: AtomicBool,
}
impl Shared {
    fn snapshot(&self) -> Value {
        let mut state = self.state.lock().unwrap();
        // A snapshot already contains these bytes. Publish their delta sequence
        // before the watermark so applying later events cannot duplicate them.
        flush_deltas(self, &mut state, true);
        let approval=state.pending.as_ref().map(|p| json!({"approval_id":p.id,"operation_digest":p.digest,"run_id":p.run_id,"expires_in_secs":p.expires.saturating_duration_since(Instant::now()).as_secs(),"typed_required":p.typed,"typed_phrase":if p.typed {Some("yes")}else{None},"operation":p.operation}));
        json!({"worker_instance_id":self.events.instance,"snapshot_watermark":self.events.sequence.load(Ordering::Acquire),"live_text":state.live_text,"live_reasoning":state.live_reasoning,"live_truncated":state.live_text.len()>=128*1024||state.live_reasoning.len()>=128*1024,"last_tool":state.last_tool,"session_id":self.session_id,"run_id":state.run_id,"phase":state.phase,"pending_approval":approval,"history":state.history,"cost_usd":null})
    }
}

/// All Display paths use the same bounded event sink; CLI takes no new path.
pub fn display(kind: &str, payload: Value) {
    if let Some(shared) = SESSION.get() {
        let mut state = shared.state.lock().unwrap();
        match kind {
            "message.delta" => {
                fault("mid_stream");
                append_bounded(&mut state.live_text, payload["text"].as_str().unwrap_or(""));
                append_bounded(&mut state.pending_text, payload["text"].as_str().unwrap_or(""));
                flush_deltas(shared, &mut state, false);
                return;
            }
            "reasoning.delta" => {
                append_bounded(&mut state.live_reasoning, payload["text"].as_str().unwrap_or(""));
                append_bounded(&mut state.pending_reasoning, payload["text"].as_str().unwrap_or(""));
                flush_deltas(shared, &mut state, false);
                return;
            }
            "tool.proposed" | "tool.started" | "tool.completed" => {
                state.last_tool = json!({"name":payload["name"],"ok":payload["ok"],"summary":crate::ui::truncate(&payload.to_string(),4096)})
            }
            _ => {}
        }
        flush_deltas(shared, &mut state, true);
        if payload.to_string().len() > 256 * 1024 {
            shared.events.emit("event.truncated", json!({"original_kind":kind,"name":payload["name"],"truncated":true,"summary":"large display event; inspect authoritative transcript or file"}));
        } else {
            shared.events.emit(kind, payload);
        }
    }
}
fn flush_deltas(shared: &Shared, state: &mut State, force: bool) {
    if !force && state.last_delta.elapsed() < Duration::from_millis(34) {
        return;
    }
    for (kind, text) in [
        ("message.delta", std::mem::take(&mut state.pending_text)),
        ("reasoning.delta", std::mem::take(&mut state.pending_reasoning)),
    ] {
        if !text.is_empty() {
            shared.events.emit(kind, json!({"text":text}));
        }
    }
    state.last_delta = Instant::now();
}
pub fn flush_display() {
    if let Some(shared) = SESSION.get() {
        let mut state = shared.state.lock().unwrap();
        flush_deltas(shared, &mut state, false);
    }
}

fn approval_ttl() -> Duration {
    #[cfg(debug_assertions)]
    if let Ok(value) = std::env::var("RUSTY_WORKER_TEST_APPROVAL_TTL_MS") {
        if let Ok(value) = value.parse::<u64>() {
            return Duration::from_millis(value.clamp(1, 300000));
        }
    }
    Duration::from_secs(300)
}

pub fn approve(
    name: &str,
    args: &Value,
    cwd: &Path,
    why: &str,
    destructive: bool,
    policy: &crate::permissions::Policy,
    placement_requester: (&str, &str),
) -> std::result::Result<bool, &'static str> {
    let Some(shared) = SESSION.get() else {
        return Err("approval unavailable");
    };
    approve_on(shared, name, args, cwd, why, (destructive, policy, placement_requester))
}

fn approve_on(
    shared: &Shared,
    name: &str,
    args: &Value,
    cwd: &Path,
    why: &str,
    context: (bool, &crate::permissions::Policy, (&str, &str)),
) -> std::result::Result<bool, &'static str> {
    let (destructive, policy, (placement, requester)) = context;
    let Ok(id) = identity() else {
        return Err("approval unavailable");
    };
    let mut state = shared.state.lock().unwrap();
    while state.pending.is_some() {
        if crate::signal::interrupted()
            || shared.disconnected.load(Ordering::Acquire)
            || shared.events.closed.load(Ordering::Acquire)
        {
            return Err("approval interrupted before presentation");
        }
        state = shared.wake.wait_timeout(state, Duration::from_millis(50)).unwrap().0;
    }
    // The preceding card can resolve while this waiter is waking for a
    // disconnect/cancel. Check again before granting a new presentation.
    if crate::signal::interrupted()
        || shared.disconnected.load(Ordering::Acquire)
        || shared.events.closed.load(Ordering::Acquire)
    {
        return Err("approval interrupted before presentation");
    }
    let Some(run_id) = state.run_id.clone() else {
        return Err("approval unavailable");
    };
    let operation = json!({"name":name,"arguments":args,"cwd":cwd,"run_id":run_id,"reason":why,"destructive":destructive,"requester":requester,"placement":placement,"policy_revision":digest(&serde_json::to_value(policy).unwrap_or(Value::Null)),"lease_generation":shared.events.instance,"session_id":shared.session_id});
    if operation.to_string().len() > 128 * 1024 {
        return Err("approval unavailable");
    }
    let operation_digest = digest(&operation);
    state.pending = Some(Pending {
        id: id.clone(),
        digest: operation_digest.clone(),
        run_id: run_id.clone(),
        expires: Instant::now() + approval_ttl(),
        typed: destructive,
        operation: operation.clone(),
        answer: None,
    });
    shared.events.emit("approval.requested",json!({"approval_id":id,"operation_digest":operation_digest,"operation":operation,"expires_at":rusty::unix_now()+approval_ttl().as_secs().max(1),"typed_phrase":if destructive {Some("yes")}else{None},"sensitive":true}));
    loop {
        let pending = state.pending.as_ref().unwrap();
        if Instant::now() >= pending.expires
            || crate::signal::interrupted()
            || shared.disconnected.load(Ordering::Acquire)
            || shared.events.closed.load(Ordering::Acquire)
        {
            let reason = if Instant::now() >= pending.expires { "approval expired" } else { "approval interrupted" };
            state.pending = None;
            shared.events.emit("approval.expired", json!({"approval_id":id,"reason":reason}));
            shared.wake.notify_all();
            return Err(reason);
        }
        if let Some(answer) = state.pending.as_ref().unwrap().answer {
            state.pending = None;
            shared.events.emit("approval.resolved", json!({"approval_id":id,"allowed":answer}));
            shared.wake.notify_all();
            return Ok(answer);
        }
        state = shared.wake.wait_timeout(state, Duration::from_millis(50)).unwrap().0;
    }
}
const RECEIPT_BYTES: usize = 8 * 1024 * 1024;

fn receipt_value(receipts: &BTreeMap<String, Receipt>, reserve_states: bool) -> Result<Value> {
    let mut value = serde_json::to_value(receipts)?;
    if reserve_states {
        // Admission must leave enough space for every outstanding receipt to
        // reach its longest terminal state, including JSON escaping in IDs.
        for receipt in value.as_object_mut().unwrap().values_mut() {
            receipt["state"] = json!("not_started");
        }
    }
    if serde_json::to_vec(&value)?.len() > RECEIPT_BYTES - if reserve_states { 64 * 1024 } else { 0 } {
        return Err(ReceiptOverLimit.into());
    }
    Ok(value)
}

fn save_receipts(path: &Path, receipts: &BTreeMap<String, Receipt>) -> Result<()> {
    crate::session::save(path, receipt_value(receipts, false)?)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[derive(Debug)]
struct ReceiptOverLimit;
impl std::fmt::Display for ReceiptOverLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("worker receipt file exceeds 8 MiB decoded limit")
    }
}
impl std::error::Error for ReceiptOverLimit {}

fn read_receipt_bytes(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(RECEIPT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > RECEIPT_BYTES {
        return Err(ReceiptOverLimit.into());
    }
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(bytes.as_slice()).take(RECEIPT_BYTES as u64 + 1).read_to_end(&mut decoded)?;
        if decoded.len() > RECEIPT_BYTES {
            return Err(ReceiptOverLimit.into());
        }
        Ok(decoded)
    } else {
        Ok(bytes)
    }
}

fn load_receipts(path: &Path) -> Result<BTreeMap<String, Receipt>> {
    let mut receipts: BTreeMap<String, Receipt> = match read_receipt_bytes(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            BTreeMap::new()
        }
        Err(error) => return Err(error),
    };
    if receipts.len() > 10000
        || receipts.iter().any(|(id, r)| {
            id.is_empty()
                || id.len() > 256
                || r.fingerprint.len() != 64
                || r.run_id.len() != 32
                || r.tools.len() > 32
                || r.tools.iter().any(|tool| {
                    tool.id.len() != 32
                        || tool.tool.len() > 128
                        || tool.summary.len() > 512
                        || tool.requester.len() > 256
                        || tool.activity_id.as_ref().is_some_and(|id| id.len() > 128)
                })
                || !matches!(
                    r.state.as_str(),
                    "accepted" | "running" | "uncertain" | "not_started" | "interrupted" | "failed" | "completed"
                )
        })
    {
        return Err(anyhow!("invalid worker receipt ledger"));
    }
    for receipt in receipts.values_mut() {
        if matches!(receipt.state.as_str(), "accepted" | "running") {
            receipt.state = "uncertain".into();
        }
    }
    save_receipts(path, &receipts)?;
    Ok(receipts)
}

fn serve_read_only(mut transport: Transport, agent: &crate::agent::Agent, reason: &str) -> Result<i32> {
    let mut seq = 1;
    let instance = transport.instance.clone();
    let session_id = crate::session::id_of(agent.session_path.as_ref().unwrap());
    let snapshot = json!({"worker_instance_id":instance,"snapshot_watermark":3,"session_id":session_id,
        "run_id":null,"phase":"read_only","live_text":"","live_reasoning":"","live_truncated":false,
        "last_tool":null,"pending_approval":null,"history":history_snapshot(&agent.history),"cost_usd":null,
        "receipts":{},"receipts_truncated":true,"uncertain_run_count":0,"uncertain_runs":[],"uncertain_runs_truncated":true,
        "uncertain_tools":[],"uncertain_tool_count":0,"uncertain_tools_truncated":true,
        "read_only":true,"read_only_reason":reason,"recovery_unknown":true,"in_flight_tools":[],"in_flight_tools_truncated":true});
    let mut emit = |kind: &str, payload: Value| -> Result<()> {
        seq += 1;
        crate::protocol::write_frame(
            &mut transport.output,
            &crate::protocol::Event {
                protocol: crate::protocol::VERSION,
                instance_id: instance.clone(),
                seq,
                kind: kind.into(),
                payload,
            },
        )?;
        Ok(())
    };
    emit("session.ready", json!({"session_id":session_id,"read_only":true,"reason":reason}))?;
    emit("session.snapshot", snapshot.clone())?;
    let mut input = BufReader::new(transport.input);
    while let Some(request) = crate::protocol::read_request(&mut input)? {
        let payload = match request.method.as_str() {
            "snapshot" => json!({"request_id":request.request_id,"ok":true,"result":snapshot}),
            "shutdown" => {
                emit("reply", json!({"request_id":request.request_id,"ok":true,"result":{"stopping":true}}))?;
                break;
            }
            _ => {
                json!({"request_id":request.request_id,"ok":false,"error":"session is read-only; recovery ledger requires repair"})
            }
        };
        emit("reply", payload)?;
    }
    Ok(0)
}

pub fn serve(mut transport: Transport, agent: &mut crate::agent::Agent) -> Result<i32> {
    let instance = transport.instance.clone();
    let path =
        agent.session_path.as_ref().ok_or_else(|| anyhow!("worker needs a session"))?.with_extension("worker.json");
    if std::fs::metadata(&path).is_ok_and(|metadata| metadata.len() > RECEIPT_BYTES as u64) {
        return serve_read_only(transport, agent, "receipt_ledger_over_limit");
    }
    let receipts = match load_receipts(&path) {
        Ok(receipts) => receipts,
        Err(error) if error.is::<ReceiptOverLimit>() => {
            return serve_read_only(transport, agent, "receipt_ledger_over_limit");
        }
        Err(error) => {
            transport.startup_failed("receipt_invalid", &error.to_string())?;
            return Err(error);
        }
    };
    let latest = receipts.values().max_by_key(|r| r.ordinal);
    let uncertain = receipts.values().filter(|r| r.state == "uncertain").max_by_key(|r| r.ordinal);
    let recovered_run = uncertain.or(latest).map(|r| r.run_id.clone());
    let recovered_phase = if receipts.values().any(|r| r.state == "uncertain") {
        "uncertain"
    } else {
        latest.map_or("idle", |r| r.state.as_str())
    }
    .to_string();
    if receipts.values().any(|r| r.state == "uncertain") {
        const RECOVERY: &str = "[from rusty, not the user] [native worker recovery] The previous run stopped with uncertain external effects. Do not assume it completed and do not replay its tools. Inspect actual state and ask before retrying irreversible work.";
        if !agent.history.iter().any(|m| m["content"].as_str() == Some(RECOVERY)) {
            agent.history.push(json!({"role":"user","content":RECOVERY}));
            agent.save_session()?;
        }
    }
    let events = Events::new(transport.output, instance.clone());
    let receipts = Arc::new(Mutex::new(receipts));
    let shared = Arc::new(Shared {
        receipts: receipts.clone(),
        receipt_path: path.clone(),
        session_id: crate::session::id_of(agent.session_path.as_ref().unwrap()).to_string(),
        events,
        state: Mutex::new(State {
            run_id: recovered_run,
            phase: recovered_phase,
            pending: None,
            history: history_snapshot(&agent.history),
            live_text: String::new(),
            live_reasoning: String::new(),
            last_tool: Value::Null,
            pending_text: String::new(),
            pending_reasoning: String::new(),
            last_delta: Instant::now() - Duration::from_secs(1),
        }),
        wake: Condvar::new(),
        disconnected: AtomicBool::new(false),
    });
    SESSION.set(shared.clone()).map_err(|_| anyhow!("worker already started"))?;
    struct Drain<'a>(&'a Events);
    impl Drop for Drain<'_> {
        fn drop(&mut self) {
            self.0.finish();
        }
    }
    let _drain = Drain(&shared.events);
    shared.events.emit("session.ready", json!({"session_id":shared.session_id,"read_only":false,"lease":"owned"}));
    shared.events.emit("session.snapshot", receipt_snapshot(&shared, &receipts.lock().unwrap()));
    let reader_shared = shared.clone();
    let reader_receipts = receipts.clone();
    let receipt_path = path.clone();
    let (tx, rx) = mpsc::sync_channel::<(String, String, String)>(16);
    // Framing/version failure is fatal, but schema-invalid controls preserve
    // the session. A fatal input error still drains final durable state.
    let protocol_failed = Arc::new(AtomicBool::new(false));
    let reader_protocol_failed = protocol_failed.clone();
    std::thread::spawn(move || {
        let mut input = BufReader::new(transport.input);
        let mut invalid_controls = 0;
        loop {
            let request = match crate::protocol::read_control(&mut input) {
                Ok(Some(crate::protocol::ControlInput::Request(request))) => {
                    invalid_controls = 0;
                    request
                }
                Ok(Some(crate::protocol::ControlInput::Rejected { request_id, code })) => {
                    reader_shared.events.emit(
                        "reply",
                        json!({"request_id":request_id,"ok":false,"error":"invalid control","code":code}),
                    );
                    invalid_controls += 1;
                    if invalid_controls < crate::protocol::MAX_INVALID_CONTROLS {
                        continue;
                    }
                    reader_protocol_failed.store(true, Ordering::Release);
                    reader_shared
                        .events
                        .emit("protocol.error", json!({"error":"too many consecutive invalid controls"}));
                    break;
                }
                Ok(None) => break,
                Err(e) => {
                    reader_protocol_failed.store(true, Ordering::Release);
                    reader_shared.events.emit("protocol.error", json!({"error":e.to_string()}));
                    break;
                }
            };
            let id = &request.request_id;
            if request.method == "shutdown" {
                reply(&reader_shared.events, id, Ok(json!({"stopping":true})));
                break;
            }
            let result = handle_control(&reader_shared, &reader_receipts, &receipt_path, &tx, &request);
            reply(&reader_shared.events, id, result);
        }
        let _state = reader_shared.state.lock().unwrap();
        reader_shared.disconnected.store(true, Ordering::Release);
        crate::signal::trip();
        reader_shared.wake.notify_all();
    });
    loop {
        let (request_id, text, run_id) = match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(value) => value,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if shared.disconnected.load(Ordering::Acquire) || shared.events.closed.load(Ordering::Acquire) {
                    break;
                }
                continue;
            }
            Err(_) => break,
        };
        if shared.disconnected.load(Ordering::Acquire) {
            break;
        }
        {
            let mut s = shared.state.lock().unwrap();
            if shared.disconnected.load(Ordering::Acquire) || shared.events.closed.load(Ordering::Acquire) {
                break;
            }
            crate::signal::reset();
            // EOF never gets cleared by reset: disconnect uses the same state lock.
            if shared.disconnected.load(Ordering::Acquire) || shared.events.closed.load(Ordering::Acquire) {
                break;
            }
            s.run_id = Some(run_id.clone());
            s.phase = "running".into();
            s.live_text.clear();
            s.live_reasoning.clear();
            s.last_tool = Value::Null;
            s.pending_text.clear();
            s.pending_reasoning.clear();
            s.last_delta = Instant::now() - Duration::from_secs(1);
            shared.events.emit("run.started", json!({"run_id":run_id,"request_id":request_id}));
        }
        {
            let mut ledger = receipts.lock().unwrap();
            ledger.get_mut(&request_id).unwrap().state = "running".into();
            save_receipts(&path, &ledger)?;
        }
        if agent.advisor.is_none() && agent.memory_level.advises() {
            let started = Instant::now();
            agent.advisor = rusty::advisor::Hooks::connect(
                &crate::config::config_dir().unwrap_or_default(),
                &std::path::PathBuf::from(agent.backend.identity(&agent.cwd)),
                agent.memory_level,
            )
            .ok();
            if agent.advisor.is_none() {
                agent.memory_level = rusty::advisor::Level::Off;
            }
            if std::env::var_os("RUSTY_WORKER_TRACE").is_some() {
                eprintln!("worker startup lazy_advisor_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
            }
        }
        let result = agent.run_turn(&text);
        {
            shared.state.lock().unwrap().phase = "finishing".into();
        }
        fault("before_final_receipt");
        let saved = agent.save_session();
        let retained_tools = receipts.lock().unwrap().get(&request_id).is_some_and(|r| !r.tools.is_empty());
        let phase = if saved.is_err() || retained_tools {
            "uncertain"
        } else if crate::signal::interrupted() {
            "interrupted"
        } else if result.is_err() {
            "failed"
        } else {
            "completed"
        };
        {
            let mut s = shared.state.lock().unwrap();
            s.phase = phase.into();
            s.history = history_snapshot(&agent.history);
            s.pending = None;
        }
        {
            let mut ledger = receipts.lock().unwrap();
            ledger.get_mut(&request_id).unwrap().state = phase.into();
            save_receipts(&path, &ledger)?;
        }
        let error = result.err().or_else(|| saved.err()).map(|e| crate::infra::redact(&format!("{e:#}")).0);
        shared.events.emit("run.completed",json!({"run_id":run_id,"request_id":request_id,"state":phase,"error":error,"budget":agent.budget_snapshot(),"cost_usd":null}));
        shared.events.emit("session.snapshot", receipt_snapshot(&shared, &receipts.lock().unwrap()));
    }
    {
        let mut ledger = receipts.lock().unwrap();
        for receipt in ledger.values_mut() {
            if receipt.state == "accepted" {
                receipt.state = "not_started".into();
            }
        }
        save_receipts(&path, &ledger)?;
    }
    shared.events.emit("session.snapshot", receipt_snapshot(&shared, &receipts.lock().unwrap()));
    let failed_input = protocol_failed.load(Ordering::Acquire);
    let reason = if shared.events.closed.load(Ordering::Acquire) {
        "transport_failed"
    } else if failed_input {
        "protocol_error"
    } else {
        "shutdown"
    };
    shared.events.emit("worker.closing", json!({"reason":reason}));
    let drained = shared.events.finish();
    Ok(if drained && !failed_input { 0 } else { 74 })
}

/// Fault injection is absent from release builds and never invokes a provider.
fn fault(point: &str) {
    #[cfg(debug_assertions)]
    if active() && std::env::var("RUSTY_WORKER_ABORT_AT").ok().as_deref() == Some(point) {
        unsafe {
            libc::_exit(86);
        }
    }
    let _ = point;
}

thread_local! { static TOOL: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) }; }
pub struct ToolScope {
    id: String,
    previous: Option<String>,
}
impl ToolScope {
    pub fn enter() -> Self {
        if !active() {
            return Self { id: String::new(), previous: None };
        }
        let id = identity().unwrap_or_default();
        let previous = TOOL.with(|slot| slot.replace(Some(id.clone())));
        Self { id, previous }
    }
}
impl Drop for ToolScope {
    fn drop(&mut self) {
        if self.id.is_empty() {
            return;
        }
        TOOL.with(|slot| slot.replace(self.previous.take()));
        if std::thread::panicking() {
            return;
        }
        if let Some(shared) = SESSION.get() {
            let mut ledger = shared.receipts.lock().unwrap();
            let mut changed = false;
            for receipt in ledger.values_mut() {
                let len = receipt.tools.len();
                receipt.tools.retain(|t| t.id != self.id || t.activity_id.is_some());
                changed |= len != receipt.tools.len();
            }
            if changed && save_receipts(&shared.receipt_path, &ledger).is_err() {
                crate::signal::trip();
            }
        }
    }
}

/// Transfer the dispatch scope's durable marker to its owned asynchronous
/// shell before starting the process. Only that activity's actual termination
/// clears the marker; returning a handle is not tool completion.
pub fn bind_activity(activity_id: &str) -> Result<()> {
    let Some(shared) = SESSION.get() else {
        return Ok(());
    };
    let tool_id = TOOL.with(|slot| slot.borrow().clone()).ok_or_else(|| anyhow!("activity needs tool scope"))?;
    let mut ledger = shared.receipts.lock().unwrap();
    let flight = ledger
        .values_mut()
        .flat_map(|r| r.tools.iter_mut())
        .find(|tool| tool.id == tool_id)
        .ok_or_else(|| anyhow!("activity needs durable authorization"))?;
    flight.activity_id = Some(activity_id.into());
    flight.tool = "bash_start".into();
    save_receipts(&shared.receipt_path, &ledger)
}

pub fn activity_finished(activity_id: &str) {
    let Some(shared) = SESSION.get() else {
        return;
    };
    let mut ledger = shared.receipts.lock().unwrap();
    let mut changed = false;
    for receipt in ledger.values_mut() {
        let before = receipt.tools.len();
        receipt.tools.retain(|tool| tool.activity_id.as_deref() != Some(activity_id));
        changed |= before != receipt.tools.len();
    }
    if changed && save_receipts(&shared.receipt_path, &ledger).is_err() {
        crate::signal::trip();
    }
}

pub fn tool_started(name: &str, args: &Value, requester: &str) -> Result<()> {
    let Some(shared) = SESSION.get() else {
        return Ok(());
    };
    let id = TOOL.with(|slot| slot.borrow().clone()).ok_or_else(|| anyhow!("tool execution needs a durable scope"))?;
    let run = shared.state.lock().unwrap().run_id.clone().ok_or_else(|| anyhow!("tool needs active run"))?;
    if name.len() > 128 {
        return Err(anyhow!("tool name exceeds worker limit"));
    }
    let requester = crate::ui::truncate(requester, 256);
    let summary = crate::infra::redact(&crate::tools::summary(name, args)).0;
    let mut ledger = shared.receipts.lock().unwrap();
    let receipt = ledger.values_mut().find(|r| r.run_id == run).ok_or_else(|| anyhow!("tool run receipt missing"))?;
    if receipt.tools.len() >= 32 {
        return Err(anyhow!("too many concurrent tools"));
    }
    receipt.tools.push(ToolFlight {
        id: id.clone(),
        activity_id: None,
        tool: name.into(),
        summary: crate::ui::truncate(&summary, 512).into(),
        requester: requester.into(),
        started_at: rusty::unix_now(),
    });
    if let Err(error) = save_receipts(&shared.receipt_path, &ledger) {
        ledger.values_mut().find(|r| r.run_id == run).unwrap().tools.retain(|t| t.id != id);
        return Err(error);
    }
    drop(ledger);
    shared.state.lock().unwrap().last_tool = json!({"name":name,"phase":"started","tool_id":id,"requester":requester,"summary":crate::ui::truncate(&summary,512)});
    let mut args = args.clone();
    rusty::privacy::scrub(&mut args);
    shared.events.emit("tool.started", json!({"name":name,"arguments":args,"requester":requester,"tool_id":id}));
    fault("mid_tool");
    Ok(())
}

fn append_bounded(target: &mut String, text: &str) {
    const CAP: usize = 128 * 1024;
    if target.len() >= CAP {
        return;
    }
    let mut end = (CAP - target.len()).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&text[..end]);
}

fn history_snapshot(history: &[Value]) -> Value {
    let mut bytes = 0;
    let mut page = Vec::new();
    for message in history.iter().rev().take(200) {
        let size = message.to_string().len();
        if bytes + size > 512 * 1024 {
            break;
        }
        bytes += size;
        page.push(message.clone());
    }
    page.reverse();
    json!({"messages":page,"truncated":page.len()!=history.len()})
}

fn receipt_snapshot(shared: &Shared, ledger: &BTreeMap<String, Receipt>) -> Value {
    let mut snapshot = shared.snapshot();
    let mut recent: Vec<_> = ledger.iter().collect();
    recent.sort_unstable_by_key(|(_, r)| std::cmp::Reverse(r.ordinal));
    snapshot["receipts"] = json!(recent.iter().take(100).copied().collect::<BTreeMap<_, _>>());
    snapshot["receipts_truncated"] = json!(ledger.len() > 100);
    let uncertain: Vec<_> = recent.iter().filter(|(_, r)| r.state == "uncertain").collect();
    snapshot["uncertain_run_count"] = json!(uncertain.len());
    snapshot["uncertain_runs"] = json!(uncertain
        .iter()
        .take(100)
        .map(|(id, r)| { json!({"request_id":id,"run_id":r.run_id}) })
        .collect::<Vec<_>>());
    snapshot["uncertain_runs_truncated"] = json!(uncertain.len() > 100);
    let tools: Vec<_> = uncertain.iter().flat_map(|(_, r)| r.tools.iter().map(|tool| {
        json!({"run_id":r.run_id,"tool":tool.tool,"summary":tool.summary,"requester":tool.requester,"started_at":tool.started_at,"activity_id":tool.activity_id})
    })).collect();
    snapshot["uncertain_tool_count"] = json!(tools.len());
    snapshot["uncertain_tools"] = json!(tools.iter().take(100).collect::<Vec<_>>());
    snapshot["uncertain_tools_truncated"] = json!(tools.len() > 100);
    let running:Vec<_>=recent.iter().flat_map(|(_,r)|r.tools.iter().map(|tool|json!({"run_id":r.run_id,"tool_id":tool.id,"tool":tool.tool,"summary":tool.summary,"requester":tool.requester,"started_at":tool.started_at,"activity_id":tool.activity_id}))).collect();
    snapshot["in_flight_tools"] = json!(running.iter().take(100).collect::<Vec<_>>());
    snapshot["in_flight_tools_truncated"] = json!(running.len() > 100);

    snapshot
}

fn handle_control(
    shared: &Shared,
    receipts: &Mutex<BTreeMap<String, Receipt>>,
    path: &Path,
    tx: &mpsc::SyncSender<(String, String, String)>,
    request: &crate::protocol::Request,
) -> std::result::Result<Value, String> {
    if shared.disconnected.load(Ordering::Acquire) || shared.events.closed.load(Ordering::Acquire) {
        return Err("worker is disconnecting".into());
    }
    // Validate through the same typed schema used by Swift fixtures.
    request.command().map_err(|e| e.to_string())?;
    let p = &request.params;
    match request.method.as_str() {
        "message.submit" => {
            let text = p["text"].as_str().ok_or("text required")?;
            if text.trim().is_empty() || text.len() > 128 * 1024 {
                return Err("text must be nonempty and at most 128 KiB".into());
            }
            if text.trim_start().starts_with('/') {
                return Err("slash controls need typed commands; message.submit accepts prompts only".into());
            }
            fault("before_acceptance");
            let fingerprint = digest(&json!({"text":text}));
            let mut ledger = receipts.lock().unwrap();
            if let Some(old) = ledger.get(&request.request_id) {
                if old.fingerprint != fingerprint {
                    return Err("request id reused with changed payload".into());
                }
                return Ok(json!({"receipt":old,"duplicate":true}));
            }
            if ledger.len() >= 10000 {
                return Err("session request receipt limit reached".into());
            }
            let run_id = identity().map_err(|e| e.to_string())?;
            let ordinal = ledger.len() as u64 + 1;
            ledger.insert(
                request.request_id.clone(),
                Receipt { ordinal, fingerprint, state: "accepted".into(), run_id: run_id.clone(), tools: Vec::new() },
            );
            if let Err(error) = receipt_value(&ledger, true).and_then(|_| save_receipts(path, &ledger)) {
                ledger.remove(&request.request_id);
                return Err(error.to_string());
            }
            fault("after_receipt");
            if tx.try_send((request.request_id.clone(), text.into(), run_id.clone())).is_err() {
                ledger.get_mut(&request.request_id).unwrap().state = "not_started".into();
                save_receipts(path, &ledger).map_err(|e| e.to_string())?;
                return Err("worker message queue full".into());
            }
            fault("before_reply");
            Ok(json!({"receipt":ledger[&request.request_id],"duplicate":false}))
        }
        "run.cancel" => {
            let state = shared.state.lock().unwrap();
            if state.phase == "finishing" && state.run_id.as_deref() == p["run_id"].as_str() {
                return Ok(json!({"cancel_requested":false,"reason":"run already finishing"}));
            }
            if state.phase != "running" || state.run_id.as_deref() != p["run_id"].as_str() {
                return Err("stale run id".into());
            }
            crate::signal::trip();
            shared.wake.notify_all();
            Ok(json!({"cancel_requested":true}))
        }
        "approval.decide" => {
            let mut state = shared.state.lock().unwrap();
            let pending = state.pending.as_mut().ok_or("approval no longer pending")?;
            if pending.id != p["approval_id"].as_str().unwrap_or("")
                || pending.digest != p["operation_digest"].as_str().unwrap_or("")
                || pending.expires <= Instant::now()
                || pending.answer.is_some()
            {
                return Err("stale approval, digest mismatch or expired decision".into());
            }
            let allowed = p["allowed"].as_bool().ok_or("allowed boolean required")?;
            if allowed && pending.typed && p["typed"].as_str() != Some("yes") {
                return Err("type the exact phrase yes".into());
            }
            pending.answer = Some(allowed);
            shared.wake.notify_all();
            Ok(json!({"accepted":true}))
        }
        "snapshot" => {
            let ledger = receipts.lock().unwrap();
            Ok(receipt_snapshot(shared, &ledger))
        }
        "inspect" => Err("use a separate --inspect-worker channel so inspection cannot block cancellation".into()),
        _ => Err("unsupported worker command".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct TestEvents(mpsc::Receiver<EncodedEvent>);
    impl TestEvents {
        fn decode(frame: EncodedEvent) -> crate::protocol::Event {
            serde_json::from_slice(&frame.bytes).unwrap()
        }
        fn recv(&self) -> std::result::Result<crate::protocol::Event, mpsc::RecvError> {
            self.0.recv().map(Self::decode)
        }
        fn try_recv(&self) -> std::result::Result<crate::protocol::Event, mpsc::TryRecvError> {
            self.0.try_recv().map(Self::decode)
        }
        fn recv_timeout(
            &self,
            timeout: Duration,
        ) -> std::result::Result<crate::protocol::Event, mpsc::RecvTimeoutError> {
            self.0.recv_timeout(timeout).map(Self::decode)
        }
    }
    fn shared(capacity: usize) -> (Shared, TestEvents) {
        let (events, rx) = Events::channel("epoch-test".into(), capacity, EVENT_BYTES);
        events.sequence.store(42, Ordering::Release);
        let rx = TestEvents(rx);
        (
            Shared {
                receipts: Arc::new(Mutex::new(BTreeMap::new())),
                receipt_path: "unused".into(),
                session_id: "session-test".into(),
                events,
                state: Mutex::new(State {
                    run_id: Some("run-test".into()),
                    phase: "running".into(),
                    pending: None,
                    history: json!({"messages":[]}),
                    live_text: "partial reply".into(),
                    live_reasoning: String::new(),
                    last_tool: json!({"name":"read_file"}),
                    pending_text: String::new(),
                    pending_reasoning: String::new(),
                    last_delta: Instant::now(),
                }),
                wake: Condvar::new(),
                disconnected: AtomicBool::new(false),
            },
            rx,
        )
    }
    #[test]
    fn recovery_snapshot_is_bounded_and_has_epoch_watermark_and_live_state() {
        let (shared, _rx) = shared(4);
        let mut ledger = BTreeMap::new();
        for n in 0..10000 {
            ledger.insert(
                format!("{n:0256}"),
                Receipt {
                    ordinal: n + 1,
                    fingerprint: "a".repeat(64),
                    state: "uncertain".into(),
                    run_id: "b".repeat(32),
                    tools: Vec::new(),
                },
            );
        }
        let (tx, _) = mpsc::sync_channel(1);
        let request = crate::protocol::Request {
            protocol: 1,
            request_id: "snap".into(),
            method: "snapshot".into(),
            params: json!({}),
        };
        let result = handle_control(&shared, &Mutex::new(ledger), Path::new("unused"), &tx, &request).unwrap();
        assert_eq!(result["snapshot_watermark"], 42);
        assert_eq!(result["worker_instance_id"], "epoch-test");
        assert_eq!(result["session_id"], "session-test");
        assert_eq!(result["live_text"], "partial reply");
        assert_eq!(result["receipts"].as_object().unwrap().len(), 100);
        assert_eq!(result["receipts_truncated"], true);
        assert!(result.to_string().len() < 128 * 1024);
    }
    #[test]
    fn escaped_receipt_ids_are_bounded_before_acceptance_and_terminal_updates_fit() {
        let dir = std::env::temp_dir().join(format!("rusty-ledger-{}", identity().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("receipts.json");
        let mut ledger = BTreeMap::new();
        // Sum each encoded map entry once instead of repeatedly cloning and
        // serializing the complete near-8-MiB ledger during fixture setup.
        let mut bytes = 2usize;
        let mut n = 0u64;
        for index in 0..10000u64 {
            let id = format!("{index:016}{}", "\u{0001}".repeat(240));
            let mut receipt = Receipt {
                ordinal: index + 1,
                fingerprint: "a".repeat(64),
                state: "not_started".into(),
                run_id: "b".repeat(32),
                tools: Vec::new(),
            };
            let entry = serde_json::to_vec(&id).unwrap().len()
                + 1
                + serde_json::to_vec(&receipt).unwrap().len()
                + usize::from(!ledger.is_empty());
            if bytes + entry > RECEIPT_BYTES - 64 * 1024 {
                break;
            }
            bytes += entry;
            receipt.state = "accepted".into();
            ledger.insert(id, receipt);
            n += 1;
        }
        assert!(n < 10000);
        assert!(receipt_value(&ledger, true).is_ok());
        save_receipts(&path, &ledger).unwrap();
        let (shared, _events) = shared(4);
        let ledger = Mutex::new(ledger);
        let (tx, rx) = mpsc::sync_channel(1);
        let mut accepted = 0;
        loop {
            let before = std::fs::read(&path).unwrap();
            let request = crate::protocol::Request {
                protocol: 1,
                request_id: format!("valid-{accepted:0100}"),
                method: "message.submit".into(),
                params: json!({"text":"do work"}),
            };
            // Use valid printable IDs for admission; escaped legacy IDs above
            // still have to be counted honestly when reopening old ledgers.
            match handle_control(&shared, &ledger, &path, &tx, &request) {
                Ok(_) => {
                    rx.try_recv().unwrap();
                    accepted += 1;
                    assert!(accepted < 16);
                }
                Err(error) => {
                    assert!(error.contains("8 MiB"), "{error}");
                    assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
                    assert!(!ledger.lock().unwrap().contains_key(&request.request_id));
                    assert_eq!(std::fs::read(&path).unwrap(), before);
                    break;
                }
            }
        }
        let mut ledger = ledger.lock().unwrap();
        for receipt in ledger.values_mut() {
            receipt.state = "interrupted".into();
        }
        ledger.values_mut().next().unwrap().state = "completed".into();
        save_receipts(&path, &ledger).unwrap();
        let persisted = std::fs::read(&path).unwrap();
        assert!(persisted.len() <= RECEIPT_BYTES);
        assert!(persisted.starts_with(&[0x1f, 0x8b]), "large ledgers use existing session compression");
        let decoded = read_receipt_bytes(&path).unwrap();
        assert!(decoded.len() <= RECEIPT_BYTES);
        let reopened: BTreeMap<String, Receipt> = serde_json::from_slice(&decoded).unwrap();
        assert_eq!(reopened.values().filter(|r| r.state == "completed").count(), 1);
        assert_eq!(reopened.len(), n as usize + accepted);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn plain_and_compressed_receipts_recover_with_the_same_decoded_bound() {
        let dir = std::env::temp_dir().join(format!("rusty-ledger-codec-{}", identity().unwrap()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("receipts.json");
        let ledger: BTreeMap<_, _> = (0..500)
            .map(|n| {
                (
                    format!("request-{n}"),
                    Receipt {
                        ordinal: n + 1,
                        fingerprint: "a".repeat(64),
                        state: "running".into(),
                        run_id: format!("{n:032}"),
                        tools: Vec::new(),
                    },
                )
            })
            .collect();
        let plain = serde_json::to_vec(&ledger).unwrap();
        assert!(plain.len() > 64 * 1024);
        std::fs::write(&path, &plain).unwrap();
        let recovered = load_receipts(&path).unwrap();
        assert_eq!(recovered.len(), 500);
        assert!(recovered.values().all(|r| r.state == "uncertain"));
        assert!(std::fs::read(&path).unwrap().starts_with(&[0x1f, 0x8b]));
        let reopened = load_receipts(&path).unwrap();
        assert_eq!(reopened.len(), 500);
        assert!(reopened.values().all(|r| r.state == "uncertain"));
        // A tiny physical stream cannot bypass the decoded allocation cap.
        use std::io::Write;
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gzip.write_all(&vec![b' '; RECEIPT_BYTES + 1]).unwrap();
        let oversized = gzip.finish().unwrap();
        assert!(oversized.len() < 64 * 1024);
        std::fs::write(&path, oversized).unwrap();
        assert!(read_receipt_bytes(&path).unwrap_err().is::<ReceiptOverLimit>());
        std::fs::write(&path, [0x1f, 0x8b, 0, 0]).unwrap();
        assert!(!read_receipt_bytes(&path).unwrap_err().is::<ReceiptOverLimit>());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn older_uncertain_receipts_remain_visible_beyond_recent_page() {
        let (shared, _events) = shared(4);
        shared.state.lock().unwrap().phase = "not_started".into();
        let ledger = (0..150)
            .map(|n| {
                (
                    format!("request-{n}"),
                    Receipt {
                        ordinal: n,
                        fingerprint: "a".repeat(64),
                        state: if n == 0 { "uncertain" } else { "not_started" }.into(),
                        run_id: format!("{n:032}"),
                        tools: Vec::new(),
                    },
                )
            })
            .collect();
        let snapshot = receipt_snapshot(&shared, &ledger);
        assert_eq!(snapshot["phase"], "not_started");
        assert!(snapshot["receipts"].get("request-0").is_none());
        assert_eq!(snapshot["uncertain_run_count"], 1);
        assert_eq!(snapshot["uncertain_runs"][0]["request_id"], "request-0");
        assert_eq!(snapshot["uncertain_runs_truncated"], false);
    }

    #[test]
    fn snapshot_watermark_covers_coalesced_text_before_later_completion() {
        let (shared, rx) = shared(8);
        {
            let mut state = shared.state.lock().unwrap();
            state.live_text = "a".into();
            state.pending_text = "a".into();
            state.last_delta = Instant::now();
        }
        let snapshot = shared.snapshot();
        let delta = rx.try_recv().expect("snapshot must flush the pending delta before its watermark");
        assert_eq!(delta.kind, "message.delta");
        assert_eq!(delta.seq, 43);
        assert_eq!(delta.payload["text"], "a");
        assert_eq!(snapshot["snapshot_watermark"], delta.seq);
        assert_eq!(snapshot["live_text"], "a");
        // Applying events strictly after the watermark must not append "a" twice.
        {
            let mut state = shared.state.lock().unwrap();
            flush_deltas(&shared, &mut state, true);
            state.phase = "completed".into();
        }
        assert!(matches!(rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
        shared.events.emit("run.completed", json!({"run_id":"run-test","state":"completed"}));
        let completion = rx.try_recv().unwrap();
        assert!(completion.seq > snapshot["snapshot_watermark"].as_u64().unwrap());
        assert_eq!(completion.kind, "run.completed");
        let completed = shared.snapshot();
        assert_eq!(completed["phase"], "completed");
        assert_eq!(completed["snapshot_watermark"], completion.seq);
        assert_eq!(completed["live_text"], "a");
    }

    #[test]
    fn dropped_stream_delta_requests_resync_without_reordering_sequences() {
        let (shared, rx) = shared(2);
        for _ in 0..3 {
            shared.events.emit("message.delta", json!({"text":"next"}));
        }
        assert!(shared.events.lost.load(Ordering::Acquire));
        assert!(!shared.events.closed.load(Ordering::Acquire));
        let a = rx.recv().unwrap();
        let b = rx.recv().unwrap();
        assert!(a.seq < b.seq);
        shared.events.emit("message.delta", json!({"text":"last"}));
        let marker = rx.recv().unwrap();
        let next = rx.recv().unwrap();
        assert_eq!(marker.kind, "resync_required");
        assert!(b.seq < marker.seq && marker.seq < next.seq);
    }
    #[test]
    fn overflow_of_critical_events_requires_resync_without_cancelling_worker() {
        let (shared, rx) = shared(2);
        shared.events.emit("approval.requested", json!({"approval_id":"one"}));
        shared.events.emit("run.completed", json!({"run_id":"one"}));
        shared.events.emit("reply", json!({"request_id":"overflow"}));
        assert!(shared.events.lost.load(Ordering::Acquire));
        assert!(!shared.events.closed.load(Ordering::Acquire));
        rx.recv().unwrap();
        rx.recv().unwrap();
        shared.events.emit("reply", json!({"request_id":"fresh"}));
        assert_eq!(rx.recv().unwrap().kind, "resync_required");
        assert_eq!(rx.recv().unwrap().payload["request_id"], "fresh");
    }

    fn output_pair() -> (File, std::os::unix::net::UnixStream) {
        use std::os::fd::IntoRawFd;
        let (output, input) = std::os::unix::net::UnixStream::pair().unwrap();
        let size: libc::c_int = 16 * 1024;
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    output.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        input.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        (unsafe { File::from_raw_fd(output.into_raw_fd()) }, input)
    }
    #[test]
    fn byte_overflow_below_count_cap_emits_idle_resync_and_releases_every_byte() {
        use std::io::BufRead;
        let (output, input) = output_pair();
        let events = Events::new(output, "byte-test".into());
        let payload = json!({"text":"x".repeat(3*1024*1024)});
        for _ in 0..8 {
            events.emit("future.large", payload.clone());
        }
        assert!(events.lost.load(Ordering::Acquire), "byte bound did not reject any of only eight events");
        let used = events.budget.used.load(Ordering::Acquire);
        assert!(used > 0 && used <= EVENT_BYTES);
        let (tx, received) = mpsc::channel();
        let reader = std::thread::spawn(move || {
            let mut prior = 0;
            for line in BufReader::new(input).lines() {
                let event: crate::protocol::Event = serde_json::from_str(&line.unwrap()).unwrap();
                assert!(event.seq > prior, "writer/producer marker reordered sequences");
                prior = event.seq;
                if event.kind == "resync_required" {
                    tx.send(event.payload).unwrap();
                }
            }
        });
        // No subsequent emit/control request: the writer must publish the loss.
        let marker = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(marker["lost_event_count"].as_u64().unwrap() > 0);
        assert!(events.finish());
        reader.join().unwrap();
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
    }
    #[test]
    fn concurrent_producers_and_writer_resync_preserve_transport_sequence_order() {
        use std::io::BufRead;
        let (output, input) = output_pair();
        let events = Arc::new(Events::new(output, "race-test".into()));
        events.emit("future.block", json!({"text":"x".repeat(256*1024)}));
        for n in 0..300 {
            events.emit("future.before", json!({"index":n}));
        }
        assert!(events.lost.load(Ordering::Acquire));
        let reader = std::thread::spawn(move || {
            let mut prior = 0;
            let mut markers = 0;
            for line in BufReader::new(input).lines() {
                let event: crate::protocol::Event = serde_json::from_str(&line.unwrap()).unwrap();
                assert!(event.seq > prior, "resync marker overtook an already queued frame");
                prior = event.seq;
                markers += usize::from(event.kind == "resync_required");
            }
            markers
        });
        let (done, completed) = mpsc::channel();
        let producers: Vec<_> = (0..4)
            .map(|owner| {
                let events = events.clone();
                let done = done.clone();
                std::thread::spawn(move || {
                    for n in 0..128 {
                        events.emit("future.race", json!({"owner":owner,"index":n}));
                    }
                    done.send(()).unwrap();
                })
            })
            .collect();
        for _ in 0..4 {
            completed.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        for producer in producers {
            producer.join().unwrap();
        }
        assert!(events.finish());
        assert!(reader.join().unwrap() > 0);
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
        assert!(!events.lost.load(Ordering::Acquire));
    }
    #[test]
    fn rejected_reply_frames_release_bytes_and_report_bounded_ids() {
        let (events, _rx) = Events::channel("reply-test".into(), 0, EVENT_BYTES);
        for n in 0..100 {
            events.emit("reply", json!({"request_id":format!("read-{n}"),"ok":true,"result":{}}));
            assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
        }
        let loss = events.losses.lock().unwrap().payload();
        assert_eq!(loss["lost_reply_count"], 100);
        assert_eq!(loss["lost_reply_ids"].as_array().unwrap().len(), 16);
        assert_eq!(loss["lost_reply_ids_truncated"], true);
        assert!(events.finish());
        let seq = events.sequence.load(Ordering::Acquire);
        events.emit("reply", json!({"request_id":"late","ok":true,"result":{}}));
        assert_eq!(events.sequence.load(Ordering::Acquire), seq, "draining admitted a new event");
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
    }
    #[test]
    fn byte_budget_charges_retained_backing_capacity_not_only_wire_length() {
        let (events, _rx) = Events::channel("allocation-test".into(), 1, EVENT_BYTES);
        let event = events.frame("future.large", json!({"text":"x".repeat(3*1024*1024)}));
        let frame = EncodedEvent::encode(&event, &events.budget).unwrap().unwrap();
        assert!(frame.bytes.capacity() > frame.bytes.len());
        assert!(frame.bytes.capacity() <= crate::protocol::MAX_FRAME_BYTES);
        assert_eq!(events.budget.used.load(Ordering::Acquire), frame.bytes.capacity());
        drop(frame);
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
    }
    #[test]
    fn encoded_frame_bound_includes_newline_and_failed_encoding_refunds_reservation() {
        let (events, _rx) = Events::channel("frame-test".into(), 1, EVENT_BYTES);
        let mut event = events.frame("future.large", json!({"text":""}));
        let overhead = serde_json::to_vec(&event).unwrap().len() + 1;
        event.payload["text"] = json!("x".repeat(crate::protocol::MAX_FRAME_BYTES - overhead));
        let frame = EncodedEvent::encode(&event, &events.budget).unwrap().unwrap();
        assert_eq!(frame.bytes.len(), crate::protocol::MAX_FRAME_BYTES);
        assert_eq!(events.budget.used.load(Ordering::Acquire), frame.bytes.len());
        drop(frame);
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
        event.payload["text"] = json!("x".repeat(crate::protocol::MAX_FRAME_BYTES - overhead + 1));
        assert!(EncodedEvent::encode(&event, &events.budget).is_err());
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
    }
    #[test]
    fn transport_error_drops_in_progress_and_queued_byte_reservations() {
        let (output, input) = output_pair();
        drop(input);
        let events = Events::new(output, "error-test".into());
        for n in 0..8 {
            events.emit("future.large", json!({"index":n,"text":"x".repeat(1024*1024)}));
        }
        assert!(!events.finish(), "broken transport reported a successful drain");
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
    }
    #[cfg(debug_assertions)]
    #[test]
    fn payload_validation_failure_refunds_all_byte_reservations() {
        let (output, _input) = output_pair();
        let events = Events::new(output, "validation-test".into());
        events.emit("approval.requested", json!({"approval_id":"missing-required-fields"}));
        assert!(!events.finish());
        assert_eq!(events.budget.used.load(Ordering::Acquire), 0);
    }

    type ApprovalThread = std::thread::JoinHandle<(&'static str, std::result::Result<bool, &'static str>)>;
    fn approval_threads(shared: &Arc<Shared>) -> Vec<ApprovalThread> {
        ["worker-one", "worker-two"]
            .into_iter()
            .map(|requester| {
                let shared = shared.clone();
                std::thread::spawn(move || {
                    (
                        requester,
                        approve_on(
                            &shared,
                            "write_file",
                            &json!({"path":"a","content":"b"}),
                            Path::new("."),
                            "write",
                            (
                                false,
                                &crate::permissions::Policy::new(crate::permissions::Mode::Ask),
                                ("local", requester),
                            ),
                        ),
                    )
                })
            })
            .collect()
    }
    fn decide(shared: &Shared, card: &Value, allowed: bool) -> std::result::Result<Value, String> {
        let (tx, _rx) = mpsc::sync_channel(1);
        handle_control(
            shared,
            &Mutex::new(BTreeMap::new()),
            Path::new("unused"),
            &tx,
            &crate::protocol::Request {
                protocol: 1,
                request_id: "decision".into(),
                method: "approval.decide".into(),
                params: json!({"approval_id":card["approval_id"],"operation_digest":card["operation_digest"],"allowed":allowed}),
            },
        )
    }
    fn next_approval_event(events: &TestEvents, kind: &str) -> Value {
        let event = events.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(event.kind, kind);
        event.payload
    }

    #[test]
    fn concurrent_approvals_are_serialized_and_keep_requester_identity() {
        let (shared, events) = shared(16);
        let shared = Arc::new(shared);
        let threads = approval_threads(&shared);
        let first = next_approval_event(&events, "approval.requested");
        assert!(events.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(threads.iter().all(|thread| !thread.is_finished()), "queued approval auto-declined");
        let wrong = json!({"approval_id":first["approval_id"],"operation_digest":"wrong"});
        assert!(decide(&shared, &wrong, true).is_err());
        decide(&shared, &first, false).unwrap();
        assert_eq!(next_approval_event(&events, "approval.resolved")["allowed"], false);
        let second = next_approval_event(&events, "approval.requested");
        assert_ne!(first["approval_id"], second["approval_id"]);
        assert_ne!(first["operation_digest"], second["operation_digest"]);
        assert_ne!(first["operation"]["requester"], second["operation"]["requester"]);
        assert!(decide(&shared, &first, true).is_err(), "replayed first decision accepted");
        let cross = json!({"approval_id":second["approval_id"],"operation_digest":first["operation_digest"]});
        assert!(decide(&shared, &cross, true).is_err(), "cross-paired digest accepted");
        decide(&shared, &second, true).unwrap();
        assert_eq!(next_approval_event(&events, "approval.resolved")["allowed"], true);
        let outcomes: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap()).collect();
        assert_eq!(
            outcomes
                .iter()
                .find(|(requester, _)| Some(*requester) == first["operation"]["requester"].as_str())
                .unwrap()
                .1,
            Ok(false)
        );
        assert_eq!(
            outcomes
                .iter()
                .find(|(requester, _)| Some(*requester) == second["operation"]["requester"].as_str())
                .unwrap()
                .1,
            Ok(true)
        );
        assert!(shared.state.lock().unwrap().pending.is_none());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn concurrent_approval_expiry_belongs_only_to_the_presented_card() {
        let (shared, events) = shared(16);
        let shared = Arc::new(shared);
        let threads = approval_threads(&shared);
        let first = next_approval_event(&events, "approval.requested");
        assert!(events.recv_timeout(Duration::from_millis(50)).is_err());
        shared.state.lock().unwrap().pending.as_mut().unwrap().expires = Instant::now() - Duration::from_secs(1);
        assert!(decide(&shared, &first, true).is_err(), "expired decision accepted");
        shared.wake.notify_all();
        let expired = next_approval_event(&events, "approval.expired");
        assert_eq!(expired["approval_id"], first["approval_id"]);
        assert_eq!(expired["reason"], "approval expired");
        let second = next_approval_event(&events, "approval.requested");
        assert_ne!(second["approval_id"], first["approval_id"]);
        assert!(shared.state.lock().unwrap().pending.as_ref().unwrap().expires > Instant::now());
        decide(&shared, &second, true).unwrap();
        let resolved = next_approval_event(&events, "approval.resolved");
        assert_eq!(resolved["approval_id"], second["approval_id"]);
        let outcomes: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap().1).collect();
        assert!(outcomes.contains(&Err("approval expired")));
        assert!(outcomes.contains(&Ok(true)));
        assert!(shared.state.lock().unwrap().pending.is_none());
        assert!(events.try_recv().is_err());
    }

    #[test]
    fn cancellation_of_a_queued_approval_never_presents_or_expires_it() {
        let (shared, events) = shared(16);
        let shared = Arc::new(shared);
        let threads = approval_threads(&shared);
        let first = next_approval_event(&events, "approval.requested");
        assert!(events.recv_timeout(Duration::from_millis(50)).is_err());
        shared.disconnected.store(true, Ordering::Release);
        shared.wake.notify_all();
        let expired = next_approval_event(&events, "approval.expired");
        assert_eq!(expired["approval_id"], first["approval_id"]);
        assert_eq!(expired["reason"], "approval interrupted");
        let outcomes: Vec<_> = threads.into_iter().map(|thread| thread.join().unwrap().1).collect();
        assert!(outcomes.contains(&Err("approval interrupted")));
        assert!(outcomes.contains(&Err("approval interrupted before presentation")));
        assert!(shared.state.lock().unwrap().pending.is_none());
        assert!(events.try_recv().is_err(), "queued card emitted an event after cancellation");
    }

    #[test]
    fn unicode_live_buffers_and_history_pages_have_hard_bounds() {
        let mut text = String::new();
        append_bounded(&mut text, &"🦀".repeat(100000));
        assert!(text.len() <= 128 * 1024);
        assert!(text.is_char_boundary(text.len()));
        let history = vec![json!({"role":"assistant","content":"x".repeat(16000)}); 300];
        let page = history_snapshot(&history);
        assert_eq!(page["truncated"], true);
        assert!(page.to_string().len() < 513 * 1024);
    }
    #[test]
    fn expired_or_already_decided_approval_rejects_without_changing_answer() {
        let (shared, _rx) = shared(4);
        shared.state.lock().unwrap().pending = Some(Pending {
            id: "approval".into(),
            digest: "digest".into(),
            run_id: "run-test".into(),
            expires: Instant::now() - Duration::from_secs(1),
            typed: true,
            operation: json!({"command":"git clean -fdx"}),
            answer: None,
        });
        let request = crate::protocol::Request {
            protocol: 1,
            request_id: "decide".into(),
            method: "approval.decide".into(),
            params: json!({"approval_id":"approval","operation_digest":"digest","allowed":true,"typed":"yes"}),
        };
        let (tx, _) = mpsc::sync_channel(1);
        let ledger = Mutex::new(BTreeMap::new());
        assert!(handle_control(&shared, &ledger, Path::new("unused"), &tx, &request).is_err());
        assert_eq!(shared.state.lock().unwrap().pending.as_ref().unwrap().answer, None);
        {
            let mut s = shared.state.lock().unwrap();
            let p = s.pending.as_mut().unwrap();
            p.expires = Instant::now() + Duration::from_secs(60);
            p.answer = Some(false);
        }
        assert!(handle_control(&shared, &ledger, Path::new("unused"), &tx, &request).is_err());
        assert_eq!(shared.state.lock().unwrap().pending.as_ref().unwrap().answer, Some(false));
    }
}
