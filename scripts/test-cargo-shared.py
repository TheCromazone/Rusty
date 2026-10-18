"""Check source provenance when checkout mtimes predate shared artifacts."""
import os
from pathlib import Path
import subprocess
import tempfile

helper = Path(__file__).resolve().with_name("cargo-shared")
with tempfile.TemporaryDirectory(prefix="rusty-shared-") as tmp:
    root = Path(tmp).resolve()
    a, b = root / "a", root / "b"
    (a / "src").mkdir(parents=True)
    (a / "Cargo.toml").write_text(
        '[package]\nname="cache-fixture"\nversion="0.1.0"\nedition="2021"\n[workspace]\n'
    )
    def source(label, warning=""):
        return f'fn main(){{{warning}println!("{label}:{{}}",env!("CARGO_MANIFEST_DIR"));}}\n'
    (a / "src/main.rs").write_text(source("A"))
    for args in [
        ["init", "-q"], ["add", "."],
        ["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm",
         "test: establish cache fixture", "-m", "Check workspace identity independently of timestamps."],
        ["worktree", "add", "--detach", str(b), "HEAD"],
    ]:
        subprocess.run(["git", *args], cwd=a, check=True, capture_output=True)
    (b / "src/main.rs").write_text(source("B"))
    os.utime(b / "src/main.rs", (1, 1))
    env = dict(os.environ, RUSTY_TARGET_ROOT=str(root / "target"))
    def run(tree, *args):
        return subprocess.run([str(helper), *args], cwd=tree, env=env, text=True,
                              stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=120)
    for tree, label in [(a, "A"), (b, "B"), (a, "A")]:
        out = run(tree, "run", "--quiet")
        assert out.returncode == 0, out.stderr
        assert out.stdout.strip() == f"{label}:{tree}", out.stdout
    out = run(a, "clippy", "--", "-D", "warnings")
    assert out.returncode == 0, out.stderr
    (b / "src/main.rs").write_text(source("B", "let unused=1;"))
    os.utime(b / "src/main.rs", (1, 1))
    out = run(b, "clippy", "--", "-D", "warnings")
    assert out.returncode != 0 and "unused variable" in out.stderr, out.stderr
