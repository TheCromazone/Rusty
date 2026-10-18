use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use rusty::advisor::{Hooks, Level};
use serde_json::json;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Local L1/L2 memory advisor for rusty")]
struct Cli {
    #[arg(long, env = "RUSTY_HOME")]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Control>,
}
#[derive(Subcommand)]
enum Control {
    Status,
    Stop,
    /// This project's lessons with their confidence, best first.
    List,
    Remember {
        #[arg(long, default_value = "fact")]
        kind: String,
        /// Who vouches for it: user, agent, compaction or reflect
        #[arg(long, default_value = "user")]
        source: String,
        /// Keep a preference for every project
        #[arg(long)]
        global: bool,
        /// A file the lesson is about (repeatable): the lesson is re-checked
        /// against its contents, as when the agent passes `files`
        #[arg(long = "file", value_name = "PATH")]
        files: Vec<String>,
        text: String,
    },
    Export {
        file: PathBuf,
    },
    Import {
        file: PathBuf,
    },
}
/// The service accepts no inherited descriptors except stdin/stdout/stderr.
/// Run once before opening its database/socket or starting any service threads.
/// Concurrent launcher probes can otherwise leave unrelated pipes alive here.
fn close_inherited_descriptors() -> Result<()> {
    let directory = std::fs::read_dir("/dev/fd").or_else(|_| std::fs::read_dir("/proc/self/fd"))?;
    let mut descriptors = Vec::new();
    for entry in directory {
        let entry = entry?;
        if let Some(fd) = entry.file_name().to_str().and_then(|s| s.parse::<i32>().ok()).filter(|fd| *fd > 2) {
            descriptors.push(fd);
        }
    }
    // The directory iterator has been dropped; open nothing before closing.
    for fd in descriptors {
        unsafe {
            libc::close(fd);
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Cli::parse();
    let home = args
        .home
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config/rusty")))
        .ok_or_else(|| anyhow::anyhow!("set RUSTY_HOME or --home"))?;
    let Some(command) = args.command else {
        close_inherited_descriptors()?;
        return rusty::advisor::serve(&home);
    };
    if matches!(command, Control::Stop) {
        let socket = home.join("memory/advisor.sock");
        if !socket.exists() {
            println!("memory daemon is not running");
            return Ok(());
        }
        let r = rusty::advisor::rpc(
            &socket,
            &rusty::advisor::Request {
                op: "shutdown".into(),
                scope: "admin".into(),
                session: "admin".into(),
                seq: 0,
                level: Level::Learn,
                data: json!(null),
            },
        )?;
        if r.error {
            bail!("{}", r.text);
        }
        println!("{}", r.text);
        return Ok(());
    }
    let mut h = Hooks::connect(&home, &std::env::current_dir()?, Level::Learn)?;
    if let Control::Remember { files, .. } = &command {
        // An anchor that doesn't resolve would be dropped silently.
        if let Some(f) = files.iter().find(|f| !std::path::Path::new(f).is_file()) {
            bail!("--file {f}: no such file");
        }
    }
    let (op, data) = match &command {
        Control::Stop => unreachable!(),
        Control::Status => ("status", json!(null)),
        Control::List => ("list", json!(null)),
        Control::Remember { kind, source, global, text, files } if !files.is_empty() => {
            let r = h.remember(kind, text, source, files, *global);
            if r.error {
                bail!("{}", r.text);
            }
            println!("{}", r.text);
            return Ok(());
        }
        Control::Remember { kind, source, global, text, .. } => {
            ("remember", json!({"kind":kind,"text":text,"source":source,"global":global}))
        }
        Control::Export { .. } => ("export", json!(null)),
        Control::Import { file } => {
            let mut text = String::new();
            GzDecoder::new(std::fs::File::open(file)?).take(4 * 1024 * 1024 + 1).read_to_string(&mut text)?;
            if text.len() > 4 * 1024 * 1024 {
                bail!("decompressed import exceeds limit");
            }
            ("import", serde_json::from_str(&text)?)
        }
    };
    let r = h.control(op, data);
    if r.error {
        bail!("{}", r.text);
    }
    if let Control::Export { file } = command {
        // Never overwrite an existing destination; snapshots can contain private context.
        let f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(file)?;
        let mut w = GzEncoder::new(f, Compression::best());
        w.write_all(&serde_json::to_vec(&r.data)?)?;
        w.finish()?;
    } else {
        println!("{}", r.text);
    }
    Ok(())
}
