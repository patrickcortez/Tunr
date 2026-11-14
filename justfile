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

# Launch Tunr TUI via cargo run
run *args:
    {{cargo}} run --locked {{args}}

# Stop any running tunr playback daemon before replacing or removing the binary
_stop-daemon:
    @{{ if os_family() == "windows" { "if (Get-Command tunr -ErrorAction SilentlyContinue) { try { tunr daemon stop } catch {} }; exit 0" } else { "if command -v tunr >/dev/null 2>&1; then tunr daemon stop || true; fi" } }}

# Install tunr binary via cargo install
install:
    {{cargo}} install --path . --locked

# Rebuild and reinstall tunr binary (--force)
update: _stop-daemon
    {{cargo}} install --path . --locked --force

# Uninstall tunr binary
uninstall: _stop-daemon
    {{cargo}} uninstall tunr

# Remove Cargo build artifacts
clean:
    {{cargo}} clean
