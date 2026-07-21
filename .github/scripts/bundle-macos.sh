#!/usr/bin/env bash

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repository_root="$(cd -- "$script_dir/../.." && pwd)"
cd "$repository_root"

: "${TESTED_COMMIT:?TESTED_COMMIT is required}"
: "${SOURCE_RUN:?SOURCE_RUN is required}"
: "${GITHUB_RUN_ID:?GITHUB_RUN_ID is required}"
: "${RUST_TARGET:?RUST_TARGET is required}"
: "${MANIFEST_TARGET:?MANIFEST_TARGET is required}"
: "${EXPECTED_ARCHS:?EXPECTED_ARCHS is required}"
: "${EXPECTED_ARCH_COUNT:?EXPECTED_ARCH_COUNT is required}"

npm install --global bun@1.3.14
(
  cd weather-renderer/gui
  bun install --frozen-lockfile
  bun run tauri build --target "$RUST_TARGET" --features desktop --bundles app,dmg
)

bundle_root="target/$RUST_TARGET/release/bundle"
app_path="$(find "$bundle_root/macos" -maxdepth 1 -name '*.app' -print -quit)"
test -n "$app_path"
test -f "$app_path/Contents/MacOS/weather.app"
test ! -e "$app_path/Contents/Resources/resources/bin/weather-daemon"

actual_archs="$(lipo -archs "$app_path/Contents/MacOS/weather.app")"
actual_arch_count="$(wc -w <<< "$actual_archs" | tr -d ' ')"
if [[ "$actual_arch_count" != "$EXPECTED_ARCH_COUNT" ]]; then
  echo "unexpected architectures for $RUST_TARGET: $actual_archs" >&2
  exit 1
fi
for arch in $EXPECTED_ARCHS; do
  case " $actual_archs " in
    *" $arch "*) ;;
    *)
      echo "missing architecture $arch in $actual_archs" >&2
      exit 1
      ;;
  esac
done

mkdir -p dist/macos
ditto -c -k --sequesterRsrc --keepParent "$app_path" dist/macos/Weather.app.zip
cp "$bundle_root"/dmg/*.dmg dist/macos/
jq -n \
  --arg target "$MANIFEST_TARGET" \
  --arg commit "$TESTED_COMMIT" \
  --arg source_run "$SOURCE_RUN" \
  --arg bundle_run "$GITHUB_RUN_ID" \
  '{target: $target, commit: $commit, source_run: $source_run, bundle_run: $bundle_run}' \
  > dist/macos/manifest.json
(
  cd dist/macos
  find . -type f ! -name SHA256SUMS -exec shasum -a 256 {} \; | sort > SHA256SUMS
)
