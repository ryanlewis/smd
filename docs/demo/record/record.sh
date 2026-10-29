#!/bin/zsh -f
# Records the README media from a real Ghostty window on macOS.
# Usage: docs/demo/record/record.sh [video|stills|all]   (default: all)
# See docs/demo/README.md.
set -e
setopt pipefail

here=${0:a:h}
repo=${here:h:h:h}
demo=$repo/docs/demo
images=$repo/docs/images
smd=$repo/target/release/smd
work=$(mktemp -d)

# On any exit, stop a capture still running and close the Ghostty this run
# opened (found by its config path, which no other process uses). zsh skips
# the EXIT trap when set -e stops the script inside a function, so a failed
# command also cleans up through ZERR.
cleanup() {
  # Subshells inherit the ZERR trap; only the script itself cleans up.
  if (( ZSH_SUBSHELL )); then return; fi
  [[ -n $cap ]] && { kill $cap 2>/dev/null || true; }
  pkill -f "config-file=$work/ghostty.conf" 2>/dev/null || true
  rm -rf $work
}
trap cleanup EXIT
trap 'cleanup; exit 1' ZERR

[[ -x $smd ]] || { echo "build smd first: cargo build --release" >&2; exit 1; }
mkdir -p $images

swiftc -O -o $work/winid $here/winid.swift
swiftc -O -o $work/click $here/click.swift

# A clean config: no user Ghostty or smd settings, a fixed 100x32 window, and
# smd as the window's only process, so no prompt, hostname or path shows.
mkdir -p $work/xdg
cat > $work/run.sh <<EOF
#!/bin/zsh -f
export XDG_CONFIG_HOME=$work/xdg
cd ${(q)demo}
exec ${(q)smd} "Weather station.md"
EOF
chmod +x $work/run.sh
conf=$work/ghostty.conf
sed "s|@COMMAND@|$work/run.sh|" $here/ghostty.conf > $conf

# Sends key events to this script's Ghostty: brings it to the front by pid, not
# by name (so keys never go to another Ghostty, such as the one the script runs
# in), and stops if it does not come to the front.
keys() {
  osascript -e 'tell application "System Events"' \
    -e "set p to first process whose unix id is $pid" \
    -e 'set frontmost of p to true' \
    -e 'repeat 30 times
if frontmost of p then exit repeat
delay 0.1
end repeat' \
    -e 'if not frontmost of p then error "the demo Ghostty is not at the front"' \
    -e "$1" -e 'end tell'
}
down() { keys "repeat $1 times
key code 38
delay ${2:-0.02}
end repeat"; }

# Window content without the title bar and rounded corners, in pixels on a
# Retina (2x) display.
crop=crop=1772:1242:12:66

# Opens a fresh window and sets id x y w h to its window id and bounds.
open_window() {
  open -na /Applications/Ghostty.app --args --config-default-files=false --config-file=$conf
  sleep 3
  pid=$(pgrep -f "config-file=$conf" | head -1)
  read id x y w h rest <<< "$($work/winid $pid | head -1)"
  [[ -n $id ]] || { echo "no Ghostty window found" >&2; exit 1; }
  sleep 2
}

# Quits smd, which closes the window and this Ghostty instance, and waits for
# it to exit so the next open_window cannot find it.
close_window() {
  keys 'keystroke "q"'
  for i in {1..50}; do
    kill -0 $pid 2>/dev/null || return 0
    sleep 0.2
  done
  echo "Ghostty $pid did not exit" >&2
  exit 1
}

video() {
  open_window
  local mov=$work/demo.mov
  screencapture -x -V 30 -R $x,$y,$w,$h $mov &
  cap=$!
  # screencapture takes a few seconds to start; the still lead-in is trimmed.
  sleep 9
  for burst in 1 2 3 4; do
    down 12 0.03
    sleep 1.6
  done
  sleep 0.5
  # Tab selects the first link in view; at the bottom of the note that is
  # the reference station link in the to-do list.
  keys 'key code 48'
  sleep 1.6
  keys 'key code 36'                  # enter: follow it to Calibration
  sleep 3.5
  $work/click $((x + 45)) $((y + 51)) # the toolbar's Back button
  sleep 3
  wait $cap
  cap=
  close_window

  # Start 2 seconds before the first scroll.
  local scroll=$(ffmpeg -hide_banner -i $mov -vf freezedetect=n=0.003:d=1 -map 0:v -f null - 2>&1 |
    grep -o 'freeze_end: [0-9.]*' | head -1 | cut -d' ' -f2 || true)
  if [[ -z $scroll ]]; then
    echo "warning: found no still lead-in to trim; the GIF starts at the beginning of the capture" >&2
    scroll=0
  fi
  local start=$(( scroll > 2 ? scroll - 2 : 0 ))
  ffmpeg -loglevel error -y -ss $start -i $mov -filter_complex \
    "fps=12,$crop,scale=1000:-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=256:stats_mode=diff[p];[b][p]paletteuse=dither=bayer:bayer_scale=5:diff_mode=rectangle" \
    $images/demo.gif
}

stills() {
  open_window
  shot() {
    sleep 1.2
    screencapture -x -o -l $id $work/$1.png
    ffmpeg -loglevel error -y -i $work/$1.png -vf \
      "$crop,split[a][b];[a]palettegen=max_colors=256[p];[b][p]paletteuse=dither=none" \
      $images/$1.png
  }
  shot diagram
  down 16; shot maths
  down 24; shot table
  keys 'keystroke "G"'; sleep 0.3     # bottom
  keys 'key code 48'; sleep 0.3       # tab: first link in view, the reference station
  keys 'key code 36'; sleep 1.5       # enter: Calibration
  keys 'key code 48'                  # tab: select a link there
  shot links
  close_window
}

case ${1:-all} in
  video) video ;;
  stills) stills ;;
  all) stills; video ;;
  *) echo "usage: $0 [video|stills|all]" >&2; exit 2 ;;
esac
ls -l $images
