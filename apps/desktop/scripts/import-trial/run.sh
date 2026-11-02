#!/usr/bin/env bash
# Builds packs in a throwaway profile, the way the plan's version 4 baseline was made
# (docs/plans/import-material-fixes/plan.md, "To regenerate the baseline"). A small page on the
# app's devUrl stands in for the UI and drives its commands, so it needs a debug binary built
# without an embedded frontend (`cargo build`), and it renders no app thumbnails. With TRIAL_UI
# set, the same port serves the real frontend (`bun run vite:build`) and gate.js imports each pack
# through the real Import pack dialog, then waits for the previews, so thumbnails render.
#
#   apps/desktop/scripts/import-trial/run.sh <new folder>
#
# The folder must be new or empty, so a trial can never write to an existing baseline or to a real
# library. It ends up as the baseline is laid out: libs/, data/, cache/ and h/, with h/log.jsonl
# the page's log, h/app-*.log the app's and, under the real UI, h/watch.log the memory it saw. Then
# audit it:
#
#   python3 packages/preview-engine/tools/glb_audit.py manifest <folder>/libs/Fab \
#     <folder>/libs/Unity --out <manifest outside the folder>
#
# A gate stage is also read by gate_check.py, which checks each pack against its dialog tree, and
# gate-sheets.sh builds the after sheets from its thumbnails.
#
# Environment:
#   TRIAL_REPO     the checkout holding the debug binary and the preview engine (default this
#                  one). The baseline's is a worktree at 63efa51.
#   TRIAL_PACKS    the pack list (default packs.json here)
#   TRIAL_SECONDS  seconds before the app is stopped (default 5400)
#   TRIAL_UI       set to drive the real frontend
#   TRIAL_STAGE    this run's name, for its logs h/app-<stage>.log and h/log-<stage>.jsonl
#                  (default run, whose log stays h/log.jsonl). A folder this script made can be
#                  opened again under a new stage, to build more packs in the same profile.
#   TRIAL_MIN_AVAILABLE_MB  MemAvailable below which the app is stopped (default 1500), so a trial
#                  can't take the machine down with it
#   OPENDAM_MEMORY_MAX  the app's memory cap (default 10G, see scripts/memory-cap.sh)
set -euo pipefail

if (($# != 1)); then
  echo "usage: $0 <new folder>" >&2
  exit 2
fi
here=$(cd "$(dirname "$0")" && pwd)
packs=$(realpath "${TRIAL_PACKS:-$here/packs.json}")
export TRIAL_REPO
TRIAL_REPO=$(realpath "${TRIAL_REPO:-$here/../../../..}")
binary=$TRIAL_REPO/apps/desktop/src-tauri/target/debug/opendam-desktop
seconds=${TRIAL_SECONDS:-5400}
stage=${TRIAL_STAGE:-run}

if [[ ! -x $binary ]]; then
  echo "No debug binary at $binary. Run cargo build in apps/desktop/src-tauri first." >&2
  exit 1
fi
mkdir -p "$1"
export TRIAL_DIR
TRIAL_DIR=$(realpath "$1")
# Only this script writes h/trial, so no other folder passes for one of its profiles.
reopening=
if [[ -n $(ls -A "$TRIAL_DIR") ]]; then
  if [[ $stage == run || ! -e $TRIAL_DIR/h/trial ]]; then
    echo "$TRIAL_DIR isn't empty. A trial only runs in a new folder." \
      "A profile this script made opens again under a new TRIAL_STAGE." >&2
    exit 1
  fi
  if [[ -e $TRIAL_DIR/h/log-$stage.jsonl ]]; then
    echo "$TRIAL_DIR already ran stage $stage." >&2
    exit 1
  fi
  reopening=1
fi
mkdir -p "$TRIAL_DIR"/{data,cache,h,libs/Unity,libs/Fab,libs/Kenney}
touch "$TRIAL_DIR/h/trial"

frontend=()
if [[ -n ${TRIAL_UI:-} ]]; then
  frontend=("$TRIAL_REPO/apps/desktop/dist")
  if [[ ! -f ${frontend[0]}/index.html ]]; then
    echo "No built frontend at ${frontend[0]}. Run bun run vite:build in apps/desktop first." >&2
    exit 1
  fi
fi
python3 -I "$here/server.py" "$TRIAL_DIR" "${frontend[@]}" >"$TRIAL_DIR/h/server.log" 2>&1 &
server=$!
trap 'kill "$server"' EXIT
sleep 1
if ! kill -0 "$server" 2>/dev/null; then
  cat "$TRIAL_DIR/h/server.log" >&2
  echo "The server couldn't start. Is something else on port 1420?" >&2
  exit 1
fi

session() {
  TRIAL_STAGE=$1 TRIAL_SECONDS=$2 dbus-run-session -- env -u DISPLAY kwin_wayland --virtual \
    --no-lockscreen --width 1400 --height 900 --socket "wayland-import-trial-$$-$1" \
    --exit-with-session "$here/session.sh" || true
}

if [[ -z $reopening ]]; then
  # The first launch makes the master DB, with an empty pack list, because the libraries the imports
  # name can only go into a master DB that exists.
  echo '{"imports": []}' >"$TRIAL_DIR/h/packs.json"
  session first 120
  master=$TRIAL_DIR/data/com.opendam.desktop/opendam.sqlite
  if [[ ! -f $master ]]; then
    echo "The first launch made no master DB. See $TRIAL_DIR/h/app-first.log." >&2
    exit 1
  fi
  sqlite3 "$master" "INSERT INTO libraries (id, name, root_paths, created_at) VALUES
    ('unity', 'Unity', '[\"$TRIAL_DIR/libs/Unity\"]', 0),
    ('fab', 'Fab', '[\"$TRIAL_DIR/libs/Fab\"]', 0),
    ('kenney', 'Kenney', '[\"$TRIAL_DIR/libs/Kenney\"]', 0);"
fi

cp "$packs" "$TRIAL_DIR/h/packs.json"
rm -f "$TRIAL_DIR/h/log.jsonl"
session "$stage" "$seconds"

log=$TRIAL_DIR/h/log.jsonl
if [[ $stage != run ]]; then
  mv "$log" "$TRIAL_DIR/h/log-$stage.jsonl"
  log=$TRIAL_DIR/h/log-$stage.jsonl
fi
grep '"k":"result"' "$log" || true
if ! grep -q '"k":"done","v":true' "$log"; then
  echo "The trial didn't end clean. See $log." >&2
  exit 1
fi
