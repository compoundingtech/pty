#!/bin/sh
# Package an installed libghostty-vt static archive so a project can depend
# on pty-terminal or pty-testkit without Zig.
#
#   scripts/package-libghostty-vt.sh <target-triple> <native-install> <license-file> <dist>
#
# <native-install> contains lib/, include/, and share/pkgconfig/. It can be
# the Nix-native package or libghostty-vt-sys's ghostty-install directory.
#
# Writes <dist>/libghostty-vt-<target-triple>.tar.gz and its .sha256. The
# archive holds the static library, the headers, Ghostty's license, a SOURCE
# note, and share/pkgconfig/libghostty-vt-static.pc. The pkg-config file's
# prefix is `${pcfiledir}/../..`, so it is right wherever the archive is
# unpacked. libghostty-vt-sys looks for exactly that pkg-config name when it
# links statically.
#
# POSIX sh on purpose: the Linux release job runs `sh`, not bash.
set -eu

if [ $# -ne 4 ]; then
  echo "usage: $0 <target-triple> <native-install> <license-file> <dist>" >&2
  exit 2
fi
triple=$1
install=$2
license=$3
dist=$4

pc="$install/share/pkgconfig/libghostty-vt-static.pc"
for f in "$install/lib/libghostty-vt.a" "$install/include/ghostty/vt.h" "$pc" "$license"; do
  [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
done

# The checked owner contract pins the same commit as the locked Rust bindings.
commit=$(sed -n 's/^ *"ghosttyRev": "\([0-9a-f]*\)",*/\1/p' libghostty-vt-contract.json)
sys_version=$(awk '/^name = "libghostty-vt-sys"$/ { getline; gsub(/[^0-9.]/, ""); print; exit }' Cargo.lock)
[ -n "$commit" ] && [ -n "$sys_version" ] || { echo "could not tell which Ghostty this is" >&2; exit 1; }

name="libghostty-vt-$triple"
work=$(mktemp -d)
stage="$work/$name"
mkdir -p "$stage/lib" "$stage/share/pkgconfig"
cp "$install/lib/libghostty-vt.a" "$stage/lib/"
cp -R "$install/include" "$stage/"
# Nix output directories are read-only; the staging copy must be removable.
chmod -R u+w "$stage/include"
cp "$license" "$stage/LICENSE-ghostty"
# shellcheck disable=SC2016 # ${pcfiledir} is for pkg-config, not the shell.
sed 's|^prefix=.*|prefix=${pcfiledir}/../..|' "$pc" > "$stage/share/pkgconfig/libghostty-vt-static.pc"
# A Mac links this later with its own Xcode linker; say what it was built for.
built_for=
if command -v otool >/dev/null 2>&1; then
  built_for=$(otool -l "$stage/lib/libghostty-vt.a" \
    | awk '/LC_BUILD_VERSION/ { v = 1 } v && $1 == "minos" { m = $2 } v && $1 == "sdk" { print "macOS " m " and later (SDK " $2 ")"; exit }')
fi
cat > "$stage/SOURCE" <<EOF
libghostty-vt, the static library, for $triple${built_for:+, built for $built_for}.
Built from Ghostty commit $commit for pty ${GITHUB_SHA:-$(git rev-parse HEAD)}.
Compatible with libghostty-vt-sys $sys_version.

Unpack anywhere and point PKG_CONFIG_PATH at share/pkgconfig. With the
libghostty-vt-sys pkg-config feature on (pty-terminal turns it on), cargo
links this archive instead of building Ghostty with Zig. Use the archive from
the same pty release as the pty-terminal or pty-testkit you depend on.
EOF

mkdir -p "$dist"
# macOS tar would otherwise add AppleDouble `._*` entries for extended
# attributes; everywhere else this is ignored.
COPYFILE_DISABLE=1 tar -C "$work" -czf "$dist/$name.tar.gz" "$name"
if command -v sha256sum >/dev/null 2>&1; then
  (cd "$dist" && sha256sum "$name.tar.gz" > "$name.tar.gz.sha256")
else
  (cd "$dist" && shasum -a 256 "$name.tar.gz" > "$name.tar.gz.sha256")
fi
rm -rf "$work"
echo "packaged $dist/$name.tar.gz"
tar -tzf "$dist/$name.tar.gz"
