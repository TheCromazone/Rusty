//! Session identity, private atomic saves and one writer per transcript.
use anyhow::{bail, Context, Result};
use ring::rand::{SecureRandom, SystemRandom};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub fn create(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for _ in 0..8 {
        let mut bytes = [0; 16];
        SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("session identity unavailable"))?;
        let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let path = dir.join(format!("{}-{id}.json", rusty::unix_now()));
        match OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut file) => {
                file.write_all(b"{}")?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("could not reserve a new session")
}

pub struct Lease(File);
impl Lease {
    pub fn acquire(path: &Path) -> Result<Self> {
        let file = rusty::privacy::private_file(&path.with_extension("lock"), true)?;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            bail!("session is already open for writing: {}", path.display());
        }
        Ok(Self(file))
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.0.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn save(path: &Path, body: serde_json::Value) -> Result<()> {
    let tmp = path.with_extension("pending");
    let result = (|| {
        rusty::privacy::write_json(&tmp, body)?;
        File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, path).context("publishing session")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

pub fn latest(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json") && !p.to_string_lossy().ends_with(".trace.json"))
        .filter_map(|p| p.metadata().ok()?.modified().ok().map(|t| (t, p)))
        .max()
        .map(|(_, p)| p)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rapid_sessions_preserve_both_transcripts_and_refuse_two_writers() {
        let dir = std::env::temp_dir().join(format!("rusty-session-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = create(&dir).unwrap();
        let b = create(&dir).unwrap();
        assert_ne!(a, b);
        let lease = Lease::acquire(&a).unwrap();
        save(&a, serde_json::json!({"history":["first"]})).unwrap();
        save(&b, serde_json::json!({"history":["second"]})).unwrap();
        assert!(std::fs::read_to_string(&a).unwrap().contains("first"));
        assert!(std::fs::read_to_string(&b).unwrap().contains("second"));
        assert!(Lease::acquire(&a).is_err());
        drop(lease);
        assert!(Lease::acquire(&a).is_ok());
        assert!(!a.with_extension("pending").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
