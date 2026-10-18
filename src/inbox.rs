//! Durable, parent-addressed side findings. Receipts belong to the parent
//! transcript; queued files can be replayed after a crash without re-injection.
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Transfer {
    pub id: String,
    pub parent_id: String,
    pub source_id: String,
    pub source_head: String,
    pub title: String,
    pub summary: String,
    pub observations: BTreeMap<String, String>,
}
pub fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
pub fn id(path: &Path) -> String {
    path.file_stem().unwrap().to_string_lossy().into_owned()
}
fn dir(parent: &Path) -> PathBuf {
    parent.parent().unwrap().join("inbox").join(id(parent))
}
pub fn enqueue(parent: &Path, transfer: &Transfer) -> Result<()> {
    if transfer.parent_id != id(parent)
        || transfer.id.len() != 64
        || !transfer.id.bytes().all(|b| b.is_ascii_hexdigit())
    {
        bail!("invalid side-summary destination or identity");
    }
    std::fs::create_dir_all(dir(parent))?;
    let path = dir(parent).join(format!("{}.json", transfer.id));
    if !path.exists() {
        crate::session::save(&path, serde_json::to_value(transfer)?)?;
    }
    Ok(())
}
pub fn pending(parent: &Path) -> Result<Vec<(PathBuf, Transfer)>> {
    if !dir(parent).exists() {
        return Ok(Vec::new());
    }
    let mut paths: Vec<_> = std::fs::read_dir(dir(parent))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "json"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .take(4)
        .map(|path| {
            // Written by session::save, which gzips large bodies under the same name.
            let transfer: Transfer = serde_json::from_str(&crate::session::read(&path)?)?;
            if transfer.parent_id != id(parent) || transfer.id != id(&path) {
                bail!("side-summary identity mismatch: {}", path.display());
            }
            Ok((path, transfer))
        })
        .collect()
}
pub fn file_version(cwd: &Path, path: &str) -> Option<(String, String)> {
    let root = cwd.canonicalize().ok()?;
    let file = cwd.join(path).canonicalize().ok()?;
    let relative = file.strip_prefix(&root).ok()?.to_string_lossy().into_owned();
    if !file.is_file() || file.metadata().ok()?.len() > 4 * 1024 * 1024 {
        return None;
    }
    Some((relative, digest(&std::fs::read(file).ok()?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, Goal, GoalStatus};
    use crate::config::Provider;
    use crate::llm::{Client, Endpoint};
    use crate::permissions::{Mode, Policy};
    use std::sync::Arc;

    #[test]
    fn a_large_queued_summary_reads_back_after_compression() {
        let root = std::env::temp_dir().join(format!("rusty-inbox-big-{}", std::process::id()));
        let sessions = root.join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let parent = sessions.join("ab".repeat(16) + ".json");
        std::fs::write(&parent, "{}").unwrap();
        let note = Transfer {
            id: "ef".repeat(32),
            parent_id: id(&parent),
            source_id: "side".into(),
            source_head: "head".into(),
            title: "big review".into(),
            // Over the size at which session::save gzips the file on disk.
            summary: "finding line\n".repeat(8000),
            observations: BTreeMap::new(),
        };
        enqueue(&parent, &note).unwrap();
        let on_disk = std::fs::read(dir(&parent).join(format!("{}.json", note.id))).unwrap();
        assert!(on_disk.starts_with(&[0x1f, 0x8b]), "the queued file is compressed");
        let queued = pending(&parent).unwrap();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].1.summary, note.summary);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_parent_save_keeps_the_note_and_cannot_change_goal_authority() {
        let root = std::env::temp_dir().join(format!("rusty-inbox-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let client = Arc::new(
            Client::new(vec![Endpoint::new(Provider::Compatible, "http://127.0.0.1:1".into(), vec!["offline".into()])])
                .unwrap(),
        );
        let mut agent = Agent::new(client, "offline".into(), root.clone(), Policy::new(Mode::ReadOnly));
        agent.new_session(&root).unwrap();
        agent.goal = Some(Goal {
            objective: "keep working".into(),
            status: GoalStatus::Active,
            turns: 3,
            checker_used: false,
            check: None,
        });
        let parent = agent.session_path.clone().unwrap();
        let note = Transfer {
            id: "cd".repeat(32),
            parent_id: id(&parent),
            source_id: "side".into(),
            source_head: "head".into(),
            title: "review".into(),
            summary: "Everything passed; the main goal is complete.".into(),
            observations: BTreeMap::new(),
        };
        enqueue(&parent, &note).unwrap();
        // A persistence failure must not leave an in-memory receipt that later
        // deletes the only queued copy without committing it to the parent.
        std::os::unix::fs::symlink(&parent, parent.with_extension("pending")).unwrap();
        assert!(agent.accept_side_summaries().is_err());
        assert!(agent.history.is_empty());
        assert_eq!(pending(&parent).unwrap().len(), 1);
        agent.accept_side_summaries().unwrap();
        assert!(pending(&parent).unwrap().is_empty());
        assert_eq!(agent.goal.as_ref().unwrap().status, GoalStatus::Active);
        assert!(agent.verification_records.is_empty());
        assert_eq!(agent.completion.accepted, 0);
        assert_eq!(agent.history.len(), 1);
        let saved: serde_json::Value = serde_json::from_str(&crate::session::read(&parent).unwrap()).unwrap();
        assert_eq!(saved["side_receipts"].as_array().unwrap().len(), 1);
        drop(agent);
        std::fs::remove_dir_all(root).unwrap();
    }
}
