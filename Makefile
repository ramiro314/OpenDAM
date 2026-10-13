# One entry point for local work, CI and Mission Control.
# Set up a checkout or worktree with `mise install && make deps`.

DESKTOP := apps/desktop
TAURI := $(DESKTOP)/src-tauri
PREVIEW := packages/preview-engine

# Stamps live inside the directories they describe, so deleting
# node_modules or .venv also forces a reinstall.
JS_STAMP := node_modules/.deps-stamp
PY_STAMP := $(PREVIEW)/.venv/.deps-stamp

# mise ignores an untrusted mise.toml and its shims then fail. `mise install`
# trusts it, but Mission Control worktrees run make without that: trust it
# for make's own commands, and `make deps` persists the trust for the shell.
export MISE_TRUSTED_CONFIG_PATHS := $(CURDIR)

.PHONY: deps dev lint check test build

deps: $(JS_STAMP) $(PY_STAMP)
	@if command -v mise >/dev/null; then env -u MISE_TRUSTED_CONFIG_PATHS mise trust --quiet; fi

$(JS_STAMP): package.json $(DESKTOP)/package.json bun.lock
	bun install --frozen-lockfile
	@touch $@

$(PY_STAMP): $(PREVIEW)/pyproject.toml $(PREVIEW)/uv.lock $(PREVIEW)/.python-version
	cd $(PREVIEW) && uv sync --frozen
	@touch $@

dev: deps
	cd $(DESKTOP) && bun run dev

lint: deps
	bun run lint
	bun run format:check
	cd $(TAURI) && cargo fmt --check
	cd $(TAURI) && cargo clippy --locked --all-targets -- -D warnings
	cd $(PREVIEW) && uv run --frozen ruff format --check
	cd $(PREVIEW) && uv run --frozen ruff check

check: deps
	bun .github/scripts/check-tauri-versions.mjs
	cd $(DESKTOP) && bun run typecheck
	cd $(TAURI) && cargo check --locked

# Both runners write JUnit: bun to apps/desktop/test-results/junit.xml,
# nextest's ci profile to apps/desktop/src-tauri/target/nextest/ci/junit.xml.
# bun neither creates the report's directory nor fails when it can't write it.
# Both runners always run, so a TypeScript failure still leaves Rust results.
test: deps
	mkdir -p $(DESKTOP)/test-results
	(cd $(DESKTOP) && bun test --reporter=junit --reporter-outfile=test-results/junit.xml); bun=$$?; \
	(cd $(TAURI) && cargo nextest run --locked --profile ci); nextest=$$?; \
	exit $$((bun || nextest))

# tauri build runs the vite build first (beforeBuildCommand in tauri.conf.json).
build: deps
	cd $(DESKTOP) && bun run tauri build --no-bundle
