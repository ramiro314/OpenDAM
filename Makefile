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

.PHONY: deps dev lint check test build install uninstall install-paths

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

# bun and nextest write JUnit: bun to apps/desktop/test-results/junit.xml,
# nextest's ci profile to apps/desktop/src-tauri/target/nextest/ci/junit.xml.
# bun neither creates the report's directory nor fails when it can't write it.
# All three runners always run, so a TypeScript failure still leaves Rust and
# Python results.
test: deps
	mkdir -p $(DESKTOP)/test-results
	(cd $(DESKTOP) && bun test --reporter=junit --reporter-outfile=test-results/junit.xml); bun=$$?; \
	(cd $(TAURI) && cargo nextest run --locked --profile ci); nextest=$$?; \
	(cd $(PREVIEW) && uv run --frozen pytest); pytest=$$?; \
	exit $$((bun || nextest || pytest))

# tauri build runs the vite build first (beforeBuildCommand in tauri.conf.json).
# `bun run dev` and `bun run build` run tauri under scripts/memory-cap.sh (AGENTS.md,
# "Memory-heavy tools").
build: deps
	cd $(DESKTOP) && bun run build --no-bundle

# make install puts the release build under ~/.local, and make uninstall removes
# exactly these paths. The preview engine goes where preview.rs looks for it.
DATA_HOME := $(or $(XDG_DATA_HOME),$(HOME)/.local/share)
INSTALL_BIN := $(HOME)/.local/bin/opendam
INSTALL_DESKTOP := $(DATA_HOME)/applications/opendam.desktop
INSTALL_ICON := $(DATA_HOME)/icons/hicolor/128x128/apps/opendam.png
INSTALL_ENGINE := $(DATA_HOME)/opendam/preview-engine

define DESKTOP_ENTRY
[Desktop Entry]
Type=Application
Name=OpenDAM
GenericName=Asset Manager
Comment=Browse and organize game-dev assets
Exec=$(INSTALL_BIN)
Icon=opendam
Terminal=false
Categories=Graphics;Viewer;
StartupWMClass=opendam
endef
export DESKTOP_ENTRY

# make splits words on whitespace, so a path with a space would reach rm -rf
# and install as several paths. Refuse it before any recipe runs.
INSTALL_PATHS := $(INSTALL_BIN) $(INSTALL_DESKTOP) $(INSTALL_ICON) $(INSTALL_ENGINE)
install-paths:
ifneq ($(words $(INSTALL_PATHS)),4)
	$(error HOME, XDG_DATA_HOME and the install paths must not contain whitespace)
endif

install: install-paths build
	install -Dm755 $(TAURI)/target/release/opendam-desktop $(INSTALL_BIN)
	install -Dm644 $(TAURI)/icons/128x128.png $(INSTALL_ICON)
	install -d $(dir $(INSTALL_DESKTOP))
	printf '%s\n' "$$DESKTOP_ENTRY" > $(INSTALL_DESKTOP)
	install -Dm644 -t $(INSTALL_ENGINE) $(addprefix $(PREVIEW)/,worker.py textures.py fbx_nodes.py pyproject.toml uv.lock .python-version)
	cd $(INSTALL_ENGINE) && uv sync --frozen --no-dev

# Leaves the master DB (~/.local/share/com.opendam.desktop) and every library's
# .opendam/ folder alone.
uninstall: install-paths
	rm -f $(INSTALL_BIN) $(INSTALL_DESKTOP) $(INSTALL_ICON)
	rm -rf $(INSTALL_ENGINE)
	rmdir --ignore-fail-on-non-empty $(dir $(INSTALL_ENGINE)) 2>/dev/null || true
