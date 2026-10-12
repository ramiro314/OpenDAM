# One entry point for local work, CI and Mission Control.
# Set up a checkout or worktree with `mise install && make deps`.

DESKTOP := apps/desktop
TAURI := $(DESKTOP)/src-tauri
PREVIEW := packages/preview-engine

# Stamps live inside the directories they describe, so deleting
# node_modules or .venv also forces a reinstall.
JS_STAMP := node_modules/.deps-stamp
PY_STAMP := $(PREVIEW)/.venv/.deps-stamp

.PHONY: deps dev lint check build

deps: $(JS_STAMP) $(PY_STAMP)

$(JS_STAMP): package.json $(DESKTOP)/package.json bun.lock
	bun install --frozen-lockfile
	@touch $@

$(PY_STAMP): $(PREVIEW)/pyproject.toml $(PREVIEW)/uv.lock $(PREVIEW)/.python-version
	cd $(PREVIEW) && uv sync --frozen
	@touch $@

dev: deps
	cd $(DESKTOP) && bun run dev

lint: deps
	cd $(TAURI) && cargo clippy --locked --all-targets -- -D warnings
	cd $(PREVIEW) && uv run --frozen ruff check

check: deps
	bun .github/scripts/check-tauri-versions.mjs
	cd $(DESKTOP) && bun run typecheck
	cd $(TAURI) && cargo check --locked

# tauri build runs the vite build first (beforeBuildCommand in tauri.conf.json).
build: deps
	cd $(DESKTOP) && bun run tauri build --no-bundle
