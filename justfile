set shell := ["sh", "-cu"]
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

cargo := env("CARGO", "cargo")
export RUSTFLAGS := env("RUSTFLAGS", "-D warnings")

# List available recipes
default:
    @just --list

# Build debug binary
build:
    {{cargo}} build --locked

# Build optimized release binary
release:
    {{cargo}} build --release --locked

# Type-check all targets with warnings denied
check:
    {{cargo}} check --all-targets --locked

# Run unit and integration tests
test *args:
    {{cargo}} test --locked {{args}}

# Run ignored live audio backend tests
test-live *args:
    {{cargo}} test --locked '--' --ignored {{args}}

# Type-check Android / Termux target (native on Android, cargo-ndk API 24 elsewhere)
android-check:
    {{cargo}} {{ if os() == "android" { "check --all-targets --locked" } else { "ndk -t aarch64-linux-android --platform 24 check --all-targets --locked" } }}

# Launch Termusic TUI via cargo run
run *args:
    {{cargo}} run --locked {{args}}

# Install termusic binary via cargo install
install:
    {{cargo}} install --path . --locked

# Rebuild and reinstall termusic binary (--force)
update:
    {{cargo}} install --path . --locked --force

# Uninstall termusic binary
uninstall:
    {{cargo}} uninstall termusic

# Remove Cargo build artifacts
clean:
    {{cargo}} clean
