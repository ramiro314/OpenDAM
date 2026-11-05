#!/usr/bin/env bash
# Searches a gate profile for cap kills and OOM lines, stage by stage, and prints the memory peaks
# the report quotes. Each stage's window comes from the first and last timestamps of its own log,
# h/log-<stage>.jsonl, so this works on any profile, across reboots. It reads that stage's app log,
# the user journal for the stage's window and scopes, so another worktree's scope can't count, and
# the kernel journal for the window, whose OOM lines name no trial, so one from anywhere stands
# against the stage until a person explains it.
#
#   apps/desktop/scripts/import-trial/gate-caps.sh <trial folder>
#
# The exit status is 1 on a kill, an OOM line or a guard stop, and also when a stage can't be
# checked: no timestamps, no app log, no scopes, or a journal that doesn't reach back to its window.
# A check that couldn't look never reads as a clean stage.
set -uo pipefail
shopt -s nullglob

if (($# != 1)); then
  echo "usage: $0 <trial folder>" >&2
  exit 2
fi
h=$1/h
kills='killed by the memory cap|killed at the .* memory cap|oom-kill|out of memory|exit(ed)? (code|status) 137'
status=0

fail() {
  echo "FAIL: $*"
  status=1
}

# The Unix time of a journal's oldest entry.
oldest() {
  "$@" -q -o short-unix --no-pager 2>/dev/null | head -n 1 | cut -d. -f1
}

logs=("$h"/log-*.jsonl)
[[ -f $h/log.jsonl ]] && logs+=("$h/log.jsonl")
if ((${#logs[@]} == 0)); then
  fail "no stage log in $h"
fi
user_oldest=$(oldest journalctl --user)
kernel_oldest=$(oldest journalctl _TRANSPORT=kernel)

for log in "${logs[@]}"; do
  stage=$(basename "$log" .jsonl)
  stage=${stage#log-}
  [[ $stage == log ]] && stage=run
  first=$(head -n 1 "$log" | grep -oE '^\{"t":[0-9]+' | grep -oE '[0-9]+$')
  last=$(tail -n 1 "$log" | grep -oE '^\{"t":[0-9]+' | grep -oE '[0-9]+$')
  if [[ -z $first || -z $last ]]; then
    fail "$stage: its log has no timestamps, so it has no window"
    continue
  fi
  since=$((first / 1000 - 60))
  until=$((last / 1000 + 60))
  echo "== $stage ($(date -d "@$since" '+%F %T') to $(date -d "@$until" '+%T'))"

  app=$h/app-$stage.log
  if [[ -f $app ]]; then
    matches=$(grep -ciE "$kills" "$app")
    echo "app log matches: $matches"
    grep -iE "$kills" "$app" | cut -c1-200 | head -5
    ((matches == 0)) || fail "$stage: the app log holds a cap kill"
    killed=$(grep -ci 'the Unreal Editor was' "$app")
    echo "editor sessions the app says were killed: $killed"
    ((killed == 0)) || fail "$stage: the app says an editor session was killed"
  else
    fail "$stage: no app log"
  fi

  if [[ -z $user_oldest || $user_oldest -gt $since ]]; then
    fail "$stage: the user journal doesn't reach back to this window"
  fi
  if [[ -z $kernel_oldest || $kernel_oldest -gt $since ]]; then
    fail "$stage: the kernel journal doesn't reach back to this window"
  fi

  mapfile -t prefixes < <(grep "stage=$stage " "$h/watch.log" 2>/dev/null |
    grep -oE 'opendam-capped-[0-9]+-' | sort -u)
  if ((${#prefixes[@]} == 0)); then
    fail "$stage: watch.log names no scope of this stage"
  else
    scopes=$(
      IFS='|'
      echo "${prefixes[*]}"
    )
    echo "scopes: ${prefixes[*]}"
    lines=$(journalctl --user --since "@$since" --until "@$until" --no-pager 2>/dev/null |
      grep -E "$scopes")
    [[ -n $lines ]] || fail "$stage: the user journal holds nothing of its scopes"
    ooms=$(grep -ciE 'oom' <<<"$lines")
    # The app's own stop_leftovers (capped.rs), after a child has ended. A cap kill shows above.
    cleanups=$(grep -c 'on client request' <<<"$lines")
    others=$(grep -iE 'killed' <<<"$lines" | grep -vc 'on client request')
    echo "user journal OOM lines for them: $ooms"
    echo "scope clean-ups, client request: $cleanups"
    echo "other kills: $others"
    ((ooms == 0)) || fail "$stage: the user journal holds an OOM line"
    ((others == 0)) || fail "$stage: the user journal holds a kill"
    grep -oE 'opendam-capped-.*Consumed.*' <<<"$lines" |
      sed -E 's/\.scope: Consumed .* wall clock time, / peak /' | sort -t' ' -k3 -h | tail -8
  fi

  # A scope's peak counts file cache, which the kernel reclaims before it kills, so the peak alone
  # can't say how near a stage came to a kill. session.sh's h/memstat.tsv splits it. With swap at
  # 0, what the cap can't reclaim is anon, shmem (counted in memory.stat's file too) and kernel
  # memory but its reclaimable slab: "held" is the most of that sum in any one sample, the figure
  # to compare with the cap. "File cache" is file less shmem. max_events counts the times the
  # scope hit its cap and reclaimed, and only grows, so the most of it is the scope's count.
  if [[ -f $h/memstat.tsv ]]; then
    awk -F'\t' -v s="$stage" '
      # Columns by the header session.sh writes, so its stat_fields can change.
      NR == 1 { for (i = 1; i <= NF; i++) col[$i] = i; next }
      $col["stage"] == s {
        k = $col["scope"]
        held = $col["anon"] + $col["shmem"] + $col["kernel"] - $col["slab_reclaimable"]
        cache = $col["file"] - $col["shmem"]
        if (held > most[k]) most[k] = held
        if ($col["anon"] > anon[k]) anon[k] = $col["anon"]
        if ($col["shmem"] > shmem[k]) shmem[k] = $col["shmem"]
        if (cache > file[k]) file[k] = cache
        if ($col["peak"] > peak[k]) peak[k] = $col["peak"]
        if ($col["max_events"] > events[k]) events[k] = $col["max_events"]
      }
      END {
        for (k in most)
          printf "memory.stat %s: peak %d MiB, most held (anon, shmem, unreclaimable kernel) %d MiB, most anon %d MiB, most shmem %d MiB, most file cache %d MiB, %d reclaims at the cap\n",
            k, peak[k] / 1048576, most[k] / 1048576, anon[k] / 1048576, shmem[k] / 1048576,
            file[k] / 1048576, events[k]
      }' "$h/memstat.tsv"
  else
    echo "no h/memstat.tsv, so the peaks below don't split file cache from anon"
  fi

  kernel=$(journalctl _TRANSPORT=kernel --since "@$since" --until "@$until" --no-pager 2>/dev/null |
    grep -ciE 'out of memory|oom-kill|oom_reaper')
  echo "kernel OOM lines in the window: $kernel"
  ((kernel == 0)) || fail "$stage: the kernel journal holds an OOM line"

  guard=$(grep -A1 "stage=$stage " "$h/watch.log" 2>/dev/null | grep -c 'low memory')
  echo "guard stops: $guard"
  ((guard == 0)) || fail "$stage: the memory guard stopped it"
done
exit "$status"
