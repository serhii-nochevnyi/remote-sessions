.PHONY: check check-rust check-native check-runtime build-runtime check-cli preview-cli dev doctor server

check: check-rust
	npm run check

check-rust:
	cargo fmt --all -- --check
	cargo clippy --locked -p remote-sessions-host -p remote-sessions-server -p remote-sessions-protocol -p remote-sessions-core -p remote-sessions-desktop-bridge --all-targets -- -D warnings
	cargo test --locked -p remote-sessions-host -p remote-sessions-server -p remote-sessions-protocol -p remote-sessions-core -p remote-sessions-desktop-bridge

check-native:
	cargo check --locked -p remote-sessions-client

build-runtime:
	cargo build --locked -p remote-sessions-host -p remote-sessions-server

check-runtime: build-runtime
	python3 tests/integration/runtime_lifecycle.py --report docs/P0-RUNTIME-EVIDENCE.json

dev:
	npm run dev

doctor:
	cargo run --locked -p remote-sessions-host -- doctor

server:
	cargo run --locked -p remote-sessions-server

# Opt-in: launches installed provider CLIs, but submits no model prompts.
check-cli: build-runtime
	python3 tests/integration/cli_compatibility.py --report docs/P0-CLI-EVIDENCE.json

preview-cli:
	python3 -c "import shutil; shutil.copytree('.cache/cli-captures', 'tests/terminal-preview/captures', dirs_exist_ok=True)"
	npm exec -- vite tests/terminal-preview --host 127.0.0.1 --port 1421 --strictPort

# Opt-in real local writer-lease and native desktop bridge proof.
check-desktop: build-runtime
	cargo build --locked -p remote-sessions-desktop-bridge --example probe
	python3 tests/integration/desktop_input.py
