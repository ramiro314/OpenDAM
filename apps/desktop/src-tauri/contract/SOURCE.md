# Asset contract schema

`asset-contract.schema.json` is a byte copy of the Dungeon Game asset contract's JSON Schema.
Don't edit it. Change the contract in dungeon-game first, then re-copy it with
`scripts/sync-asset-contract.sh <dungeon-game checkout> <commit>`, which also rewrites this file.
`export::contract`'s tests fail when the copy's sha256 differs from the one below, or when the
schema's `contractVersion` differs from `CONTRACT_VERSION`.

- Repository: `ramiro314/dungeon-game`
- Path: `docs/asset-contract.schema.json`
- Commit: `05279adae1cf2151af12a3ef307f27eac695b1ad`
- sha256: `1932dcb4a9aa922dc881824d36ea60e60cf066ed1b0c6acc653098543c1c9b8d`
