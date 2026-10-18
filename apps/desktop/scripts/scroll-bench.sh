#!/usr/bin/env bash
# Runs the dev-only scroll benchmark (src/dev/scrollBench.ts) on a fresh copy of the golden bench
# snapshot, in a throwaway profile, inside a nested headless KWin. It never opens your live
# library or your real master DB. See docs/agents/running-the-app.md, "Scroll benchmark".
#
#   apps/desktop/scripts/scroll-bench.sh [1|checks]     (default 1: the timing scenarios)
#
# Environment:
#   OPENDAM_BENCH_GOLDEN  the golden snapshot (default ~/.cache/opendam-bench/kenney-golden)
#   BENCH_SHOTS           where checks-mode screenshots go (default <repo>/.evidence/scroll-bench)
#   BENCH_TIMEOUT         seconds before each app launch is stopped (default 1800)
#   BENCH_CHECKS          checks mode only: the checks to run, as comma-separated ids (U8, U3,U8)
#   OPENDAM_MEMORY_MAX    the app's memory cap (default 10G, see scripts/memory-cap.sh)
#   WEBKIT_*, __NV_*      passed through to the app, to bench another rendering mode
set -euo pipefail

mode=${1:-1}
desktop=$(cd "$(dirname "$0")/.." && pwd)
repo=$(cd "$desktop/../.." && pwd)
golden=${OPENDAM_BENCH_GOLDEN:-$HOME/.cache/opendam-bench/kenney-golden}
shots=${BENCH_SHOTS:-$repo/.evidence/scroll-bench}
timeout=${BENCH_TIMEOUT:-1800}
reports=$desktop/src-tauri/target/scroll-bench

if [[ ! -f $golden/.opendam/library.sqlite ]]; then
  echo "No golden snapshot at $golden. Make one first (docs/agents/running-the-app.md)." >&2
  exit 1
fi

run=$(mktemp -d "$(dirname "$golden")/run-XXXXXX")
trap 'rm -rf "$run"' EXIT

echo "Copying the golden snapshot to $run/library"
cp -a "$golden" "$run/library"
# The app registers the run copy as its only library from OPENDAM_BENCH_LIBRARY (debug builds),
# once it has created the master DB in the fresh XDG_DATA_HOME with its own schema.
mkdir -p "$run/xdg"

# A debug binary with the frontend embedded and built in Vite's development mode, so the bench
# module is in it. Not the Vite dev server: under its http://localhost origin, WebKitGTK fails
# every <img> from the asset protocol, and the grid would show no images.
echo "Building the debug binary with an embedded development-mode frontend"
(cd "$desktop" && VITE_SCROLL_BENCH="$mode" bun run tauri build --debug --no-bundle \
  --config '{"build":{"beforeBuildCommand":"NODE_ENV=development bunx vite build --mode development"}}' \
  >"$run/build.log" 2>&1) || { tail -30 "$run/build.log" >&2; exit 1; }

mkdir -p "$shots" "$reports"
# Timing runs start a fresh app process for each scenario, so no scenario's RSS includes memory an
# earlier one left in the web process. Checks run in one process.
if [[ $mode == checks ]]; then
  checks=${BENCH_CHECKS:-all}
  scenarios=("${checks// /}")
else
  scenarios=()
  for size in sm md lg; do
    for name in continuousScroll endToEnd randomJumps; do scenarios+=("$size/$name"); done
  done
fi

cat >"$run/session.sh" <<EOF
#!/usr/bin/env bash
# Runs inside the nested KWin: for each scenario, starts the app, screenshots each pause and stops
# the app at its report. The app runs under scripts/memory-cap.sh, in its own scope, so a web
# process that runs away is killed at the cap instead of taking down the caller's session.
cd "$desktop/src-tauri"

# The one-time thumbnail cleanup pass runs when a fresh run copy opens, and starts over in every
# app process until it finishes (about 25 minutes on Kenney). A warm-up process runs it to the
# end first, so no scenario shares the machine with it or measures thumbnails it would clear.
db="$run/library/.opendam/library.sqlite"
cleaned() {
  [[ \$(sqlite3 -readonly -cmd '.timeout 20000' "\$db" \\
    "SELECT value FROM library_meta WHERE key = 'thumbs_no_upscale'" 2>/dev/null) == done ]]
}
if ! cleaned; then
  echo "Warm-up: running the thumbnail cleanup pass to the end"
  XDG_DATA_HOME="$run/xdg" OPENDAM_NO_KEYRING=1 OPENDAM_BENCH_LIBRARY="$run/library" \\
    OPENDAM_SCROLL_BENCH_ONLY=warmup "$repo/scripts/memory-cap.sh" ./target/debug/opendam-desktop \\
    >"$run/warmup.log" 2>&1 &
  app=\$!
  deadline=\$((SECONDS + $timeout))
  until cleaned || ! kill -0 \$app 2>/dev/null || ((SECONDS > deadline)); do sleep 10; done
  kill \$app 2>/dev/null
  wait \$app 2>/dev/null
  # The warm-up's own report, if it got that far, is not a result: no scenario or check is named
  # warmup, so it holds an error or no checks
  sed -n 's/^\[scroll-bench\] \(done\|failed\) \([^:]*\).*/\2/p' "$run/warmup.log" | xargs -r rm -f
  if ! cleaned; then
    echo "Warm-up: the thumbnail cleanup pass did not finish; see $run/warmup.log"
    cat "$run/warmup.log" >>"$run/app.log"
    exit 1
  fi
fi

for only in ${scenarios[*]}; do
  log="$run/app-\${only//\//-}.log"
  [[ \$only == all ]] && only=
  XDG_DATA_HOME="$run/xdg" OPENDAM_NO_KEYRING=1 OPENDAM_BENCH_LIBRARY="$run/library" \\
    OPENDAM_SCROLL_BENCH_ONLY=\$only "$repo/scripts/memory-cap.sh" ./target/debug/opendam-desktop \\
    >"\$log" 2>&1 &
  app=\$!
  # tail runs beside the loop, not in a pipeline, so it can be stopped as soon as the report
  # line arrives; a pipeline would wait for tail, which waits for the app's next log line.
  # --pid ends it when the app exits without a report, as when the memory cap kills it.
  exec 3< <(timeout $timeout tail -n +1 -F --pid=\$app "\$log" 2>/dev/null)
  feeder=\$!
  while IFS= read -r line <&3; do
    case "\$line" in
      *"[scroll-bench] pause "*)
        name=\${line##*pause }
        spectacle -b -n -a -o "$shots/\$name.png" && echo "screenshot $shots/\$name.png" ;;
      *"[scroll-bench] done "* | *"[scroll-bench] failed"*)
        echo "\$line"
        break ;;
    esac
  done
  kill \$feeder \$app 2>/dev/null
  exec 3<&-
  wait \$app 2>/dev/null
  cat "\$log" >>"$run/app.log"
  # A cap kill is a finding: report it rather than start the next scenario's app
  grep -q '^memory-cap: .* killed at the' "\$log" && break
done
EOF
chmod +x "$run/session.sh"

echo "Running VITE_SCROLL_BENCH=$mode in a nested headless KWin (up to ${timeout}s per scenario)"
dbus-run-session -- env -u DISPLAY kwin_wayland --virtual --no-lockscreen \
  --width 1400 --height 900 --socket "wayland-bench-$$" --exit-with-session "$run/session.sh" ||
  true # KWin exits non-zero when the session ends; the app log says whether the run finished

cp "$run/app.log" "$reports/last-run.log"
if capped=$(grep -m1 '^memory-cap: .* killed at the' "$run/app.log"); then
  echo "$capped. That is a finding, not a flake: report it, don't rerun past it (AGENTS.md," \
    "\"Memory-heavy tools\"). The last [scroll-bench] lines before the kill:" >&2
  grep '^\[scroll-bench\]' "$run/app.log" | tail -3 >&2 || true
  exit 137
fi
mapfile -t done_reports < <(sed -n 's/^\[scroll-bench\] done //p' "$run/app.log")
if [[ ${#done_reports[@]} -ne ${#scenarios[@]} ]]; then
  echo "The run did not finish; see $reports/last-run.log" >&2
  exit 1
fi
if [[ $mode == checks ]]; then
  echo "[scroll-bench] done ${done_reports[0]}"
  exit 0
fi

# One report from the per-scenario ones: the first run's context, every run's scenarios
jq -s '. as $runs | {
  kind: "timing",
  context: (($runs[0].context | del(.scenarioFilter))
    + {isolation: "each scenario in a fresh app process",
       indexingSettled: ($runs | map(.context.indexingSettled) | all)}),
  scenarios: ($runs | map(.scenarios) | reduce .[] as $s ({}; . * $s))
}' "${done_reports[@]}" >"$run/merged.json"
rm -f "${done_reports[@]}"
# Named after the partials are gone: the last one may carry this same second's timestamp
merged=$reports/$(jq -r .context.renderMode "$run/merged.json")-$(date +%s).json
mv "$run/merged.json" "$merged"
echo "[scroll-bench] done $merged"
