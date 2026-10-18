//! Context-only side questions. The parent history and tool authority never
//! enter this state; only a frozen message snapshot and the shared client do.
use crate::display;
use crate::execution::ExecutionMode;
use crate::llm::{Client, Event};
use anyhow::Result;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::sync::{mpsc::Receiver, Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Memory {
    exchanges: Vec<Value>,
    pub usage: Vec<(String, u64, u64)>,
}
pub type Shared = Arc<Mutex<Memory>>;
struct Pending {
    rx: Receiver<Event>,
    question: String,
}
/// What a side question sees: the system prompt, the main conversation as
/// its last request saw it, and the live state note. Built into messages
/// only when a question is asked.
pub struct Conversation {
    pub system: String,
    pub history: crate::context::Snapshot,
    pub note: Option<Value>,
}

impl Conversation {
    pub fn messages(&self) -> Vec<Value> {
        let history = self.history.messages();
        let mut messages = Vec::with_capacity(history.len() + 2);
        messages.push(json!({"role":"system","content":self.system}));
        messages.extend_from_slice(history);
        messages.extend(self.note.clone());
        messages
    }
}

struct Context {
    client: Arc<Client>,
    model: String,
    snapshot: Conversation,
    memory: Shared,
    pending: Option<Pending>,
    side: Option<crate::side::Parent>,
}
thread_local! { static ACTIVE: RefCell<Option<Context>> = const { RefCell::new(None) }; }
pub struct Scope;
impl Scope {
    pub fn enter(client: Arc<Client>, model: String, snapshot: Conversation, memory: Shared) -> Self {
        ACTIVE.with(|a| *a.borrow_mut() = Some(Context { client, model, snapshot, memory, pending: None, side: None }));
        Self
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        ACTIVE.with(|a| {
            a.borrow_mut().take();
        });
    }
}
pub fn refresh(snapshot: Conversation) {
    ACTIVE.with(|a| {
        if let Some(c) = a.borrow_mut().as_mut() {
            c.snapshot = snapshot;
        }
    });
}
pub fn attach_side(parent: Option<crate::side::Parent>) {
    ACTIVE.with(|a| {
        if let Some(c) = a.borrow_mut().as_mut() {
            c.side = parent;
        }
    });
}
pub fn submit(question: &str) {
    ACTIVE.with(|a| {
        if let Some(c) = a.borrow_mut().as_mut() {
            c.start(question);
        }
    });
}
impl Context {
    fn start(&mut self, question: &str) {
        if self.side.as_ref().is_some_and(|p| crate::side::focused(&p.book)) {
            display::notice("use /back before asking a main-chat /btw question");
            return;
        }
        if self.pending.is_some() {
            display::notice("btw: a side answer is still running; try again when it finishes");
            return;
        }
        if question.is_empty() {
            let memory = self.memory.lock().unwrap();
            let text = memory
                .exchanges
                .last()
                .and_then(|m| m["content"].as_str())
                .unwrap_or("usage: /btw <question about this conversation>");
            display::notice(&format!("btw: {text}"));
            return;
        }
        let mut messages = self.snapshot.messages();
        messages.extend(self.memory.lock().unwrap().exchanges.clone());
        messages.push(json!({"role":"user","content":question}));
        let rx = self.client.start_chat(
            self.model.clone(),
            messages,
            json!([]),
            0.2,
            (ExecutionMode::Standard, "aside", Some(2048)),
        );
        self.pending = Some(Pending { rx, question: question.to_string() });
        display::notice("btw: answering separately; main work continues");
    }
    fn poll(&mut self) {
        if let Some(parent) = &self.side {
            crate::side::poll(parent);
        }
        if let Some(input) = crate::signal::take_command("/btw") {
            self.start(input.strip_prefix("/btw").unwrap_or("").trim());
        }
        let result = loop {
            match self.pending.as_ref().map(|p| p.rx.try_recv()) {
                Some(Ok(Event::Done(r))) => break Some(r),
                Some(Ok(_)) => continue,
                Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                    break Some(Err(anyhow::anyhow!("side response disconnected")))
                }
                _ => break None,
            }
        };
        if let Some(result) = result {
            let pending = self.pending.take().unwrap();
            match result {
                Err(e) => display::notice(&format!("btw unavailable: {e:#}")),
                Ok(reply) => {
                    let mut answer = reply.content;
                    if !reply.tool_calls.is_empty() {
                        answer.push_str("\nNo tools were executed. Use a read-only side chat to investigate further.");
                    }
                    if reply.finish_reason.as_deref() == Some("length") {
                        answer.push_str("\n(side answer truncated)");
                    }
                    let mut m = self.memory.lock().unwrap();
                    if let Some(u) = reply.usage {
                        m.usage.push((self.model.clone(), u.prompt, u.completion));
                    }
                    m.exchanges.push(json!({"role":"user","content":pending.question}));
                    m.exchanges.push(json!({"role":"assistant","content":crate::ui::truncate(&answer, 8000)}));
                    if m.exchanges.len() > 8 {
                        m.exchanges.drain(..2);
                    }
                    display::notice(&format!("btw: {answer}\n(end of side answer; main conversation unchanged)"));
                }
            }
        }
    }
}
pub fn poll() {
    ACTIVE.with(|a| {
        if let Some(c) = a.borrow_mut().as_mut() {
            c.poll();
        }
    });
}
pub fn settle() -> Result<()> {
    loop {
        if crate::signal::interrupted() {
            return Ok(());
        }
        crate::signal::poll_keys();
        if !ACTIVE.with(|a| a.borrow().as_ref().is_some_and(|c| c.pending.is_some())) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
