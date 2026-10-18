//! Completed-session messages leave RAM but remain available for explicit exports.
use anyhow::{Context, Result};
use serde_json::Value;
use std::fs::File;
use std::io::{BufReader, BufWriter, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
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

    pub fn append(&mut self, directory: &Path, archived: &[Value], history: &[Value]) -> Result<()> {
        if archived.is_empty() && history.is_empty() {
            return Ok(());
        }
        if self.file.is_none() {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let path = directory.join(format!(
                "rusty-archive-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .with_context(|| format!("creating trajectory archive in {}", directory.display()))?;
            // Once unlinked, the owned descriptor is the only reference and is
            // closed when the agent is dropped or the process terminates.
            std::fs::remove_file(&path).context("unlinking private trajectory archive")?;
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
                &std::env::temp_dir(),
                &[json!({"role":"user","content":"first"})],
                &[json!({"role":"assistant","content":"password = ARCHIVE_SECRET"})],
            )
            .unwrap();
        // Reading must not change where a later append writes.
        assert_eq!(archive.messages().unwrap().len(), 2);
        archive.append(&std::env::temp_dir(), &[], &[json!({"role":"user","content":"last"})]).unwrap();
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
        assert!(archive.append(&std::env::temp_dir(), &[], &messages).is_err());
        assert_eq!(archive.len(), 0);
        assert_eq!(messages[0]["content"], "keep this live");
    }
}

#[cfg(test)]
mod disk_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn file_size_limit_rolls_back_partial_archive() {
        const PROBE: &str = "RUSTY_ARCHIVE_FSIZE_PROBE";
        if std::env::var_os(PROBE).is_none() {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "archive::disk_tests::file_size_limit_rolls_back_partial_archive",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(PROBE, "1")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let started = std::time::Instant::now();
            while child.try_wait().unwrap().is_none() {
                if started.elapsed() > std::time::Duration::from_secs(10) {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("archive file-size probe timed out");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            return;
        }
        let mut archive = Archive::default();
        let original = json!({"role":"user","content":"already archived"});
        archive.append(&std::env::temp_dir(), &[], std::slice::from_ref(&original)).unwrap();
        let before = archive.file.as_ref().unwrap().metadata().unwrap().len();
        let messages = vec![json!({"role":"user","content":"x".repeat(16 * 1024)})];
        let error = unsafe {
            let mut old = std::mem::zeroed::<libc::rlimit>();
            assert_eq!(libc::getrlimit(libc::RLIMIT_FSIZE, &mut old), 0);
            let old_handler = libc::signal(libc::SIGXFSZ, libc::SIG_IGN);
            assert_ne!(old_handler, libc::SIG_ERR);
            let limit = libc::rlimit { rlim_cur: 2048, rlim_max: old.rlim_max };
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &limit), 0);
            let result = archive.append(&std::env::temp_dir(), &[], &messages);
            assert_eq!(libc::setrlimit(libc::RLIMIT_FSIZE, &old), 0);
            libc::signal(libc::SIGXFSZ, old_handler);
            result.unwrap_err()
        };
        assert!(
            error.chain().any(|cause| cause
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.raw_os_error() == Some(libc::EFBIG))),
            "{error:#}"
        );
        assert_eq!(archive.len(), 1);
        assert_eq!(archive.file.as_ref().unwrap().metadata().unwrap().len(), before);
        assert_eq!(archive.messages().unwrap(), vec![original]);
        assert_eq!(messages[0]["content"].as_str().unwrap().len(), 16 * 1024);
    }
}
