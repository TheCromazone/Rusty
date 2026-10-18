# The visible tests must survive; hidden tests check the contract with
# assertions the agent cannot see, ignore or edit.
grep -q 'crme-brle' src/lib.rs && ! grep -q 'todo!' src/lib.rs || exit 1
mkdir -p tests && cp "$EVAL_DIR/hidden.rs" tests/zz_hidden.rs
# Name the targets so a Cargo.toml that turns off default test targets
# cannot skip them.
cargo test -q --lib --test zz_hidden 2>&1 | tail -3
test "${PIPESTATUS[0]}" -eq 0
