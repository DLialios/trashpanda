#!/usr/bin/env bash
# Build, sign and publish the GitHub release of the version in Cargo.toml.
#
# To release:
#   1. Raise `version` in Cargo.toml, commit, and push the commit.
#   2. Run ./release.sh. minisign asks for the key's password once per build.
#
# Needs cargo with the x86_64-pc-windows-gnu target, mingw-w64-gcc, minisign,
# jq, and gh logged in. The secret key is ~/.minisign/trashpanda.key, or
# $MINISIGN_KEY. Every build carries minisign.pub and refuses updates signed
# with any other key, so if the secret key is lost, a build with a new key has
# to be handed out by hand.
#
# The release is created as a draft holding all its files and only then
# published, so releases/latest never points at a release with files missing.
# If the script stops after creating the draft, delete the draft on GitHub
# before running it again.

set -euo pipefail
cd "$(dirname "$0")"

repo=DLialios/trashpanda
key=${MINISIGN_KEY:-$HOME/.minisign/trashpanda.key}
windows=x86_64-pc-windows-gnu

die() {
    echo "release.sh: $*" >&2
    exit 1
}

for tool in cargo gh jq minisign x86_64-w64-mingw32-gcc; do
    command -v "$tool" >/dev/null || die "$tool is not installed"
done
[[ -f $key ]] || die "no signing key at $key (set MINISIGN_KEY)"

version=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[0].version')
tag=v$version
commit=$(git rev-parse HEAD)

[[ -z $(git status --porcelain) ]] || die "commit or stash your changes first"
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && die "tag $tag already exists; raise the version"
git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null && die "tag $tag already exists on GitHub"
gh release view "$tag" -R "$repo" >/dev/null 2>&1 && die "release $tag already exists, perhaps as a draft"
git fetch -q origin
[[ -n $(git branch -r --contains "$commit") ]] || die "push $commit to GitHub first"

cargo test --locked --quiet
cargo build --locked --release
cargo build --locked --release --target "$windows"

out=target/release-$tag
rm -rf "$out"
mkdir -p "$out"
cp target/release/trashpanda "$out/trashpanda-linux-x86_64"
cp "target/$windows/release/trashpanda.exe" "$out/trashpanda-windows-x86_64.exe"

# The trusted comment is signed too; the program checks it names this version
# and the platform it runs on.
sign() {
    minisign -S -s "$key" -m "$out/$1" -t "trashpanda $version $2"
    minisign -V -q -p minisign.pub -m "$out/$1" || die "$key does not match minisign.pub"
}
sign trashpanda-linux-x86_64 linux-x86_64
sign trashpanda-windows-x86_64.exe windows-x86_64

jq -n --arg version "$version" '{version: $version, assets: {
    "linux-x86_64": "trashpanda-linux-x86_64",
    "windows-x86_64": "trashpanda-windows-x86_64.exe"}}' >"$out/latest.json"

gh release create "$tag" -R "$repo" --draft --target "$commit" --title "trashpanda $version" \
    --generate-notes "$out"/*
gh release edit "$tag" -R "$repo" --draft=false --latest
git fetch -q --tags origin
echo "Published https://github.com/$repo/releases/tag/$tag"
