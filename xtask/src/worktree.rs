//! A worktree gets its own source tree and a compiler-specific shared cache.
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Command, ExitCode};

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

pub fn command(args: &[String]) -> Result<ExitCode> {
    let Some(branch) = args.first().filter(|b| !b.starts_with('-')) else {
        bail!("usage: cargo xtask worktree BRANCH [--dir PARENT] [--target-root DIRECTORY]");
    };
    let mut parent = super::root().parent().context("repository parent")?.join("worktrees");
    let mut target = None;
    let mut it = args[1..].iter();
    while let Some(arg) = it.next() {
        let value = it.next().with_context(|| format!("{arg} needs a directory"))?;
        match arg.as_str() {
            "--dir" => parent = PathBuf::from(value),
            "--target-root" => target = Some(std::path::absolute(value)?),
            _ => bail!("unknown worktree option: {arg}"),
        }
    }
    // Git validates the branch itself. The path must stay a single component.
    let name = branch.replace(['/', '\\'], "-");
    if name == "." || name == ".." {
        bail!("invalid worktree name");
    }
    std::fs::create_dir_all(&parent)?;
    let path = parent.canonicalize()?.join(name);
    // Always start new work from the freshly fetched remote main.
    super::sh("git", &["fetch", "origin", "main"])?;
    let exists = Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .current_dir(super::root())
        .status()?
        .success();
    if exists {
        super::sh("git", &["worktree", "add", path.to_str().context("worktree path is not UTF-8")?, branch])?;
    } else {
        super::sh(
            "git",
            &["worktree", "add", "-b", branch, path.to_str().context("worktree path is not UTF-8")?, "origin/main"],
        )?;
    }
    println!("\nRun in your shell (the wrapper locks each whole command):\ncd {}", quote(&path.to_string_lossy()));
    if let Some(target) = target {
        println!("export RUSTY_TARGET_ROOT={}", quote(&target.to_string_lossy()));
    }
    // The helper may not have landed on main yet. Use the creating checkout's
    // absolute script so a newly created branch can use the cache immediately.
    println!(
        "cargo() {{ {} \"$@\"; }}\ncargo build --locked\ncargo xtask qa --changed",
        quote(&super::root().join("scripts/cargo-shared").to_string_lossy())
    );
    Ok(ExitCode::SUCCESS)
}
