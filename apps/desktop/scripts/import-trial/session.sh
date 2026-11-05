#!/usr/bin/env bash
# run.sh's session inside the nested KWin: runs the app on the trial's throwaway profile, under the
# memory cap, until the page logs `done`, $TRIAL_SECONDS pass, h/stop appears or the machine runs
# short of memory.
export XDG_DATA_HOME=$TRIAL_DIR/data XDG_CACHE_HOME=$TRIAL_DIR/cache OPENDAM_NO_KEYRING=1
export OPENDAM_PYTHON=$TRIAL_REPO/packages/preview-engine/.venv/bin/python
export OPENDAM_WORKER=$TRIAL_REPO/packages/preview-engine/worker.py
cd "$TRIAL_DIR" || exit 1

# This trial's capped scopes, as cgroup folders. A scope has no MainPID, so a process of the trial
# names it: its environment or its command line holds a path below the trial's folder. The slash
# keeps `/x/trial` from matching `/x/trial-2`, because stop_scopes kills what this finds.
trial_scopes() {
  local scope pid
  for scope in /sys/fs/cgroup/user.slice/user-"$UID".slice/user@"$UID".service/app.slice/opendam-capped-*.scope; do
    for pid in $(cat "$scope/cgroup.procs" 2>/dev/null); do
      if grep -qaF "$TRIAL_DIR/" "/proc/$pid/environ" "/proc/$pid/cmdline" 2>/dev/null; then
        echo "$scope"
        break
      fi
    done
  done
}

# Blender and the Unreal Editor run in scopes of their own, which outlive the app and this session:
# the app dies on SIGTERM with no chance to stop them.
stop_scopes() {
  local scope
  for scope in $(trial_scopes); do
    systemctl --user kill --signal=SIGKILL "$(basename "$scope")" 2>/dev/null
    echo "$(date +%T) stopped $(basename "$scope" .scope)" >>"$TRIAL_DIR/h/watch.log"
  done
}

# One line per check: what the machine has left, and each of this trial's capped scopes with its
# peak and its cap in MiB (`max` when it has none).
watch_memory() {
  local available line scope peak cap
  available=$(awk '/^MemAvailable/ { print int($2 / 1024) }' /proc/meminfo)
  line="$(date +%T) stage=$TRIAL_STAGE available=${available}M"
  for scope in $(trial_scopes); do
    # A scope can end after trial_scopes lists it, and bash ends the caller's loop on arithmetic
    # over nothing, which would stop the app mid-import.
    peak=$(cat "$scope/memory.peak" 2>/dev/null)
    cap=$(cat "$scope/memory.max" 2>/dev/null)
    [[ -n $peak && -n $cap ]] || continue
    peak=$((peak / 1048576))
    [[ $cap == max ]] || cap=$((cap / 1048576))
    line+=" $(basename "$scope" .scope)=${peak}M/${cap}"
  done
  echo "$line" >>"$TRIAL_DIR/h/watch.log"
  if ((available < ${TRIAL_MIN_AVAILABLE_MB:-1500})); then
    echo "$(date +%T) low memory, stopping" >>"$TRIAL_DIR/h/watch.log"
    touch "$TRIAL_DIR/h/stop"
  fi
}

# Every 5 s, what each scope's memory is made of, from its memory.stat, since memory.peak counts
# file cache, which the kernel reclaims before it kills, with the app's own memory (anon, which
# can't be swapped out under MemorySwapMax=0). One h/memstat.tsv line per scope, in bytes, and one
# h/memproc.tsv line per process of it, in kB, with its peak resident size (VmHWM).
stat_fields=(anon file kernel shmem file_mapped file_dirty file_writeback slab_reclaimable)
watch_stat() {
  local now scope name stat fields field events pid
  now=$(date +%s)
  for scope in $(trial_scopes); do
    name=$(basename "$scope" .scope)
    stat=$(cat "$scope/memory.stat" 2>/dev/null) || continue
    fields=()
    for field in "${stat_fields[@]}"; do
      fields+=("$(awk -v f="$field" '$1 == f { print $2 }' <<<"$stat")")
    done
    # How often the scope reached its cap and had to reclaim, and the kills there.
    events=$(awk '$1 == "max" || $1 == "oom_kill" { printf "%s\t", $2 }' "$scope/memory.events")
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$now" "$TRIAL_STAGE" "$name" \
      "$(cat "$scope/memory.current")" "$(cat "$scope/memory.peak")" \
      "$(IFS=$'\t' && echo "${fields[*]}")" "${events%$'\t'}" >>"$TRIAL_DIR/h/memstat.tsv"
    for pid in $(cat "$scope/cgroup.procs" 2>/dev/null); do
      awk -v t="$now" -v s="$TRIAL_STAGE" -v n="$name" -v p="$pid" '
        /^Name:/ { c = $2 } /^VmHWM:/ { h = $2 } /^RssAnon:/ { a = $2 } /^RssFile:/ { f = $2 }
        /^RssShmem:/ { m = $2 }
        END { if (c != "") printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n", t, s, n, p, c, a, f, m, h }
      ' "/proc/$pid/status" 2>/dev/null >>"$TRIAL_DIR/h/memproc.tsv"
    done
  done
}
[[ -e $TRIAL_DIR/h/memstat.tsv ]] || printf 'time\tstage\tscope\tcurrent\tpeak\t%s\tmax_events\toom_kills\n' \
  "$(IFS=$'\t' && echo "${stat_fields[*]}")" >"$TRIAL_DIR/h/memstat.tsv"
[[ -e $TRIAL_DIR/h/memproc.tsv ]] || printf 'time\tstage\tscope\tpid\tname\trss_anon\trss_file\trss_shmem\thwm\n' \
  >"$TRIAL_DIR/h/memproc.tsv"

rm -f "$TRIAL_DIR/h/stop"
"$TRIAL_REPO/scripts/memory-cap.sh" "$TRIAL_REPO/apps/desktop/src-tauri/target/debug/opendam-desktop" \
  >"$TRIAL_DIR/h/app-$TRIAL_STAGE.log" 2>&1 &
pid=$!
for ((second = 0; second < TRIAL_SECONDS; second++)); do
  kill -0 "$pid" 2>/dev/null || break
  grep -q '"k":"done"' "$TRIAL_DIR/h/log.jsonl" 2>/dev/null && break
  [[ -e $TRIAL_DIR/h/stop ]] && break
  ((second % 30 == 0)) && watch_memory
  ((second % 5 == 0)) && watch_stat
  sleep 1
done
kill "$pid" 2>/dev/null
sleep 3
stop_scopes
