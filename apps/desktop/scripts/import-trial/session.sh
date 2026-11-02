#!/usr/bin/env bash
# run.sh's session inside the nested KWin: runs the app on the trial's throwaway profile, under the
# memory cap, until the page logs `done` or $TRIAL_SECONDS pass.
export XDG_DATA_HOME=$TRIAL_DIR/data XDG_CACHE_HOME=$TRIAL_DIR/cache OPENDAM_NO_KEYRING=1
export OPENDAM_PYTHON=$TRIAL_REPO/packages/preview-engine/.venv/bin/python
export OPENDAM_WORKER=$TRIAL_REPO/packages/preview-engine/worker.py
cd "$TRIAL_DIR" || exit 1
"$TRIAL_REPO/scripts/memory-cap.sh" "$TRIAL_REPO/apps/desktop/src-tauri/target/debug/opendam-desktop" \
  >"$TRIAL_DIR/h/app-$TRIAL_STAGE.log" 2>&1 &
pid=$!
for _ in $(seq 1 "$TRIAL_SECONDS"); do
  kill -0 "$pid" 2>/dev/null || break
  grep -q '"k":"done"' "$TRIAL_DIR/h/log.jsonl" 2>/dev/null && break
  sleep 1
done
kill "$pid" 2>/dev/null
sleep 3
