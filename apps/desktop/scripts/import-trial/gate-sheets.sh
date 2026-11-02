#!/usr/bin/env bash
# Builds a gate profile's after sheets from the committed path lists, the before sheets' lists,
# through the pack map:
#
#   apps/desktop/scripts/import-trial/gate-sheets.sh <trial folder> <out folder>
#
# Each sheet is <out folder>/<list>-after.png. A list's first line names its baseline pack, and the
# Stylarts and Leartes packs are in the Fab library, the others in Unity. The lists in unreal/ have
# no baseline, so they go through a map of their own, and a pack not built yet is left out.
set -euo pipefail

if (($# != 2)); then
  echo "usage: $0 <trial folder> <out folder>" >&2
  exit 2
fi
repo=$(cd "$(dirname "$0")/../../../.." && pwd)
plan=$repo/docs/plans/import-material-fixes
mkdir -p "$2"
status=0
for list in "$plan"/sheet-paths/*.txt; do
  pack=$(head -n 1 "$list" | sed 's/^# *//')
  case $pack in
    Stylarts/* | "Leartes Studios"/*) library=Fab ;;
    *) library=Unity ;;
  esac
  name=$(basename "$list" .txt)
  "$repo/packages/preview-engine/.venv/bin/python" "$repo/packages/preview-engine/tools/contact_sheet.py" \
    "$1/libs/$library" --pack "$pack" --paths "$list" --side current \
    --pack-map "$plan/pack-map.tsv" --out "$2/$name-after.png" || status=$?
done
map=$2/unreal-pack-map.tsv
printf 'baseline_pack\tbaseline_prefix\tcurrent_pack\tcurrent_prefix\tglbs\n' >"$map"
lists=()
for list in "$plan"/sheet-paths/unreal/*.txt; do
  pack=$(head -n 1 "$list" | sed 's/^# *//')
  if [[ -d $1/libs/Fab/$pack ]]; then
    printf '%s\t\t%s\t\t0\n' "$pack" "$pack" >>"$map"
    lists+=("$list")
  fi
done
for list in "${lists[@]}"; do
  pack=$(head -n 1 "$list" | sed 's/^# *//')
  "$repo/packages/preview-engine/.venv/bin/python" "$repo/packages/preview-engine/tools/contact_sheet.py" \
    "$1/libs/Fab" --pack "$pack" --paths "$list" --side current \
    --pack-map "$map" --out "$2/$(basename "$list" .txt)-after.png" || status=$?
done
exit "$status"
