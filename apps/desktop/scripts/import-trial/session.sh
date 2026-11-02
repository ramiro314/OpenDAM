#!/usr/bin/env bash
# run.sh's session inside the nested KWin: runs the app on the trial's throwaway profile, under the
# memory cap, until the page logs `done`, $TRIAL_SECONDS pass, h/stop appears or the machine runs
# short of memory.
export XDG_DATA_HOME=$TRIAL_DIR/data XDG_CACHE_HOME=$TRIAL_DIR/cache OPENDAM_NO_KEYRING=1
export OPENDAM_PYTHON=$TRIAL_REPO/packages/preview-engine/.venv/bin/python
export OPENDAM_WORKER=$TRIAL_REPO/packages/preview-engine/worker.py
cd "$TRIAL_DIR" || exit 1

# One line per check: what the machine has left, and each of this trial's capped scopes with its
# peak and its cap in MiB (`max` when it has none). A scope has no MainPID, so a process of the
# trial names it: its environment or its command line holds the trial's folder.
watch_memory() {
  local available line scope pid peak cap
  available=$(awk '/^MemAvailable/ { print int($2 / 1024) }' /proc/meminfo)
  line="$(date +%T) stage=$TRIAL_STAGE available=${available}M"
  for scope in /sys/fs/cgroup/user.slice/user-"$UID".slice/user@"$UID".service/app.slice/opendam-capped-*.scope; do
    for pid in $(cat "$scope/cgroup.procs" 2>/dev/null); do
      if grep -qaF "$TRIAL_DIR" "/proc/$pid/environ" "/proc/$pid/cmdline" 2>/dev/null; then
        peak=$(($(cat "$scope/memory.peak") / 1048576))
        cap=$(cat "$scope/memory.max")
        [[ $cap == max ]] || cap=$((cap / 1048576))
        line+=" $(basename "$scope" .scope)=${peak}M/${cap}"
        break
      fi
    done
  done
  echo "$line" >>"$TRIAL_DIR/h/watch.log"
  if ((available < ${TRIAL_MIN_AVAILABLE_MB:-1500})); then
    echo "$(date +%T) low memory, stopping" >>"$TRIAL_DIR/h/watch.log"
    touch "$TRIAL_DIR/h/stop"
  fi
}

rm -f "$TRIAL_DIR/h/stop"
"$TRIAL_REPO/scripts/memory-cap.sh" "$TRIAL_REPO/apps/desktop/src-tauri/target/debug/opendam-desktop" \
  >"$TRIAL_DIR/h/app-$TRIAL_STAGE.log" 2>&1 &
pid=$!
for ((second = 0; second < TRIAL_SECONDS; second++)); do
  kill -0 "$pid" 2>/dev/null || break
  grep -q '"k":"done"' "$TRIAL_DIR/h/log.jsonl" 2>/dev/null && break
  [[ -e $TRIAL_DIR/h/stop ]] && break
  ((second % 30 == 0)) && watch_memory
  sleep 1
done
kill "$pid" 2>/dev/null
sleep 3
