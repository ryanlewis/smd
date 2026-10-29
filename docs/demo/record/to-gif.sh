#!/bin/zsh -f
# Turns a macOS screen recording of the demo window into docs/images/demo.gif.
# Usage: docs/demo/record/to-gif.sh VIDEO [options]
#   --start SEC    drop the recording before this time (seconds or mm:ss)
#   --end SEC      drop the recording after this time
#   --margin PT    desktop kept around the window, in points (default 48)
#   --crop W:H:X:Y crop in video pixels, for a recording that is not of the
#                  whole display (default: worked out from open.sh's window.txt)
#   --max-mb MB    largest GIF size (default 3)
#   --out FILE     where to write the GIF (default docs/images/demo.gif)
# See docs/demo/README.md.
set -e
setopt pipefail
zmodload zsh/mathfunc

here=${0:a:h}
repo=${here:h:h:h}
dir=${TMPDIR:-/tmp}/smd-demo

video=$1
[[ -f $video ]] || { echo "usage: $0 VIDEO [--start SEC] [--end SEC] [--margin PT] [--crop W:H:X:Y] [--max-mb MB] [--out FILE]" >&2; exit 2; }
shift
start= end= margin=48 crop= max_mb=3 out=$repo/docs/images/demo.gif
while (( $# )); do
  case $1 in
    --start) start=$2 ;;
    --end) end=$2 ;;
    --margin) margin=$2 ;;
    --crop) crop=$2 ;;
    --max-mb) max_mb=$2 ;;
    --out) out=$2 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift 2
done

read vw vh < <(ffprobe -v error -select_streams v:0 -show_entries stream=width,height -of csv=p=0:s=x $video | tr x ' ')

if [[ -z $crop ]]; then
  [[ -f $dir/window.txt ]] || { echo "no $dir/window.txt: open the window with open.sh, or pass --crop" >&2; exit 1; }
  read _ wx wy ww wh < <(grep '^window' $dir/window.txt)
  read _ sx sy sw sh < <(grep '^screen' $dir/window.txt)
  # A whole-display recording is the display's size times its scale (2 on
  # Retina). Anything else is a part of the screen, so the window's place in
  # it is unknown.
  (( scale = vw / (sw + 0.0) ))
  if (( vh < sh * scale - 4 || vh > sh * scale + 4 )); then
    echo "the recording (${vw}x$vh) is not of the whole display the window is on (${sw}x$sh points); pass --crop W:H:X:Y" >&2
    exit 1
  fi
  # The window plus the margin, kept inside the frame, in even pixels.
  x0=$(( int((wx - sx - margin) * scale) & ~1 )); (( x0 < 0 )) && x0=0
  y0=$(( int((wy - sy - margin) * scale) & ~1 )); (( y0 < 0 )) && y0=0
  x1=$(( int((wx - sx + ww + margin) * scale) & ~1 )); (( x1 > vw )) && x1=$(( vw & ~1 ))
  y1=$(( int((wy - sy + wh + margin) * scale) & ~1 )); (( y1 > vh )) && y1=$(( vh & ~1 ))
  crop=$(( x1 - x0 )):$(( y1 - y0 )):$x0:$y0
fi
echo "crop $crop of ${vw}x$vh"
cw=${crop%%:*}

trim=()
[[ -n $start ]] && trim+=(-ss $start)
[[ -n $end ]] && trim+=(-to $end)

# Lower frame rates, then fewer colours without dithering, then smaller sizes,
# until the GIF fits: split panes need the width to stay readable.
max=$(( max_mb * 1000000 ))
tmp=$(mktemp -t smd-demo).gif
for width fps colours in 1000 12 256  1000 10 256  1000 8 256  900 8 256  900 8 128  800 8 128  700 8 128  640 8 128; do
  (( width > cw )) && width=$cw
  dither=bayer:bayer_scale=5
  (( colours < 256 )) && dither=none
  ffmpeg -loglevel error -y $trim -i $video -filter_complex \
    "fps=$fps,crop=$crop,scale=$width:-2:flags=lanczos,split[a][b];[a]palettegen=max_colors=${colours}:stats_mode=diff[p];[b][p]paletteuse=dither=${dither}:diff_mode=rectangle" \
    $tmp
  size=$(stat -f %z $tmp)
  echo "${width}px wide at $fps fps, $colours colours: $size bytes"
  (( size <= max )) && break
done
mv $tmp $out
(( size <= max )) || echo "warning: still over $max_mb MB; trim the recording with --start and --end" >&2
echo "wrote $out ($size bytes, $(ffprobe -v error -show_entries format=duration -of csv=p=0 $out) s)"
