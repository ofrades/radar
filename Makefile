# radar — native project workspace
#
#   make            build (release, embedded terminals)
#   make install    install to ~/.local and register with the launcher
#   make dev        hot-reload loop: rebuild and restart on every save
#   make test       unit tests
#   make check      clippy, both feature sets
#   make uninstall  remove what install put down

FEATURES ?= vte

.PHONY: build install dev test check uninstall clean

build:
	cargo build --release --features $(FEATURES)

install:
	./install.sh

dev:
	./dev.sh $(FEATURES)

test:
	cargo test --features $(FEATURES)

check:
	cargo clippy --all-targets --features gui -- -D warnings
	cargo clippy --all-targets --features vte -- -D warnings
	@echo "clippy clean for gui and vte"

uninstall:
	./install.sh --uninstall

clean:
	cargo clean
