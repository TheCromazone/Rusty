set -o pipefail
cargo test -q 2>&1 | tail -3 && grep -q 'crme-brle' src/lib.rs && ! grep -q 'todo!' src/lib.rs
