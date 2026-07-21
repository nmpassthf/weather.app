#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/../.." && pwd)"
cd "$repository_root"

: "${TESTED_COMMIT:?TESTED_COMMIT is required}"
: "${SOURCE_RUN:?SOURCE_RUN is required}"
: "${GITHUB_RUN_ID:?GITHUB_RUN_ID is required}"

sudo apt-get update
sudo apt-get install --yes --no-install-recommends \
  build-essential \
  pkg-config \
  libglib2.0-dev \
  libgtk-3-dev \
  libwebkit2gtk-4.1-dev \
  libxdo-dev \
  libssl-dev \
  libayatana-appindicator3-dev \
  librsvg2-dev \
  patchelf \
  libfuse2

npm install --global bun@1.3.14
(
  cd weather-renderer/gui
  bun install --frozen-lockfile
  bun run tauri build --features desktop --bundles appimage,deb
)

test "$(getconf GNU_LIBC_VERSION)" = "glibc 2.35"
mkdir -p dist/linux
cp target/release/bundle/appimage/*.AppImage dist/linux/
cp target/release/bundle/deb/*.deb dist/linux/

highest_glibc="$(
  readelf --version-info target/release/weather.app \
    | grep -o 'GLIBC_[0-9.]\+' \
    | sort -Vu \
    | tail -1
)"
test -n "$highest_glibc"
test "$(printf '%s\n' "$highest_glibc" GLIBC_2.35 | sort -V | tail -1)" = GLIBC_2.35

jq -n \
  --arg target "linux-x86_64" \
  --arg commit "$TESTED_COMMIT" \
  --arg source_run "$SOURCE_RUN" \
  --arg bundle_run "$GITHUB_RUN_ID" \
  '{target: $target, commit: $commit, source_run: $source_run, bundle_run: $bundle_run}' \
  > dist/linux/manifest.json
(
  cd dist/linux
  find . -type f ! -name SHA256SUMS -exec sha256sum {} + | sort > SHA256SUMS
)
