# PTY fleet release `face607`

Production source is pinned to commit
`face607b443b3ff35e934434a28f575822392aa8` (`@compoundingtech/pty`
`0.12.0`). The archives are relocatable and include a pinned Node runtime,
the platform-native `node-pty` payload, an executable relative launcher, and
`RELEASE.json`.

Targets:

- `pty-face607-darwin-arm64.tar.gz` — macOS ARM64, macOS 14 minimum.
- `pty-face607-linux-x86_64.tar.gz` — Linux x86_64, GLIBC 2.28 maximum.

`SHA256SUMS` verifies the archives. `LAUNCHER-SHA256SUMS` records the SHA-256
of each extracted `<archive-root>/bin/pty`.

## Atomic install

Choose the matching asset and expected checksum from `SHA256SUMS`, then:

```sh
set -eu

asset=pty-face607-darwin-arm64.tar.gz  # Linux: pty-face607-linux-x86_64.tar.gz
asset_root=${asset%.tar.gz}
release_url=https://github.com/compoundingtech/pty/releases/download/fleet-face607
install_root=${PTY_INSTALL_ROOT:-"$HOME/.local/opt/pty"}
stable_bin=${PTY_STABLE_BIN:-"$HOME/.local/bin/pty"}
version_dir="$install_root/releases/$asset_root"
incoming="$install_root/.incoming-$asset_root-$$"

mkdir -p "$install_root/releases" "$(dirname "$stable_bin")"
curl -fL "$release_url/$asset" -o "$asset"
curl -fL "$release_url/SHA256SUMS" -o SHA256SUMS

# macOS:
grep "  $asset\$" SHA256SUMS | shasum -a 256 -c -
# Linux uses: grep "  $asset\$" SHA256SUMS | sha256sum -c -

test ! -e "$version_dir"
mkdir "$incoming"
tar -C "$incoming" -xzf "$asset"
candidate="$incoming/$asset_root"

test "$("$candidate/bin/pty" --version)" = "0.12.0"
smoke_root=$(mktemp -d)
PTY_ROOT="$smoke_root" "$candidate/bin/pty" list --json
rm -rf "$smoke_root"

mv "$candidate" "$version_dir"
rmdir "$incoming"

previous=
if test -L "$install_root/current"; then
  previous=$(readlink "$install_root/current")
fi
ln -s "$version_dir" "$install_root/.current-$$"
mv -f "$install_root/.current-$$" "$install_root/current"

ln -s "$install_root/current/bin/pty" "$stable_bin.new-$$"
mv -f "$stable_bin.new-$$" "$stable_bin"

hash -r 2>/dev/null || true
command -v pty
pty --version
final_smoke_root=$(mktemp -d)
PTY_ROOT="$final_smoke_root" pty list --json
rm -rf "$final_smoke_root"
printf 'previous target: %s\n' "$previous"
```

The directory containing `stable_bin` must precede any older NVM/global PTY
entry in the supervisor/root `PATH`.

## Rollback

Keep the previous `readlink "$install_root/current"` value from install. To
roll back atomically:

```sh
set -eu
install_root=${PTY_INSTALL_ROOT:-"$HOME/.local/opt/pty"}
previous=/absolute/path/printed/by/install
test -x "$previous/bin/pty"
"$previous/bin/pty" --version
ln -s "$previous" "$install_root/.rollback-$$"
mv -f "$install_root/.rollback-$$" "$install_root/current"
```

Do not delete either version directory until all long-lived roots have been
restarted and the rollback window has closed.
