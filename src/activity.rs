//! Local shell lifetimes. Handles belong to one coordinator and are never
//! restored from transcript text or serialized receipts.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use crate::{backend::ShellResult, infra, signal, tools};

const MAX_RUNNING: usize = 4;
const MAX_STARTED: u64 = 32;

#[derive(Clone, Debug, Serialize, PartialEq)]
pub enum Status {
    Running,
    Exited,
    TimedOut,
    Cancelled,
    Interrupted,
    Failed,
}

/// Export only. A saved record cannot construct a process handle.
#[derive(Clone, Debug, Serialize)]
pub struct Record {
    id: String,
    command: String,
    timeout_secs: u64,
    status: Status,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    seconds: f64,
}

impl Record {
    pub fn running(&self) -> bool {
        self.status == Status::Running
    }
}

struct Entry {
    record: Record,
    started: Instant,
    cancel: Arc<AtomicBool>,
    rx: mpsc::Receiver<Result<ShellResult>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Entry {
    fn finish(&mut self, result: Result<ShellResult>, requested: Option<Status>) {
        self.record.seconds = self.started.elapsed().as_secs_f64();
        match result {
            Ok((status, stdout, stderr)) => {
                self.record.exit_code = status.flatten();
                self.record.stdout = infra::redact(&stdout).0;
                self.record.stderr = infra::redact(&stderr).0;
                self.record.status = match status {
                    Some(_) => Status::Exited,
                    None => requested.unwrap_or_else(|| {
                        if signal::interrupted() {
                            Status::Interrupted
                        } else {
                            Status::TimedOut
                        }
                    }),
                };
            }
            Err(e) => {
                self.record.status = Status::Failed;
                self.record.stderr = infra::redact(&format!("{e:#}")).0;
            }
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    fn refresh(&mut self) {
        if self.record.status != Status::Running {
            return;
        }
        match self.rx.try_recv() {
            Ok(result) => self.finish(result, None),
            Err(mpsc::TryRecvError::Disconnected) => {
                self.finish(Err(anyhow::anyhow!("shell owner disconnected")), None)
            }
            Err(mpsc::TryRecvError::Empty) => {}
        }
    }

    fn cancel(&mut self, status: Status) {
        self.refresh();
        if self.record.status == Status::Running {
            self.cancel.store(true, Ordering::Relaxed);
            let result = self.rx.recv().unwrap_or_else(|_| Err(anyhow::anyhow!("shell owner disconnected")));
            self.finish(result, Some(status));
        }
    }
}

#[derive(Default)]
pub struct Activities {
    nonce: String,
    started: u64,
    entries: BTreeMap<String, Entry>,
}

impl Activities {
    pub fn start(&mut self, command: &str, timeout: u64) -> Result<Record> {
        if command.trim().is_empty() || !(1..=600).contains(&timeout) {
            bail!("command must be nonempty and timeout_secs must be 1..600");
        }
        if self.running() >= MAX_RUNNING || self.started >= MAX_STARTED {
            bail!("activity limit reached (four active, 32 starts per session)");
        }
        if self.nonce.is_empty() {
            use ring::rand::{SecureRandom, SystemRandom};
            let mut bytes = [0; 16];
            SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("activity identity unavailable"))?;
            self.nonce = bytes.iter().map(|b| format!("{b:02x}")).collect();
        }
        self.started += 1;
        let id = format!("shell-{}-{}", self.nonce, self.started);
        let record = Record {
            id: id.clone(),
            command: infra::redact(command).0,
            timeout_secs: timeout,
            status: Status::Running,
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            seconds: 0.0,
        };
        let started = Instant::now();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = cancel.clone();
        let command = command.to_string();
        let (tx, rx) = mpsc::channel();
        crate::session_worker::bind_activity(&id)?;
        let activity_id = id.clone();
        let thread = std::thread::Builder::new()
            .name("owned-shell".into())
            .spawn(move || {
                let result = tools::run_cancellable(&command, Duration::from_secs(timeout), &worker_cancel);
                crate::session_worker::activity_finished(&activity_id);
                let _ = tx.send(result);
            })
            .inspect_err(|_| crate::session_worker::activity_finished(&id))
            .context("start shell owner")?;
        self.entries.insert(id, Entry { record: record.clone(), started, cancel, rx, thread: Some(thread) });
        Ok(record)
    }

    pub fn wait(&mut self, id: &str, seconds: u64) -> Result<Record> {
        if seconds > 30 {
            bail!("wait_secs must be 0..30; waiting does not extend the command deadline");
        }
        let entry =
            self.entries.get_mut(id).context("unknown activity handle; only this session's live handles work")?;
        entry.refresh();
        let until = Instant::now() + Duration::from_secs(seconds);
        while entry.record.status == Status::Running && Instant::now() < until {
            signal::poll_keys();
            if signal::interrupted() {
                entry.cancel(Status::Interrupted);
                break;
            }
            match entry.rx.recv_timeout(Duration::from_millis(50).min(until.saturating_duration_since(Instant::now())))
            {
                Ok(result) => entry.finish(result, None),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    entry.finish(Err(anyhow::anyhow!("shell owner disconnected")), None)
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        // Running replies intentionally stay identical: ownership and the fixed
        // deadline justify the wait, not changing timestamps or model prose.
        Ok(entry.record.clone())
    }

    pub fn cancel(&mut self, id: &str) -> Result<Record> {
        let entry = self.entries.get_mut(id).context("unknown activity handle; cannot cancel a foreign process")?;
        entry.cancel(Status::Cancelled);
        Ok(entry.record.clone())
    }

    pub fn waiting(&self, id: &str) -> bool {
        self.entries.get(id).is_some_and(|e| e.record.status == Status::Running)
    }

    pub fn running(&mut self) -> usize {
        self.entries.values_mut().for_each(Entry::refresh);
        self.entries.values().filter(|e| e.record.status == Status::Running).count()
    }

    pub fn records(&self) -> Vec<Record> {
        self.entries.values().map(|e| e.record.clone()).collect()
    }

    pub fn cancel_all(&mut self, interrupted: bool) -> usize {
        let n = self.running();
        for entry in self.entries.values_mut() {
            entry.cancel(if interrupted { Status::Interrupted } else { Status::Cancelled });
        }
        n
    }
}

impl Drop for Activities {
    fn drop(&mut self) {
        self.cancel_all(signal::interrupted());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deadline_is_owned_and_polling_does_not_restart_a_command() {
        let mut a = Activities::default();
        let r = a.start("sleep 30 & wait", 1).unwrap();
        let before = Instant::now();
        let result = a.wait(&r.id, 3).unwrap();
        assert_eq!(result.status, Status::TimedOut);
        assert!(before.elapsed() < Duration::from_secs(2));
        assert_eq!(a.wait(&r.id, 0).unwrap().seconds, result.seconds);
    }

    #[test]
    fn dropping_owner_cleans_up_ordinary_descendants() {
        let mut a = Activities::default();
        let r = a.start("sleep 30 & wait", 30).unwrap();
        assert!(a.waiting(&r.id));
        assert!(a.wait("forged", 0).is_err());
        let before = Instant::now();
        assert_eq!(a.cancel(&r.id).unwrap().status, Status::Cancelled);
        assert_eq!(a.running(), 0);
        assert!(before.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn active_limit_cannot_be_reset_by_polling_or_cancelled_starts() {
        let mut a = Activities::default();
        let handles: Vec<_> = (0..4).map(|_| a.start("sleep 30", 30).unwrap()).collect();
        assert!(a.start("true", 30).is_err());
        for r in handles {
            a.cancel(&r.id).unwrap();
        }
        for _ in 4..32 {
            let r = a.start("sleep 30", 30).unwrap();
            a.cancel(&r.id).unwrap();
        }
        assert!(a.start("true", 30).is_err());
        assert!(a.start("true", 0).is_err());
    }
}
