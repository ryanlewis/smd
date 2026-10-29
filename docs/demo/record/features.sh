#!/bin/zsh -f
# Takes one still per feature for the README's Screenshots section, each from
# a real Ghostty window sized to its note in docs/demo/features/.
# Usage: docs/demo/record/features.sh [feature ...]   (default: all of them)
# See docs/demo/README.md.
set -e
setopt pipefail

here=${0:a:h}
repo=${here:h:h:h}
features=$repo/docs/demo/features
images=$repo/docs/images
smd=$repo/target/release/smd
work=$(mktemp -d)

# On any exit, close the Ghostty this run opened (found by its config path,
# which no other process uses). zsh skips the EXIT trap when set -e stops the
# script inside a function, so a failed command also cleans up through ZERR.
cleanup() {
  # Subshells inherit the ZERR trap; only the script itself cleans up.
  if (( ZSH_SUBSHELL )); then return; fi
  pkill -f "config-file=$work/ghostty.conf" 2>/dev/null || true
  rm -rf $work
}
trap cleanup EXIT
trap 'cleanup; exit 1' ZERR

[[ -x $smd ]] || { echo "build smd first: cargo build --release" >&2; exit 1; }
mkdir -p $images

swiftc -O -o $work/winid $here/winid.swift

# A clean config: no user Ghostty or smd settings, and smd as the window's
# only process, so no prompt, hostname or path shows. run.sh opens the note
# named in $work/note; open_window writes the config for each shot's size.
mkdir -p $work/xdg
cat > $work/run.sh <<EOF
#!/bin/zsh -f
export XDG_CONFIG_HOME=$work/xdg
note=\$(<$work/note)
cd ${(q)features}/\${note:h}
exec ${(q)smd} \${note:t}
EOF
chmod +x $work/run.sh
conf=$work/ghostty.conf

# Sends key events to this script's Ghostty, by pid, as record.sh does.
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

# Opens note $1 (relative to docs/demo/features) in a window of $2 columns
# and $3 rows, and sets pid and id.
open_window() {
  print -r -- $1 > $work/note
  sed -e "s|@COMMAND@|$work/run.sh|" -e "s|^window-width = .*|window-width = $2|" \
    -e "s|^window-height = .*|window-height = $3|" $here/ghostty.conf > $conf
  open -na /Applications/Ghostty.app --args --config-default-files=false --config-file=$conf
  sleep 3
  pid=$(pgrep -f "config-file=$conf" | head -1 || true)
  [[ -n $pid ]] || { echo "Ghostty did not start" >&2; exit 1; }
  read id rest <<< "$($work/winid $pid | head -1)"
  [[ -n $id ]] || { echo "no Ghostty window found" >&2; exit 1; }
  # Mermaid and maths take a moment to draw.
  sleep 3
}

# Quits smd and waits for this Ghostty instance to exit, so the next
# open_window cannot find it. Ghostty does not always quit when smd does, so
# after a few seconds it is stopped by its pid.
close_window() {
  keys 'keystroke "q"'
  for i in {1..20}; do
    kill -0 $pid 2>/dev/null || return 0
    sleep 0.2
  done
  kill $pid 2>/dev/null || true
  for i in {1..50}; do
    kill -0 $pid 2>/dev/null || return 0
    sleep 0.2
  done
  echo "Ghostty $pid did not exit" >&2
  exit 1
}

# Captures the window as docs/images/$1.png, without the title bar and
# rounded corners (pixels on a Retina, 2x, display), in 256 colours. Ghostty
# will not make a window shorter than about five rows, so $2, when given,
# keeps only that many rows: a row is 37 pixels at font size 14, with 18 of
# padding above and below.
shot() {
  local h=ih-78
  [[ -n $2 ]] && h="min(ih-78\,$(( 36 + $2 * 37 )))"
  screencapture -x -o -l $id $work/$1.png
  ffmpeg -loglevel error -y -i $work/$1.png -vf \
    "crop=iw-24:$h:12:66,split[a][b];[a]palettegen=max_colors=256[p];[b][p]paletteuse=dither=none" \
    $images/$1.png
}

# One function per feature: the note, the window size in cells, and any keys.
diagram() { open_window diagram.md 64 6; shot diagram 4; close_window; }
maths() { open_window maths.md 40 4; shot maths 3; close_window; }
headings() { open_window headings.md 46 10; shot headings; close_window; }
code() { open_window code.md 54 8; shot code 8; close_window; }
diff() { open_window diff.md 58 14; shot diff; close_window; }
table() { open_window table.md 80 13; shot table; close_window; }
table-cards() { open_window table.md 44 24; shot table-cards 23; close_window; }
links() {
  open_window links/Station.md 60 3
  keys 'key code 48'; sleep 0.3       # tab: select the Calibration link
  keys 'key code 36'; sleep 1.5       # enter: follow it
  shot links 3
  close_window
}

all=(diagram maths headings code diff table table-cards links)
# Check every name before any shot overwrites its picture.
for f in ${@:-$all}; do
  (( ${all[(Ie)$f]} )) || { echo "unknown feature: $f (one of: $all)" >&2; exit 2; }
done
for f in ${@:-$all}; do
  $f
done
ls -l $images/*.png
