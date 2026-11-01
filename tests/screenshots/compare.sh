#!/usr/bin/env bash
# Compares the screenshots capture.sh saved with the references in
# tests/screenshots/reference. For each one that differs it leaves three
# files in the output folder: <name>.png (this run), <name>.reference.png
# and <name>.diff.png (changed pixels in red).
#
#   tests/screenshots/compare.sh <folder capture.sh wrote>
#   tests/screenshots/compare.sh --update <folder capture.sh wrote>
#
# --update copies the screenshots over the references instead.
#
# A pixel counts as changed when a colour channel moves by more than FUZZ;
# a screenshot fails when more than MAX_PIXELS pixels change. Both allow
# for small antialiasing differences between runs.
set -euo pipefail

update=false
if [ "${1:-}" = --update ]; then
  update=true
  shift
fi
if [ $# -ne 1 ]; then
  echo "usage: $0 [--update] <folder capture.sh wrote>" >&2
  exit 2
fi

here=$(cd "$(dirname "$0")" && pwd)
refs=$here/reference
out=$1
fuzz=${FUZZ:-10%}
max=${MAX_PIXELS:-100}

shopt -s nullglob
shots=()
for shot in "$out"/*.png; do
  case $shot in *.reference.png | *.diff.png) continue ;; esac
  shots+=("$shot")
done
if [ ${#shots[@]} -eq 0 ]; then
  echo "no screenshots in $out" >&2
  exit 1
fi

if $update; then
  mkdir -p "$refs"
  for shot in "${shots[@]}"; do
    cp "$shot" "$refs/"
    echo "updated reference/$(basename "$shot")"
  done
  exit 0
fi

status=0
for shot in "${shots[@]}"; do
  name=$(basename "$shot" .png)
  ref=$refs/$name.png
  if [ ! -f "$ref" ]; then
    echo "FAIL $name: no reference; see CONTRIBUTING.md to make one"
    status=1
    continue
  fi
  cp "$ref" "$out/$name.reference.png"
  size=$(identify -format '%wx%h' "$shot")
  ref_size=$(identify -format '%wx%h' "$ref")
  if [ "$size" != "$ref_size" ]; then
    echo "FAIL $name: the screenshot is $size, the reference $ref_size"
    status=1
    continue
  fi
  # compare prints the count on stderr and exits 1 when any pixel differs.
  changed=$(compare -metric AE -fuzz "$fuzz" "$ref" "$shot" "$out/$name.diff.png" 2>&1 >/dev/null || true)
  changed=${changed%% *}
  # Large counts can come out as 1.2e+06.
  [[ $changed =~ ^[0-9.]+(e\+[0-9]+)?$ ]] && changed=$(printf '%.0f' "$changed")
  if ! [[ $changed =~ ^[0-9]+$ ]]; then
    echo "FAIL $name: compare failed: $changed"
    status=1
  elif [ "$changed" -gt "$max" ]; then
    echo "FAIL $name: $changed pixels changed (at most $max allowed)"
    status=1
  else
    echo "ok   $name: $changed pixels changed"
    rm "$out/$name.reference.png" "$out/$name.diff.png"
  fi
done
exit "$status"
