default:
    @just --list

# Walk transcripts and report (M1)
doctor:
    cargo run --bin ck -- doctor

# Same, but don't write anything to ~/.context-keeper
doctor-dry:
    cargo run --bin ck -- doctor --dry-run

# Run all Rust tests
test:
    cargo test --workspace

# Format
fmt:
    cargo fmt --all

# Lint (deny warnings)
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Production build
build:
    cargo build --release --bin ck
