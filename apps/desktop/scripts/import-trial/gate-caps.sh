#!/usr/bin/env bash
# Searches a gate profile for cap kills and OOM lines, stage by stage, and prints the memory peaks
# the report quotes. It reads each stage's app log, and the journals for that stage's window and
# scopes, so another worktree's scope can't count.
#
#   apps/desktop/scripts/import-trial/gate-caps.sh <trial folder>
#
# Each line of its table: a stage, then its boot (0 now, -1 the one before) and its window.
set -uo pipefail

if (($# != 1)); then
  echo "usage: $0 <trial folder>" >&2
  exit 2
fi
h=$1/h
kills='killed by the memory cap|killed at the .* memory cap|oom-kill|out of memory|exit(ed)? (code|status) 137'
day=2026-11-02
while read -r stage boot since until; do
  log=$h/app-$stage.log
  echo "== $stage ($since to $until, boot $boot)"
  echo "app log matches: $(grep -ciE "$kills" "$log")"
  grep -iE "$kills" "$log" | cut -c1-200 | head -5
  mapfile -t pids < <(grep "stage=$stage " "$h/watch.log" | grep -oE 'opendam-capped-[0-9]+-' | sort -u)
  scopes=$(IFS='|'; echo "${pids[*]}")
  journal=$(journalctl --user -b "$boot" --since "$day $since" --until "$day $until" --no-pager 2>/dev/null)
  echo "scopes: ${pids[*]}"
  echo "user journal OOM lines for them: $(grep -E "$scopes" <<<"$journal" | grep -ciE 'oom')"
  # The app's own stop_leftovers (capped.rs), after a child has ended. A cap kill shows above.
  echo "scope clean-ups, client request: $(grep -E "$scopes" <<<"$journal" | grep -c 'on client request')"
  echo "other kills: $(grep -E "$scopes" <<<"$journal" | grep -iE 'killed' | grep -vc 'on client request')"
  echo "editor sessions the app says were killed: $(grep -c 'the Unreal Editor was' "$log")"
  echo "kernel OOM lines in the window: $(journalctl -k -b "$boot" --since "$day $since" --until "$day $until" --no-pager 2>/dev/null | grep -ciE 'out of memory|oom-kill|oom_reaper')"
  grep -E "$scopes" <<<"$journal" | grep -oE 'opendam-capped-.*Consumed.*' | sed -E 's/\.scope: Consumed .* wall clock time, / peak /' | sort -t' ' -k3 -h | tail -8
  echo "guard stops: $(grep -c "low memory" <(grep -A1 "stage=$stage " "$h/watch.log"))"
done <<'STAGES'
packs -1 11:15:00 11:36:30
lost-ruins -1 11:36:30 11:48:00
kenney -1 11:48:00 12:40:30
waterways 0 13:17:00 13:35:00
STAGES
