//! Completed-session messages leave RAM but remain available for explicit exports.
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Archive {
    file: Option<File>,
    count: usize,
}

impl Archive {
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn append(&mut self, archived: &[Value], history: &[Value]) -> Result<()> {
        if archived.is_empty() && history.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = std::env::temp_dir().join(format!(
                "rusty-archive-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let file = std::fs::OpenOptions::new().read(true).write(true).create_new(true).mode(0o600).open(&path)?;
            // No directory entry survives a crash. The owned descriptor is the
            // only reference and is closed when the agent is dropped.
            std::fs::remove_file(&path)?;
            self.file = Some(file);
        }
        let file = self.file.as_mut().unwrap();
        let before = file.seek(SeekFrom::End(0))?;
        let result: Result<()> = (|| {
            let mut writer = BufWriter::new(&mut *file);
            for message in archived.iter().chain(history) {
                let mut message = message.clone();
                rusty::privacy::scrub(&mut message);
                serde_json::to_writer(&mut writer, &message)?;
                writer.write_all(b"\n")?;
            }
            writer.flush()?;
            Ok(())
        })();
        if let Err(error) = result {
            file.set_len(before).context("rolling back incomplete transcript archive")?;
            file.seek(SeekFrom::End(0))?;
            return Err(error).context("archiving completed session; live history retained");
        }
        self.count += archived.len() + history.len();
        Ok(())
    }

    /// Materialize only for explicit trajectory output, never for a model step.
    pub fn messages(&self) -> Result<Vec<Value>> {
        let Some(file) = &self.file else { return Ok(Vec::new()) };
        let mut reader = file.try_clone()?;
        reader.seek(SeekFrom::Start(0))?;
        let messages = serde_json::Deserializer::from_reader(BufReader::new(reader))
            .into_iter::<Value>()
            .collect::<std::result::Result<Vec<_>, _>>()?;
        anyhow::ensure!(messages.len() == self.count, "transcript archive count mismatch");
        Ok(messages)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn archive_preserves_order_redacts_and_has_no_linked_file() {
        let mut archive = Archive::default();
        archive
            .append(
                &[json!({"role":"user","content":"first"})],
                &[json!({"role":"assistant","content":"password = ARCHIVE_SECRET"})],
            )
            .unwrap();
        // Reading must not change where a later append writes.
        assert_eq!(archive.messages().unwrap().len(), 2);
        archive.append(&[], &[json!({"role":"user","content":"last"})]).unwrap();
        let messages = archive.messages().unwrap();
        assert_eq!(messages[0]["content"], "first");
        assert_eq!(messages[2]["content"], "last");
        assert!(!serde_json::to_string(&messages).unwrap().contains("ARCHIVE_SECRET"));
        let metadata = archive.file.as_ref().unwrap().metadata().unwrap();
        assert_eq!(metadata.nlink(), 0);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(archive.len(), 3);
    }
    #[test]
    fn failed_archive_write_does_not_advance_count_or_change_input() {
        let mut archive = Archive { file: Some(File::open("/dev/null").unwrap()), count: 0 };
        let messages = vec![json!({"role":"user","content":"keep this live"})];
        assert!(archive.append(&[], &messages).is_err());
        assert_eq!(archive.len(), 0);
        assert_eq!(messages[0]["content"], "keep this live");
    }
}
