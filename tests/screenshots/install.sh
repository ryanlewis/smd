#!/usr/bin/env bash
# Installs what capture.sh needs on Debian or Ubuntu, as root: Xvfb,
# ImageMagick, xdotool and the libraries kitty loads from apt, then pinned
# releases of kitty and the DejaVu fonts, each checked against its SHA-256.
#
#   sudo tests/screenshots/install.sh
#
# A new kitty or font version changes the screenshots: update the
# references in the same change (see CONTRIBUTING.md).
set -euo pipefail

kitty_version=0.49.2
kitty_sha256=d573618b911e9c461bd421b96c13c74c7f1cb2f1ac9c327818d4ff84366cf5c6
dejavu_version=2.37
dejavu_sha256=fa9ca4d13871dd122f61258a80d01751d603b4d3ee14095d65453b4e846e17d7

export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -q -y --no-install-recommends \
  ca-certificates curl bzip2 xz-utils \
  xvfb x11-utils xdotool imagemagick \
  libgl1 libgl1-mesa-dri libegl1 libxkbcommon-x11-0 libxcursor1 \
  libxrandr2 libxi6 libxinerama1 libfontconfig1 libdbus-1-3 fontconfig

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fetch() {
  curl -fsSL --retry 3 -o "$work/$2" "$1"
  echo "$3  $work/$2" | sha256sum -c -
}

fetch "https://github.com/kovidgoyal/kitty/releases/download/v$kitty_version/kitty-$kitty_version-x86_64.txz" \
  kitty.txz "$kitty_sha256"
rm -rf /opt/kitty
mkdir -p /opt/kitty
tar -xJf "$work/kitty.txz" -C /opt/kitty
ln -sf /opt/kitty/bin/kitty /usr/local/bin/kitty

# smd reads the DejaVu files from this folder by name; kitty finds DejaVu
# Sans Mono through fontconfig. Both get the pinned files, not the system's.
fetch "https://github.com/dejavu-fonts/dejavu-fonts/releases/download/version_${dejavu_version//./_}/dejavu-fonts-ttf-$dejavu_version.tar.bz2" \
  dejavu.tar.bz2 "$dejavu_sha256"
tar -xjf "$work/dejavu.tar.bz2" -C "$work"
mkdir -p /usr/share/fonts/truetype/dejavu
cp "$work/dejavu-fonts-ttf-$dejavu_version"/ttf/*.ttf /usr/share/fonts/truetype/dejavu/
fc-cache -f >/dev/null

kitty --version
