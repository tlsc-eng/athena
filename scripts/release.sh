#!/bin/zsh
# Tags v<version>, waits for the release workflow to publish the zip, then points the tap's cask at it.
set -euo pipefail
cd "${0:a:h}/.."

version=${1:?usage: scripts/release.sh <version>}
tap=${ATHENA_TAP:-../homebrew-athena}

[[ -z $(git status --porcelain) ]] || { echo "release: commit or stash changes first" >&2; exit 1; }
[[ $(git branch --show-current) == main ]] || { echo "release: run from main" >&2; exit 1; }
[[ -d $tap/.git ]] || { echo "release: no tap checkout at $tap (set ATHENA_TAP)" >&2; exit 1; }
cargo_version=$(cargo metadata --no-deps --format-version 1 \
  | python3 -I -c 'import json,sys; print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="athena"))')
[[ $cargo_version == "$version" ]] || { echo "release: Cargo.toml is at $cargo_version" >&2; exit 1; }

git tag -a "v$version" -m "Athena $version"
git push origin main "v$version"

# The run appears a few seconds after the tag lands.
run=""
for _ in {1..30}; do
  run=$(gh run list --workflow release.yml --branch "v$version" --limit 1 --json databaseId -q '.[0].databaseId')
  [[ -n $run ]] && break
  sleep 2
done
[[ -n $run ]] || { echo "release: the release workflow did not start" >&2; exit 1; }
gh run watch "$run" --exit-status

sha=$(gh release download "v$version" --pattern "*.sha256" --output - | awk '{print $1}')
[[ $sha =~ ^[0-9a-f]{64}$ ]] || { echo "release: bad checksum '$sha'" >&2; exit 1; }

mkdir -p "$tap/Casks"
sed -e "s/@VERSION@/$version/" -e "s/@SHA256@/$sha/" packaging/athena.rb.in > "$tap/Casks/athena.rb"
git -C "$tap" add Casks/athena.rb
git -C "$tap" commit -m "athena $version"
git -C "$tap" push
echo "released $version: brew update && brew upgrade --cask athena"
