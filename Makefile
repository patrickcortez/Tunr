CARGO ?= cargo

.PHONY: all build release check test test-pulse run install update uninstall clean help

all: build

build:
	$(CARGO) build

release:
	$(CARGO) build --release

check:
	RUSTFLAGS="-D warnings" $(CARGO) check --all-targets

test:
	RUSTFLAGS="-D warnings" $(CARGO) test

test-pulse:
	RUSTFLAGS="-D warnings" $(CARGO) test -- --ignored

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
	@echo "  check       Type-check all targets with warnings denied"
	@echo "  test        Run unit and integration tests"
	@echo "  test-pulse  Run ignored live PulseAudio test"
	@echo "  run         Launch Termusic TUI via cargo run"
	@echo "  install     Install termusic binary via cargo install"
	@echo "  update      Rebuild and reinstall termusic binary (--force)"
	@echo "  uninstall   Uninstall termusic binary"
	@echo "  clean       Remove Cargo build artifacts"
