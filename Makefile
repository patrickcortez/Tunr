CARGO ?= cargo

.PHONY: all build release check test test-live test-pulse run install update uninstall clean help

all: build

build:
	$(CARGO) build

release:
	$(CARGO) build --release

check:
	RUSTFLAGS="-D warnings" $(CARGO) check --all-targets --locked

test:
	RUSTFLAGS="-D warnings" $(CARGO) test --locked

test-live:
	RUSTFLAGS="-D warnings" $(CARGO) test --locked -- --ignored

test-pulse: test-live

run:
	$(CARGO) run

install:
	$(CARGO) install --path . --locked

update:
	$(CARGO) install --path . --locked --force

uninstall:
	$(CARGO) uninstall termusic

clean:
	$(CARGO) clean

help:
	@echo "Termusic Makefile targets:"
	@echo "  build       Build debug binary"
	@echo "  release     Build optimized release binary"
	@echo "  check       Type-check all targets with warnings denied (--locked)"
	@echo "  test        Run unit and integration tests (--locked)"
	@echo "  test-live   Run ignored live audio backend tests (--locked)"
	@echo "  test-pulse  Alias for test-live (backward compatibility)"
	@echo "  run         Launch Termusic TUI via cargo run"
	@echo "  install     Install termusic binary via cargo install"
	@echo "  update      Rebuild and reinstall termusic binary (--force)"
	@echo "  uninstall   Uninstall termusic binary"
	@echo "  clean       Remove Cargo build artifacts"
	@echo "  help        Show this help message"
