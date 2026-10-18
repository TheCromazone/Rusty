//! Persistent, read-only conversations attached to one parent session.
use crate::{
    agent::Agent,
    display,
    llm::Client,
    permissions::{Mode, Policy},
    signal,
};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc::{self, Receiver},
    Arc, Mutex,
};
use std::thread::JoinHandle;
use std::time::Duration;

#[derive(Clone, Serialize, Deserialize)]
pub struct Meta {
    pub title: String,
    pub parent_id: String,
    pub base_len: usize,
    #[serde(default)]
    pub discussion: Vec<Value>,
    #[serde(default)]
    pub observations: BTreeMap<String, String>,
    #[serde(default)]
    pub sent_heads: Vec<String>,
}
#[derive(Clone)]
pub struct Parent {
    pub client: Arc<Client>,
    pub model: String,
    pub cwd: PathBuf,
    pub path: PathBuf,
    pub history: Vec<Value>,
    pub notes: String,
    pub deny: Vec<String>,
    pub window: usize,
    pub history_cap: Option<usize>,
    pub book: Shared,
    pub local: bool,
}
impl Parent {
    fn id(&self) -> String {
        self.path.file_stem().unwrap().to_string_lossy().into_owned()
    }
    fn dir(&self) -> PathBuf {
        self.path.parent().unwrap().join("sides")
    }
}
pub type Shared = Arc<Mutex<Book>>;
#[derive(Default)]
pub struct Book {
    focus: Option<String>,
    children: BTreeMap<String, Child>,
    pub usage: Vec<(String, u64, u64)>,
}
struct Child {
    title: String,
    state: State,
}
enum State {
    Idle(Box<Agent>),
    Running {
        rx: Receiver<(Box<Agent>, Result<String>, u64, u64)>,
        cancel: Arc<AtomicBool>,
        join: Option<JoinHandle<()>>,
    },
}
impl Drop for Book {
    fn drop(&mut self) {
        for child in self.children.values_mut() {
            if let State::Running { cancel, join, .. } = &mut child.state {
                cancel.store(true, Ordering::SeqCst);
                if let Some(j) = join.take() {
                    let _ = j.join();
                }
            }
        }
    }
}
pub fn focused(book: &Shared) -> bool {
    book.lock().unwrap().focus.is_some()
}
/// Whether any side chat was opened from this session.
pub fn any(book: &Shared) -> bool {
    !book.lock().unwrap().children.is_empty()
}
pub fn running(book: &Shared) -> bool {
    let book = book.lock().unwrap();
    book.focus.as_ref().is_some_and(|id| matches!(book.children[id].state, State::Running { .. }))
}
fn set_focus(book: &mut Book, id: Option<String>) {
    let token = id.as_ref().and_then(|id| book.children.get(id)).and_then(|child| match &child.state {
        State::Running { cancel, .. } => Some(cancel.clone()),
        State::Idle(_) => None,
    });
    book.focus = id;
    signal::focus_cancel(token);
}

pub fn command(parent: &Parent, input: &str) -> Result<()> {
    poll_results(&parent.book);
    if input.starts_with("/send") {
        if input.trim() != "/send main" {
            bail!("usage: /send main (from a selected side chat)");
        }
        return start_job(parent, None);
    }
    let rest = input.strip_prefix("/side").unwrap_or("").trim();
    let (action, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if input == "/back" || action == "back" {
        set_focus(&mut parent.book.lock().unwrap(), None);
        display::notice("main chat selected; side work keeps its own transcript");
        return Ok(());
    }
    if action.is_empty() || action == "list" {
        std::fs::create_dir_all(parent.dir())?;
        for entry in std::fs::read_dir(parent.dir())? {
            let path = entry?.path();
            if path.extension().is_none_or(|x| x != "json") {
                continue;
            }
            let v: Value = serde_json::from_str(&crate::session::read(&path)?)?;
            if let Ok(meta) = serde_json::from_value::<Meta>(v["side"].clone()) {
                if meta.parent_id == parent.id() {
                    display::notice(&format!("side {}: {}", path.file_stem().unwrap().to_string_lossy(), meta.title));
                }
            }
        }
        return Ok(());
    }
    if !matches!(action, "new" | "open") || arg.trim().is_empty() {
        bail!("usage: /side new <name> | /side open <id or name> | /side list | /back");
    }
    let mut book = parent.book.lock().unwrap();
    if !parent.local {
        bail!("persistent side chats currently require local tools");
    }
    if book.children.len() >= 8 && action == "new" {
        bail!("eight side chats are already open; resume another parent session before opening more");
    }
    let (id, child) = if action == "new" {
        let mut agent =
            Agent::new(parent.client.clone(), parent.model.clone(), parent.cwd.clone(), Policy::new(Mode::ReadOnly));
        agent.configure_side(
            parent,
            Meta {
                title: arg.trim().chars().take(80).collect(),
                parent_id: parent.id(),
                base_len: parent.history.len(),
                discussion: Vec::new(),
                observations: BTreeMap::new(),
                sent_heads: Vec::new(),
            },
        );
        agent.new_session(&parent.dir())?;
        let id = agent.session_path.as_ref().unwrap().file_stem().unwrap().to_string_lossy().into_owned();
        let title = agent.side_meta.as_ref().unwrap().title.clone();
        (id, Some(Child { title, state: State::Idle(Box::new(agent)) }))
    } else {
        let live: Vec<_> = book
            .children
            .iter()
            .filter(|(id, c)| id.as_str() == arg || c.title == arg)
            .map(|(id, _)| id.clone())
            .collect();
        if live.len() == 1 {
            (live[0].clone(), None)
        } else {
            if book.children.len() >= 8 {
                bail!("eight side chats are already open; restart before opening more");
            }
            let mut matches = Vec::new();
            for entry in std::fs::read_dir(parent.dir()).context("no saved side chats")? {
                let path = entry?.path();
                if path.extension().is_none_or(|x| x != "json") {
                    continue;
                }
                let value: Value = serde_json::from_str(&crate::session::read(&path)?)?;
                let Ok(meta) = serde_json::from_value::<Meta>(value["side"].clone()) else { continue };
                if meta.parent_id == parent.id() && (meta.title == arg || path.file_stem().is_some_and(|s| s == arg)) {
                    matches.push((path, meta));
                }
            }
            if matches.len() != 1 {
                bail!("side chat name is missing or ambiguous; use its ID from /side list");
            }
            let (path, meta) = matches.pop().unwrap();
            let mut agent = Agent::new(
                parent.client.clone(),
                parent.model.clone(),
                parent.cwd.clone(),
                Policy::new(Mode::ReadOnly),
            );
            agent.configure_side(parent, meta.clone());
            agent.load_session(&path)?;
            (
                path.file_stem().unwrap().to_string_lossy().into_owned(),
                Some(Child { title: meta.title, state: State::Idle(Box::new(agent)) }),
            )
        }
    };
    if let Some(child) = child {
        book.children.insert(id.clone(), child);
    }
    set_focus(&mut book, Some(id.clone()));
    display::notice(&format!("side {id} selected (read-only, live workspace); /back returns to main"));
    Ok(())
}

pub fn submit(parent: &Parent, question: &str) -> Result<()> {
    start_job(parent, Some(question))
}
fn start_job(parent: &Parent, question: Option<&str>) -> Result<()> {
    poll_results(&parent.book);
    let mut book = parent.book.lock().unwrap();
    let id = book.focus.clone().context("no side chat selected")?;
    let child = book.children.remove(&id).context("side chat missing")?;
    let Child { title, state } = child;
    let State::Idle(mut agent) = state else {
        book.children.insert(id, Child { title, state });
        bail!("side chat is still running; wait for its answer or use /back");
    };
    let source_head = crate::inbox::digest(&serde_json::to_vec(&agent.side_meta.as_ref().unwrap().discussion)?);
    if question.is_none() && agent.side_meta.as_ref().unwrap().sent_heads.contains(&source_head) {
        book.children.insert(id, Child { title, state: State::Idle(agent) });
        display::notice("this side revision was already sent to main");
        return Ok(());
    }
    let target = parent.path.clone();
    let source_id = id.clone();
    let summarizing = question.is_none();
    let (tx, rx) = mpsc::channel();
    let cancel = Arc::new(AtomicBool::new(false));
    let stop = cancel.clone();
    let question = question.map(str::to_string);
    let join = std::thread::spawn(move || {
        let _scope = signal::cancel_scope(stop);
        let before = (agent.totals.prompt, agent.totals.completion);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<String> {
            if let Some(question)=question {
                let finished=agent.run_turn(&question)?;
                let answer=agent.history.iter().rev().find(|m|m["role"]=="assistant")
                    .and_then(|m|m["content"].as_str()).unwrap_or("").to_string();
                if let Some(meta)=&mut agent.side_meta {
                    meta.discussion.push(json!({"question":crate::ui::truncate(&question,8000),"answer":crate::ui::truncate(&answer,8000),"interrupted":!finished}));
                    if meta.discussion.len()>64 {meta.discussion.remove(0);}
                }
                if !finished {bail!("side turn interrupted; partial findings are unconfirmed");}
                Ok(answer)
            } else {
                let summary=agent.summarize_side()?;
                let meta=agent.side_meta.as_mut().unwrap();
                let transfer=crate::inbox::Transfer {
                    id:crate::inbox::digest(format!("{source_id}:{source_head}").as_bytes()),parent_id:meta.parent_id.clone(),
                    source_id,source_head:source_head.clone(),title:meta.title.clone(),summary:crate::ui::truncate(&summary,8000).to_string(),observations:meta.observations.clone(),
                };
                crate::inbox::enqueue(&target,&transfer)?;
                meta.sent_heads.push(source_head);
                Ok(format!("summary queued for main:\n{summary}"))
            }
        })).unwrap_or_else(|_|Err(anyhow::anyhow!("side turn panicked")));
        let result = agent.save_session().and(result);
        let used = (agent.totals.prompt - before.0, agent.totals.completion - before.1);
        let _ = tx.send((agent, result, used.0, used.1));
    });
    book.children.insert(id.clone(), Child { title, state: State::Running { rx, cancel, join: Some(join) } });
    set_focus(&mut book, Some(id));
    display::notice(if summarizing {
        "preparing side summary for main"
    } else {
        "side question running; /back returns to main, Ctrl-C stops this side"
    });
    Ok(())
}
fn poll_results(shared: &Shared) {
    let mut book = shared.lock().unwrap();
    let ids: Vec<_> = book.children.keys().cloned().collect();
    for id in ids {
        let ready = match &book.children[&id].state {
            State::Running { rx, .. } => rx.try_recv().ok(),
            _ => None,
        };
        if let Some((agent, result, prompt, completion)) = ready {
            let child = book.children.get_mut(&id).unwrap();
            if let State::Running { join, .. } = &mut child.state {
                if let Some(j) = join.take() {
                    let _ = j.join();
                }
            }
            display::notice(&format!(
                "side {}: {}",
                child.title,
                result.unwrap_or_else(|e| format!("unavailable: {e:#}"))
            ));
            let model = agent.model.clone();
            child.state = State::Idle(agent);
            book.usage.push((model, prompt, completion));
            if book.focus.as_ref() == Some(&id) {
                signal::focus_cancel(None);
            }
        }
    }
}
pub fn poll(parent: &Parent) {
    poll_results(&parent.book);
    if let Some(input) = signal::peek_queued() {
        if input == "/back" || input == "/side" || input.starts_with("/side ") || input.starts_with("/send") {
            signal::take_queued();
            if let Err(e) = command(parent, &input) {
                display::notice(&format!("side: {e:#}"));
            }
        } else if focused(&parent.book) && !input.starts_with("/btw") {
            if !input.starts_with('/') && running(&parent.book) {
                return;
            }
            signal::take_queued();
            if input.starts_with('/') {
                display::notice("use /back before main-chat commands");
            } else if let Err(e) = submit(parent, &input) {
                display::notice(&format!("side: {e:#}"));
            }
        }
    }
}

pub fn wait(parent: &Parent) {
    loop {
        poll(parent);
        signal::poll_keys();
        let book = parent.book.lock().unwrap();
        if !book.focus.as_ref().is_some_and(|id| matches!(book.children[id].state, State::Running { .. })) {
            break;
        }
        drop(book);
        std::thread::sleep(Duration::from_millis(10));
    }
}
pub fn shutdown(shared: &Shared) {
    let mut book = shared.lock().unwrap();
    for child in book.children.values_mut() {
        if let State::Running { cancel, join, .. } = &mut child.state {
            cancel.store(true, Ordering::SeqCst);
            if let Some(j) = join.take() {
                let _ = j.join();
            }
        }
    }
    drop(book);
    poll_results(shared);
    signal::focus_cancel(None);
}
