#!/usr/bin/env bash
# Publishes files to the rolling "nightly" pre-release of this repository (build.yml, pushes to main only).
#
#   scripts/nightly.sh FILE...             upload FILEs, replacing any with the same name
#   scripts/nightly.sh --finalize FILE...  the same, then set the release body; run once, after every package is up
#
# The release is created on first use (with retries: the five package jobs can race to create it), its tag
# "nightly" is moved to $GITHUB_SHA through the git refs API, and the body becomes
#   commit: $GITHUB_SHA
#   <blank line>
#   Rebuilt on every push to main. Files are replaced in place.
# The in-app updater (src/core/update.rs, channel "nightly") reads that first line to learn which commit the
# files were built from, so --finalize writes it only after all the files are in place.
# A run whose commit is no longer the tip of main publishes nothing (a newer run is on its way).
#
# Needs: gh, GH_TOKEN (contents: write), GITHUB_SHA; GITHUB_REPOSITORY defaults to KyttuXD/Kyttu-AI.
set -euo pipefail

tag=nightly
repo=${GITHUB_REPOSITORY:-KyttuXD/Kyttu-AI}
sha=${GITHUB_SHA:?GITHUB_SHA is not set}
finalize=false
if [[ "${1:-}" == --finalize ]]; then
  finalize=true
  shift
fi
files=("$@")
for f in "${files[@]}"; do
  [[ -f "$f" ]] || { echo "::error::$f does not exist"; exit 1; }
done

# retry N CMD...: runs CMD up to N times, waiting a little longer after each failure.
retry() {
  local tries=$1 n=1
  shift
  until "$@"; do
    [[ $n -lt $tries ]] || return 1
    echo "Attempt $n of $tries failed: $*" >&2
    sleep $((n * 5))
    n=$((n + 1))
  done
}

# Another push to main may have started a newer run; leave the release to it.
tip=$(gh api "repos/$repo/git/ref/heads/main" --jq .object.sha 2> /dev/null || true)
if [[ -n "$tip" && "$tip" != "$sha" ]]; then
  echo "main is at $tip, not $sha: not publishing this older nightly."
  exit 0
fi

body=$(printf 'commit: %s\n\nRebuilt on every push to main. Files are replaced in place.' "$sha")

ensure_release() {
  gh release view "$tag" --repo "$repo" > /dev/null 2>&1 && return 0
  # Created by a parallel job in the meantime is fine too.
  gh release create "$tag" --repo "$repo" --prerelease --target "$sha" --title Nightly \
    --notes "Nightly builds are being published." > /dev/null 2>&1 \
    || gh release view "$tag" --repo "$repo" > /dev/null 2>&1
}
retry 5 ensure_release

# Move the tag to this commit (create it if the release left it missing).
move_tag() {
  gh api --method PATCH "repos/$repo/git/refs/tags/$tag" -f sha="$sha" -F force=true > /dev/null 2>&1 \
    || gh api --method POST "repos/$repo/git/refs" -f ref="refs/tags/$tag" -f sha="$sha" > /dev/null
}
retry 5 move_tag

if [[ ${#files[@]} -gt 0 ]]; then
  retry 3 gh release upload "$tag" "${files[@]}" --repo "$repo" --clobber
fi

if [[ "$finalize" == true ]]; then
  retry 3 gh release edit "$tag" --repo "$repo" --prerelease --latest=false --title Nightly --notes "$body"
fi
echo "Published ${files[*]:-nothing} to the $tag release at $sha."
