#!/bin/zsh -f
# Opens the demo note in a new Ghostty window with a clean config, centred on
# the display it opens on, for recording by hand. Builds nothing: run
# `cargo build --release` first. Quit smd with q to close the window.
# Usage: docs/demo/record/open.sh
# It saves the window's place for to-gif.sh. See docs/demo/README.md.
set -e

here=${0:a:h}
repo=${here:h:h:h}
demo=$repo/docs/demo
smd=$repo/target/release/smd
# Kept after this script exits: the window runs run.sh from here, and
# to-gif.sh reads window.txt.
dir=${TMPDIR:-/tmp}/smd-demo
mkdir -p $dir/xdg

[[ -x $smd ]] || { echo "build smd first: cargo build --release" >&2; exit 1; }
swiftc -O -o $dir/screen $here/screen.swift

# A clean config: no user Ghostty or smd settings, a fixed 100x32 window, and
# smd as the window's only process, so no prompt, hostname or path shows.
cat > $dir/run.sh <<EOF
#!/bin/zsh -f
export XDG_CONFIG_HOME=$dir/xdg
cd ${(q)demo}
exec ${(q)smd} "Weather station.md"
EOF
chmod +x $dir/run.sh
conf=$dir/ghostty.conf
sed "s|@COMMAND@|$dir/run.sh|" $here/ghostty.conf > $conf

if pgrep -f "config-file=$conf" >/dev/null; then
  echo "a demo window is already open; quit it with q first" >&2
  exit 1
fi
# A window.txt from an earlier run must not stand in for this one.
rm -f $dir/window.txt
open -na /Applications/Ghostty.app --args --config-default-files=false --config-file=$conf
for i in {1..50}; do
  pid=$(pgrep -f "config-file=$conf" | head -1 || true)
  [[ -n $pid ]] && $dir/screen $pid > $dir/window.txt 2>/dev/null && break
  sleep 0.2
done
[[ -s $dir/window.txt ]] || { echo "no Ghostty window found" >&2; exit 1; }

# Centre the window on its display.
read _ wx wy ww wh < <(grep '^window' $dir/window.txt)
read _ sx sy sw sh < <(grep '^screen' $dir/window.txt)
osascript -e 'tell application "System Events"' \
  -e "set p to first process whose unix id is $pid" \
  -e "set position of window 1 of p to {$(( sx + (sw - ww) / 2 )), $(( sy + (sh - wh) / 2 ))}" \
  -e 'set frontmost of p to true' \
  -e 'end tell'
sleep 0.5
$dir/screen $pid > $dir/window.txt
cat $dir/window.txt
echo "The demo window is open. Saved its place to $dir/window.txt for to-gif.sh."
