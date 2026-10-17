# Anthropic response fixtures

The parser tests in `src/autotag/anthropic.rs` read these files. No test calls the API.

Every file here is synthesized from the documented Messages API shapes (response body, `usage`,
`stop_reason: "refusal"` with `stop_details`, and the error body). `test_connection.json` is
meant to be replaced by a real response: the plan's contract check makes the first "Test
connection" call with the owner's key and keeps its response as the fixture.

To capture it, run a debug build with the key set and a fixture path:

```sh
ANTHROPIC_API_KEY=... \
OPENDAM_ANTHROPIC_FIXTURE="$PWD/apps/desktop/src-tauri/src/autotag/fixtures/anthropic/test_connection.json" \
make dev
```

Then open Settings and press "Test connection". The app writes the response there only after
checking that it doesn't contain the key. Commit it if `cargo nextest run autotag::anthropic`
still passes. If the API rejects a field with a 400, drop that field from `build_request` and
record it in the pull request.
