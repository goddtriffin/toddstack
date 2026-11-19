#!/usr/bin/env bash
# Publishes every library whose version isn't on its registry yet, then tags and releases it on GitHub.
# Idempotent: a version already published is skipped, so re-running (or CI on an unrelated push) changes nothing.
#
#   scripts/publish.sh          publish (CI does this on main, with trusted publishing: no tokens)
#   scripts/publish.sh check    for PRs: fail if a library's files changed but its version is already published
#
# Libraries (registry, name, directory, version source, git tag prefix or - for none):
set -euo pipefail
cd "$(dirname "$0")/.."
MODE="${1:-publish}"
LIBS=(
  "crate webserver-base     rust/webserver_base       rust/Cargo.toml                  webserver-base-v"
  "crate flyball            rust/flyball              rust/flyball/Cargo.toml          flyball-v"
  "crate toddstack-accounts-server-sdk rust/toddstack_accounts_server_sdk rust/toddstack_accounts_server_sdk/Cargo.toml toddstack-accounts-server-sdk-v"
  "crate toddstack-frontend-engine rust/toddstack_frontend_engine rust/toddstack_frontend_engine/Cargo.toml toddstack-frontend-engine-v"
  "crate shakedown rust/shakedown rust/shakedown/Cargo.toml shakedown-v"
  "jsr   @todd/webserver-base typescript/webserver-base typescript/deno.jsonc        -"
  "jsr   @todd/toddstack-accounts typescript/accounts  typescript/accounts/deno.jsonc toddstack-accounts-v"
  "npm   toddstack-accounts typescript/accounts       typescript/accounts/package.json toddstack-accounts-v"
)
UA="toddstack-publish (github.com/goddtriffin/toddstack)"

version_of() { # first `version = "x"` (TOML) or "version": "x" (JSON) in the file
  sed -nE 's/^version = "([^"]+)"/\1/p; s/^ *"version": "([^"]+)".*/\1/p' "$1" | head -1
}
published() { # registry name version -> exit 0 if that version is on the registry
  case $1 in
    crate) curl -fsS -A "$UA" "https://crates.io/api/v1/crates/$2/$3" >/dev/null 2>&1 ;;
    jsr) curl -fsS "https://jsr.io/$2/meta.json" | grep -q "\"$3\"" ;;
    npm) [[ -n $(npm view "$2@$3" version 2>/dev/null) ]] ;;
  esac
}
publish() { # registry name dir
  case $1 in
    crate) (cd rust && cargo publish --locked --package "$2") ;;
    jsr) (cd "$(dirname "$4")" && deno publish) ;;
    npm) (cd "$3" && rm -rf dist && npx -y -p typescript@5.9.3 tsc -p tsconfig.npm.json && npm publish --access public) ;;
  esac
}

failed=0
for lib in "${LIBS[@]}"; do
  read -r registry name dir manifest tag <<<"$lib"
  version=$(version_of "$manifest")
  if [[ $MODE == check ]]; then
    base=$(git merge-base HEAD origin/main)
    if ! git diff --quiet "$base" HEAD -- "$dir" && published "$registry" "$name" "$version"; then
      echo "::error::$dir changed but $name $version is already on $registry: bump the version in $manifest"
      failed=1
    fi
    continue
  fi
  if published "$registry" "$name" "$version"; then
    echo "$registry $name $version: already published"
  else
    echo "$registry $name $version: publishing"
    publish "$registry" "$name" "$dir" "$manifest"
  fi
  # Tags follow the Rust version; a TS-only package (`-`) gets none.
  if [[ $tag != - ]] && ! git ls-remote --exit-code --tags origin "refs/tags/$tag$version" >/dev/null; then
    gh release create "$tag$version" --target "$(git rev-parse HEAD)" --latest=false \
      --title "$name $version" --generate-notes
  fi
done
exit $failed
