#!/usr/bin/env bash
# Point Formula/claude-sandbox.rb at a released tag.
#
#   scripts/bump-formula.sh v0.1.0
#
# GitHub generates a tag's source tarball on demand, so the tag has to exist and
# be pushed before its checksum can be computed — which is why this runs after
# tagging rather than before. .github/workflows/release.yml runs it on every
# `v*` tag push and commits the result; run it by hand if you tag without CI.
set -euo pipefail

repo=dpowers/claude_sandbox
root=$(cd "$(dirname "$0")/.." && pwd)
formula=$root/Formula/claude-sandbox.rb

tag=${1-}
if [ -z "$tag" ]; then
    echo "usage: $(basename "$0") <tag>    e.g. $(basename "$0") v0.1.0" >&2
    exit 1
fi
case $tag in
    v*) ;;
    *) tag=v$tag ;;
esac
version=${tag#v}

# A formula whose version disagrees with the binary's own is worse than no
# formula at all, so refuse rather than paper over it.
crate=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -1)
if [ "$crate" != "$version" ]; then
    echo "error: Cargo.toml is at $crate but the tag says $version" >&2
    echo "       bump the version in Cargo.toml (and Cargo.lock) first" >&2
    exit 1
fi

url="https://github.com/$repo/archive/refs/tags/$tag.tar.gz"

if command -v sha256sum >/dev/null 2>&1; then
    sum() { sha256sum | cut -d' ' -f1; }
else
    sum() { shasum -a 256 | cut -d' ' -f1; }
fi

# A tag pushed seconds ago can 404 briefly while GitHub catches up.
sha=
for attempt in 1 2 3 4 5; do
    echo "fetching $url (attempt $attempt)" >&2
    if sha=$(curl -fsSL --retry 2 "$url" | sum) && [ -n "$sha" ]; then
        break
    fi
    sha=
    sleep 5
done
if [ -z "$sha" ]; then
    echo "error: could not download $url" >&2
    echo "       has the tag been pushed? git push origin $tag" >&2
    exit 1
fi

# Only the first url/sha256 pair — the stable spec — is ours to touch; anything
# later (a bottle block, say) belongs to whatever added it.
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
awk -v url="$url" -v sha="$sha" '
    !seen_url && /^  url "/ { print "  url \"" url "\""; seen_url = 1; next }
    !seen_sha && /^  sha256 "/ { print "  sha256 \"" sha "\""; seen_sha = 1; next }
    { print }
' "$formula" >"$tmp"

if ! grep -q "\"$sha\"" "$tmp" || ! grep -q "\"$url\"" "$tmp"; then
    echo "error: could not find the url/sha256 lines in $formula" >&2
    exit 1
fi

cat "$tmp" >"$formula"
echo "$formula -> $tag"
echo "  url    $url"
echo "  sha256 $sha"
