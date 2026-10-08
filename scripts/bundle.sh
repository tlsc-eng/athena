#!/bin/zsh
# Builds dist/Athena.app (release, arm64), signs it ad hoc and zips it for a GitHub release.
set -euo pipefail
cd "${0:a:h}/.."

version=$(cargo metadata --no-deps --format-version 1 \
  | python3 -I -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="athena"))')
stage=dist.new
app=$stage/Athena.app

cargo build --release --locked -p athena -p athena-mux

rm -rf "$stage"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/athena target/release/athena-mux "$app/Contents/MacOS/"
cp assets/Athena.icns "$app/Contents/Resources/"
sed "s/@VERSION@/$version/g" assets/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

# Inside out: the helper first, then the bundle that seals it.
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/athena-mux"
codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict --deep "$app"

ditto -c -k --keepParent "$app" "$stage/Athena-$version-arm64.zip"
(cd "$stage" && shasum -a 256 "Athena-$version-arm64.zip" | tee "Athena-$version-arm64.zip.sha256")

# Swap whole directories: a daemon started from dist/Athena.app must never find its path missing.
rm -rf dist.old
[[ -d dist ]] && mv dist dist.old
mv "$stage" dist
rm -rf dist.old
