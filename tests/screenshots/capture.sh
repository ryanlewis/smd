#!/usr/bin/env bash
# Opens each note in this folder with smd in kitty, on a headless X server,
# and saves a screenshot of the window once the drawing stops changing.
#
#   tests/screenshots/capture.sh <smd binary> <output folder>
#
# Needs Xvfb, kitty, xdotool, xdpyinfo and ImageMagick on PATH. CI installs
# pinned versions; see .github/workflows/screenshots.yml and CONTRIBUTING.md.
set -euo pipefail

if [ $# -ne 2 ]; then
  echo "usage: $0 <smd binary> <output folder>" >&2
  exit 2
fi

here=$(cd "$(dirname "$0")" && pwd)
smd=$(realpath "$1")
mkdir -p "$2"
out=$(realpath "$2")
work=$(mktemp -d)
display=":${SCREENSHOT_DISPLAY:-99}"
# How long a note may take to settle, in seconds.
timeout=${SCREENSHOT_TIMEOUT:-60}

xvfb=
kitty=
# shellcheck disable=SC2329 # run by the trap
cleanup() {
  [ -n "$kitty" ] && kill "$kitty" 2>/dev/null || true
  [ -n "$xvfb" ] && kill "$xvfb" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

Xvfb "$display" -screen 0 1920x1440x24 -dpi 96 -nolisten tcp 2>"$work/xvfb.log" &
xvfb=$!
export DISPLAY=$display
for _ in $(seq 50); do
  xdpyinfo >/dev/null 2>&1 && break
  sleep 0.1
done
xdpyinfo >/dev/null

# No user config for smd or kitty; software OpenGL, as there is no GPU.
export XDG_CONFIG_HOME=$work/config
export KITTY_CONFIG_DIRECTORY=$work/config/kitty
export LIBGL_ALWAYS_SOFTWARE=1
mkdir -p "$KITTY_CONFIG_DIRECTORY"

# Prints a checksum of the window's pixels.
pixels() {
  import -window "$1" rgb:- | md5sum | cut -d' ' -f1
}

# Waits until three captures of window $1 a second apart match; $2 names it
# in the message if that takes too long.
settle() {
  local last='' now same=0 start=$SECONDS
  while [ "$same" -lt 2 ]; do
    if [ $((SECONDS - start)) -ge "$timeout" ]; then
      echo "$2: still changing after ${timeout}s" >&2
      return 1
    fi
    sleep 1
    now=$(pixels "$1") || {
      echo "$2: cannot capture the window" >&2
      return 1
    }
    if [ "$now" = "$last" ]; then same=$((same + 1)); else same=0; fi
    last=$now
  done
}

status=0
cd "$here"
for note in *.md; do
  name=${note%.md}
  class="smd-screenshot-$name"
  log="$work/$name.log"
  go="$work/$name.go"
  # smd starts only once $go exists: kitty is slow to answer while it sets
  # up its first frame, and smd would give up on its start-up queries and
  # draw without graphics.
  # shellcheck disable=SC2016 # expanded by the inner sh
  kitty --config "$here/kitty.conf" --class "$class" \
    sh -c 'while [ ! -e "$3" ]; do sleep 0.1; done
      "$0" "$1" 2>"$2"; echo "smd exited with $?" >>"$2"; sleep 600' \
    "$smd" "$note" "$log" "$go" &
  kitty=$!

  # kitty also makes windows that are never shown; only the shown one
  # holds the terminal.
  window=$(timeout 30 xdotool search --sync --onlyvisible --class "$class" | head -n1) || {
    echo "$note: no kitty window appeared" >&2
    status=1
    kill "$kitty" 2>/dev/null || true
    kitty=
    continue
  }
  settle "$window" "$note: kitty" || status=1
  touch "$go"
  settle "$window" "$note" || status=1

  if [ -s "$log" ]; then
    echo "$note: smd wrote:" >&2
    cat "$log" >&2
    status=1
  fi
  # -strip leaves out the date chunks, so the same pixels give the same file.
  import -window "$window" -strip "png:$out/$name.png"
  echo "$note: saved $out/$name.png"

  kill "$kitty" 2>/dev/null || true
  wait "$kitty" 2>/dev/null || true
  kitty=
done
exit "$status"
