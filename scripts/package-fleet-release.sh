#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 4 ]]; then
  echo "usage: $0 SOURCE_DIR OUTPUT_DIR PLATFORM ARCH" >&2
  exit 2
fi

source_dir=$(cd "$1" && pwd)
output_dir=$2
platform=$3
arch=$4
mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)

source_sha=face607b443b3ff35e934434a28f575822392aa8
source_version=0.12.0
node_version=v22.17.1
asset_stem="pty-face607-${platform}-${arch}"
stage_parent=$(mktemp -d)
stage="$stage_parent/$asset_stem"

cleanup() {
  rm -rf "$stage_parent"
}
trap cleanup EXIT

actual_sha=$(git -C "$source_dir" rev-parse HEAD)
if [[ "$actual_sha" != "$source_sha" ]]; then
  echo "source mismatch: expected $source_sha, got $actual_sha" >&2
  exit 1
fi

case "$platform-$arch" in
  linux-x86_64)
    [[ "$(uname -s)" == "Linux" ]]
    [[ "$(uname -m)" == "x86_64" ]]
    export npm_config_build_from_source=true
    export CFLAGS="${CFLAGS:-} -ffile-prefix-map=$source_dir=."
    export CXXFLAGS="${CXXFLAGS:-} -ffile-prefix-map=$source_dir=."
    ;;
  darwin-arm64)
    [[ "$(uname -s)" == "Darwin" ]]
    [[ "$(uname -m)" == "arm64" ]]
    ;;
  *)
    echo "unsupported target: $platform-$arch" >&2
    exit 2
    ;;
esac

cd "$source_dir"
[[ "$(node --version)" == "$node_version" ]]
npm ci
npm run typecheck
npm run build

# npm/build must reproduce tracked face607 sources exactly. The release tooling
# lives on a separate branch and is never copied into the product payload.
git diff --exit-code
[[ -z "$(git status --porcelain --untracked-files=no)" ]]

# Pruning dev dependencies normalizes the stale root version recorded in
# package-lock.json (0.11.0) to package.json's exact 0.12.0. Permit only that
# known packaging-only lockfile mutation; package-lock.json is not shipped.
npm prune --omit=dev --ignore-scripts
[[ "$(git diff --name-only)" == "package-lock.json" ]]

if [[ "$platform" == "linux" ]]; then
  native_dir="$source_dir/node_modules/node-pty/build/Release"
  [[ -f "$native_dir/pty.node" ]]
  [[ -x "$native_dir/spawn-helper" ]]
  strip --strip-unneeded "$native_dir/pty.node" "$native_dir/spawn-helper"

  # manylinux_2_28 is the build container and this gate makes the compatibility
  # ceiling explicit for both the bundled runtime and node-pty native pieces.
  max_glibc=$(
    {
      readelf --version-info "$(command -v node)"
      readelf --version-info "$native_dir/pty.node"
      readelf --version-info "$native_dir/spawn-helper"
    } | grep -oE 'GLIBC_[0-9]+(\.[0-9]+)*' | sort -Vu | tail -1
  )
  if [[ "$(printf '%s\n' "$max_glibc" GLIBC_2.28 | sort -V | tail -1)" != "GLIBC_2.28" ]]; then
    echo "glibc ceiling exceeded: $max_glibc" >&2
    exit 1
  fi

  # node-gyp leaves intermediate objects in build/. Keep only the two runtime
  # files so compiler paths and unneeded build material cannot enter the asset.
  runtime_native=$(mktemp -d)
  cp "$native_dir/pty.node" "$native_dir/spawn-helper" "$runtime_native/"
  rm -rf "$source_dir/node_modules/node-pty/build"
  mkdir -p "$native_dir"
  mv "$runtime_native/pty.node" "$runtime_native/spawn-helper" "$native_dir/"
  rmdir "$runtime_native"
else
  native_dir="$source_dir/node_modules/node-pty/prebuilds/darwin-arm64"
  [[ -f "$native_dir/pty.node" ]]
  [[ -x "$native_dir/spawn-helper" ]]
  file "$(command -v node)" "$native_dir/pty.node" "$native_dir/spawn-helper" |
    grep -q "arm64"
fi

mkdir -p \
  "$stage/bin" \
  "$stage/lib/pty" \
  "$stage/share/pty/completions"

cp "$(command -v node)" "$stage/bin/node"
cp "$GITHUB_WORKSPACE/release-tools/scripts/fleet-pty-launcher" "$stage/bin/pty"
cp "$source_dir/bin/pty" "$stage/lib/pty/bin-pty"
mkdir -p "$stage/lib/pty/bin"
mv "$stage/lib/pty/bin-pty" "$stage/lib/pty/bin/pty"
cp -R "$source_dir/dist" "$stage/lib/pty/dist"
cp -R "$source_dir/node_modules" "$stage/lib/pty/node_modules"
cp "$source_dir/package.json" "$source_dir/LICENSE" "$stage/lib/pty/"
cp -R "$source_dir/completions/." "$stage/share/pty/completions/"

# Keep only the current platform's node-pty native payload. Runtime JS resolves
# build/Release first on Linux and prebuilds/<platform>-<arch> on Darwin.
if [[ "$platform" == "linux" ]]; then
  rm -rf "$stage/lib/pty/node_modules/node-pty/prebuilds"
else
  find "$stage/lib/pty/node_modules/node-pty/prebuilds" \
    -mindepth 1 -maxdepth 1 ! -name darwin-arm64 -exec rm -rf {} +
fi
rm -rf \
  "$stage/lib/pty/node_modules/node-pty/src" \
  "$stage/lib/pty/node_modules/node-pty/deps" \
  "$stage/lib/pty/node_modules/node-pty/scripts" \
  "$stage/lib/pty/node_modules/node-pty/third_party"

chmod 0755 "$stage/bin/node" "$stage/bin/pty" "$stage/lib/pty/bin/pty"
find "$stage/lib/pty/node_modules/node-pty" -name spawn-helper -exec chmod 0755 {} +
if [[ "$platform" == "darwin" ]]; then
  xattr -cr "$stage"
fi

release_platform=$platform \
release_arch=$arch \
release_node_version=$node_version \
release_source_sha=$source_sha \
release_source_version=$source_version \
release_manifest="$stage/RELEASE.json" \
  "$stage/bin/node" --input-type=module -e '
    import fs from "node:fs";
    fs.writeFileSync(process.env.release_manifest, JSON.stringify({
      schemaVersion: 1,
      product: "compoundingtech/pty",
      sourceSha: process.env.release_source_sha,
      sourceVersion: process.env.release_source_version,
      nodeVersion: process.env.release_node_version,
      platform: process.env.release_platform,
      arch: process.env.release_arch
    }, null, 2) + "\n");
  '

# Shipped artifacts must not capture the CI checkout path.
if grep -R -a -F "$GITHUB_WORKSPACE" "$stage" >/dev/null; then
  echo "artifact contains CI workspace path" >&2
  exit 1
fi

archive="$output_dir/$asset_stem.tar.gz"
tar -C "$stage_parent" -czf "$archive" "$asset_stem"

extract_dir=$(mktemp -d)
trap 'rm -rf "$extract_dir" "$stage_parent"' EXIT
tar -C "$extract_dir" -xzf "$archive"
installed="$extract_dir/$asset_stem"

[[ "$("$installed/bin/node" --version)" == "$node_version" ]]
[[ "$("$installed/bin/pty" --version)" == "$source_version" ]]
"$installed/bin/node" -e '
  const fs = require("node:fs");
  const manifest = JSON.parse(fs.readFileSync(process.argv[1], "utf8"));
  if (manifest.sourceSha !== "face607b443b3ff35e934434a28f575822392aa8") process.exit(1);
' "$installed/RELEASE.json"

smoke_root=$(mktemp -d)
trap 'rm -rf "$smoke_root" "$extract_dir" "$stage_parent"' EXIT
PTY_ROOT="$smoke_root" "$installed/bin/pty" list --json |
  "$installed/bin/node" -e '
    let data = "";
    process.stdin.on("data", chunk => data += chunk);
    process.stdin.on("end", () => {
      const parsed = JSON.parse(data);
      if (!Array.isArray(parsed) || parsed.length !== 0) process.exit(1);
    });
  '
PTY_ROOT="$smoke_root" "$installed/bin/pty" run -d --id release-smoke -- cat
PTY_ROOT="$smoke_root" "$installed/bin/pty" list --json |
  "$installed/bin/node" -e '
    let data = "";
    process.stdin.on("data", chunk => data += chunk);
    process.stdin.on("end", () => {
      const parsed = JSON.parse(data);
      if (!parsed.some(item => item.name === "release-smoke" && item.status === "running")) {
        process.exit(1);
      }
    });
  '
PTY_ROOT="$smoke_root" "$installed/bin/pty" kill release-smoke
PTY_ROOT="$smoke_root" "$installed/bin/pty" rm release-smoke

launcher_sha=$(
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$installed/bin/pty" | awk '{print $1}'
  else
    shasum -a 256 "$installed/bin/pty" | awk '{print $1}'
  fi
)
printf '%s  %s/bin/pty\n' "$launcher_sha" "$asset_stem" \
  > "$output_dir/$asset_stem.launcher.sha256"
