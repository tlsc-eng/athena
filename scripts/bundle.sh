#!/bin/zsh
# Builds dist/Athena.app (release, arm64), signs it ad hoc and zips it for a GitHub release.
set -euo pipefail
cd "${0:a:h}/.."

version=$(cargo metadata --no-deps --format-version 1 \
  | python3 -I -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="athena"))')
app=dist/Athena.app

cargo build --release --locked -p athena -p athena-mux

rm -rf dist
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/athena target/release/athena-mux "$app/Contents/MacOS/"
cp assets/Athena.icns "$app/Contents/Resources/"
sed "s/@VERSION@/$version/g" assets/Info.plist > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null

# Inside out: the helper first, then the bundle that seals it.
codesign --force --sign - --timestamp=none "$app/Contents/MacOS/athena-mux"
codesign --force --sign - --timestamp=none "$app"
codesign --verify --strict --deep "$app"

ditto -c -k --keepParent "$app" "dist/Athena-$version-arm64.zip"
shasum -a 256 "dist/Athena-$version-arm64.zip" | tee "dist/Athena-$version-arm64.zip.sha256"
