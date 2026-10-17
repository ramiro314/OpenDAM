#!/usr/bin/env bash
# Runs a command in its own systemd user scope under a memory cap, so a runaway process (the
# desktop app's WebKit web process reached about 20 GB in the scroll bench's checks mode) is
# killed on its own instead of taking down whatever else shares the caller's cgroup. See
# AGENTS.md, "Memory-heavy tools".
#
#   scripts/memory-cap.sh <command> [args...]
#
# Environment:
#   OPENDAM_MEMORY_MAX  the scope's MemoryMax (default 10G); swap is capped at 0
#
# The exit status is the command's. When the cap kills it, the whole scope goes (OOMPolicy=kill),
# the status is 137 and a "killed at the memory cap" line goes to stderr. Without a reachable
# `systemd-run --user` (macOS, CI containers) the command runs uncapped, after a one-line warning.
set -uo pipefail

if (($# == 0)); then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi
max=${OPENDAM_MEMORY_MAX:-10G}

if ! command -v systemd-run >/dev/null || ! systemctl --user show-environment >/dev/null 2>&1; then
  echo "memory-cap: systemd-run --user is unavailable, running $1 without a memory cap" >&2
  exec "$@"
fi

unit=opendam-capped-$$-$RANDOM.scope
# In the background, so a TERM or INT sent to this script reaches the command: bash runs traps
# only between commands, never while it waits on a foreground child. A background job's stdin
# is /dev/null unless it is redirected, hence <&0. With --scope, systemd-run execs the command,
# so $child is the command itself.
systemd-run --user --scope --quiet --unit="$unit" \
  -p MemoryMax="$max" -p MemorySwapMax=0 -p OOMPolicy=kill -- "$@" <&0 &
child=$!
trap 'kill -TERM "$child" 2>/dev/null' TERM INT HUP

# wait also returns when a trapped signal arrives, so wait again until the command has exited.
# Its stderr is muted so bash's "Killed" job notice doesn't stand in for the report below.
{ wait "$child"; } 2>/dev/null
status=$?
while kill -0 "$child" 2>/dev/null; do
  { wait "$child"; } 2>/dev/null
  status=$?
done

if [[ $(systemctl --user show "$unit" -p Result --value 2>/dev/null) == oom-kill ]]; then
  echo "memory-cap: $1 was killed at the $max memory cap (oom-kill in $unit)" >&2
fi
# A failed scope stays loaded until its failed state is reset
systemctl --user reset-failed "$unit" 2>/dev/null
exit "$status"
