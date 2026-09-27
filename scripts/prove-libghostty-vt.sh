#!/bin/sh
# Prove a libghostty-vt archive from package-libghostty-vt.sh is usable: build
# and test pty-terminal and pty-testkit against it with no Zig anywhere.
#
#   scripts/prove-libghostty-vt.sh <libghostty-vt-<triple>.tar.gz>
#
# Every PATH entry that holds a `zig` is dropped and the Ghostty source
# overrides are cleared, so libghostty-vt-sys cannot build Ghostty itself;
# the only way the build succeeds is by finding the archive through
# pkg-config. The build uses its own target directory, so nothing a Zig build
# left behind can stand in for the archive.
#
# POSIX sh on purpose: the Linux release job runs `sh`, not bash.
set -eu

if [ $# -ne 1 ]; then
  echo "usage: $0 <libghostty-vt-<triple>.tar.gz>" >&2
  exit 2
fi
archive=$1
work=$(mktemp -d)
tar -xzf "$archive" -C "$work"
set -- "$work"/libghostty-vt-*
[ $# -eq 1 ] && [ -d "$1" ] || { echo "the archive holds no single libghostty-vt-* directory" >&2; exit 1; }
lib=$1

no_zig=
old_ifs=$IFS
IFS=:
for dir in $PATH; do
  if [ -n "$dir" ] && [ ! -e "$dir/zig" ]; then
    no_zig="${no_zig:+$no_zig:}$dir"
  fi
done
IFS=$old_ifs
PATH=$no_zig
export PATH
unset GHOSTTY_SOURCE_DIR GHOSTTY_ZIG_SYSTEM_DIR
if command -v zig >/dev/null 2>&1; then
  echo "FAIL: zig is still reachable at $(command -v zig)" >&2
  exit 1
fi
echo "no zig on PATH"

PKG_CONFIG_PATH="$lib/share/pkgconfig"
export PKG_CONFIG_PATH
# pkg-config also searches the system's own directories. Only a resolution
# into this archive proves anything about this archive.
libs=$(pkg-config --static --libs libghostty-vt-static)
echo "pkg-config --static --libs libghostty-vt-static: $libs"
case "$libs" in
  *"$lib/"*) ;;
  *)
    echo "FAIL: pkg-config resolved libghostty-vt-static outside the archive" >&2
    exit 1
    ;;
esac

CARGO_TARGET_DIR="$work/target"
export CARGO_TARGET_DIR
cargo test -p pty-terminal
# Real processes in real PTYs, read back through the linked library. Not
# terminal_spawn: three of its tests wait for bash's `$` prompt, and the
# Linux release job runs as root, where the prompt is `#`.
cargo test -p pty-testkit --test terminal_fidelity --test terminal_queries
rm -rf "$work"
echo "pty-terminal and pty-testkit built and passed against $archive with no Zig"
