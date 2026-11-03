#!/bin/zsh
# Builds assets/Daedalus.icns from scripts/icon.swift. Run after changing the icon.
set -euo pipefail
cd "${0:a:h}/.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
swift scripts/icon.swift "$work/icon.png"
set_dir="$work/Daedalus.iconset"
mkdir "$set_dir"
for s in 16 32 128 256 512; do
  sips -z $s $s "$work/icon.png" --out "$set_dir/icon_${s}x${s}.png" >/dev/null
  d=$((s * 2))
  sips -z $d $d "$work/icon.png" --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set_dir" -o assets/Daedalus.icns
echo "wrote assets/Daedalus.icns"
