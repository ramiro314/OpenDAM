#!/usr/bin/env bash
# Re-copies the Dungeon Game asset contract's JSON Schema into the app, byte for byte, and
# rewrites the SOURCE.md beside it with the commit and sha256 (plan game-export, C1). This is the
# only way to change the copy: run it once the contract change has merged in dungeon-game.
#
#   scripts/sync-asset-contract.sh <dungeon-game checkout> <commit>
#
# The commit is read from the checkout's object store, so its working tree may be on any branch.
set -euo pipefail

if (($# != 2)); then
  echo "usage: $0 <dungeon-game checkout> <commit>" >&2
  exit 2
fi
checkout=$1
schema_path=docs/asset-contract.schema.json
dest=$(cd "$(dirname "$0")/.." && pwd)/apps/desktop/src-tauri/contract

commit=$(git -C "$checkout" rev-parse --verify --quiet "$2^{commit}") || {
  echo "sync-asset-contract: $2 is not a commit in $checkout" >&2
  exit 1
}
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
git -C "$checkout" show "$commit:$schema_path" >"$tmp"
sha=$(sha256sum "$tmp" | cut -d' ' -f1)
mv "$tmp" "$dest/asset-contract.schema.json"
chmod 644 "$dest/asset-contract.schema.json"

cat >"$dest/SOURCE.md" <<EOF
# Asset contract schema

\`asset-contract.schema.json\` is a byte copy of the Dungeon Game asset contract's JSON Schema.
Don't edit it. Change the contract in dungeon-game first, then re-copy it with
\`scripts/sync-asset-contract.sh <dungeon-game checkout> <commit>\`, which also rewrites this file.
\`export::contract\`'s tests fail when the copy's sha256 differs from the one below, or when the
schema's \`contractVersion\` differs from \`CONTRACT_VERSION\`.

- Repository: \`ramiro314/dungeon-game\`
- Path: \`$schema_path\`
- Commit: \`$commit\`
- sha256: \`$sha\`
EOF
echo "sync-asset-contract: copied $schema_path at $commit (sha256 $sha)"
