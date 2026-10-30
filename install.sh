#!/bin/sh
# Install smd from a GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/ryanlewis/smd/main/install.sh | sh
#
# Settings, read from the environment:
#   SMD_VERSION      release to install, such as 0.2.0 or v0.2.0 (default: latest)
#   SMD_INSTALL_DIR  folder to put the binary in (default: ~/.local/bin)
#
# Builds exist for macOS on Apple silicon and Linux (glibc 2.30 or later) on
# x86_64 and arm64. The download is checked against the release's
# checksums.txt before anything is installed.

set -eu

REPO=ryanlewis/smd

say() {
	printf 'smd install: %s\n' "$*" >&2
}

fail() {
	say "error: $*"
	exit 1
}

need() {
	command -v "$1" >/dev/null 2>&1 || fail "needs '$1', which was not found"
}

# Print the body of a URL, or save it to a file when a second argument is
# given.
fetch() {
	if command -v curl >/dev/null 2>&1; then
		if [ $# -eq 2 ]; then
			curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$1"
		else
			curl -fsSL --proto '=https' --tlsv1.2 "$1"
		fi
	elif command -v wget >/dev/null 2>&1; then
		if [ $# -eq 2 ]; then
			wget -q --https-only -O "$2" "$1"
		else
			wget -q --https-only -O - "$1"
		fi
	else
		fail "needs curl or wget"
	fi
}

# The tag the releases/latest page redirects to. This avoids the GitHub API
# and its rate limit.
latest_version() {
	url=https://github.com/$REPO/releases/latest
	if command -v curl >/dev/null 2>&1; then
		final=$(curl -fsSLI --proto '=https' --tlsv1.2 -o /dev/null -w '%{url_effective}' "$url")
	else
		final=$(wget -q --https-only --max-redirect=5 -S --spider "$url" 2>&1 |
			sed -n 's/^ *[Ll]ocation: *//p' | tail -n 1 | tr -d '\r')
	fi
	tag=${final##*/}
	case $tag in
	v[0-9]*) printf '%s\n' "${tag#v}" ;;
	*) fail "could not work out the latest release from $url" ;;
	esac
}

sha256_of() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d ' ' -f 1
	elif command -v shasum >/dev/null 2>&1; then
		shasum -a 256 "$1" | cut -d ' ' -f 1
	else
		fail "needs sha256sum or shasum to check the download"
	fi
}

main() {
	need uname
	need tar
	need mktemp
	if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
		fail "needs curl or wget"
	fi

	os=$(uname -s)
	arch=$(uname -m)
	case $os in
	Darwin) os=darwin ;;
	Linux) os=linux ;;
	*) fail "no build for $os; see https://github.com/$REPO#install for other ways to install" ;;
	esac
	case $arch in
	x86_64 | amd64) arch=amd64 ;;
	arm64 | aarch64) arch=arm64 ;;
	*) fail "no build for $arch; see https://github.com/$REPO#install for other ways to install" ;;
	esac
	if [ "$os" = darwin ] && [ "$arch" = amd64 ]; then
		# A shell running under Rosetta reports x86_64 on an Apple silicon Mac.
		if [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
			arch=arm64
		else
			fail "no build for Intel Macs; install with Cargo instead, see https://github.com/$REPO#install"
		fi
	fi
	if [ "$os" = linux ]; then
		if ldd --version 2>&1 | grep -qi musl; then
			fail "the Linux builds need glibc, and this system uses musl; install with Cargo instead, see https://github.com/$REPO#install"
		fi
		# Prints "glibc 2.31", for example.
		glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
		glibc=${glibc#glibc }
		major=${glibc%%.*}
		minor=${glibc#*.}
		minor=${minor%%.*}
		case $major$minor in
		'' | *[!0-9]*) say "could not tell the glibc version; the Linux builds need 2.30 or later" ;;
		*)
			if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 30 ]; }; then
				fail "the Linux builds need glibc 2.30 or later, and this system has $glibc; install with Cargo instead, see https://github.com/$REPO#install"
			fi
			;;
		esac
	fi

	version=${SMD_VERSION:-latest}
	if [ "$version" = latest ]; then
		version=$(latest_version)
	fi
	version=${version#v}
	case $version in
	[0-9]*.[0-9]*.[0-9]*) ;;
	*) fail "SMD_VERSION should be a release number such as 0.2.0, or latest; got '${SMD_VERSION:-$version}'" ;;
	esac

	dir=${SMD_INSTALL_DIR:-$HOME/.local/bin}
	# A trailing slash would stop the PATH check below from matching.
	while [ "$dir" != / ] && [ "${dir%/}" != "$dir" ]; do
		dir=${dir%/}
	done
	archive=smd_${version}_${os}_${arch}.tar.gz
	base=https://github.com/$REPO/releases/download/v$version

	tmp=$(mktemp -d)
	staged=$dir/.smd.$$
	trap 'rm -rf "$tmp" "$staged"' EXIT
	trap 'exit 130' INT
	trap 'exit 143' TERM

	say "downloading smd $version for $os/$arch"
	fetch "$base/$archive" "$tmp/$archive" || fail "could not download $base/$archive"
	fetch "$base/checksums.txt" "$tmp/checksums.txt" || fail "could not download $base/checksums.txt"

	want=$(awk -v f="$archive" '$2 == f { print $1 }' "$tmp/checksums.txt")
	[ -n "$want" ] || fail "$archive is not listed in checksums.txt"
	got=$(sha256_of "$tmp/$archive")
	[ "$want" = "$got" ] || fail "checksum mismatch for $archive (expected $want, got $got)"

	tar -xzf "$tmp/$archive" -C "$tmp" smd
	mkdir -p "$dir"
	# Copy then rename, so a running smd is never left half-written.
	cp "$tmp/smd" "$staged"
	chmod 755 "$staged"
	mv -f "$staged" "$dir/smd"

	say "installed $dir/smd"
	case :${PATH}: in
	*:"$dir":*) ;;
	*) say "$dir is not on your PATH; add it in your shell's startup file" ;;
	esac
}

# Everything runs from main, so a download cut short cannot run half a script.
main "$@"
