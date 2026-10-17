# Anthropic response fixtures

The parser tests in `src/autotag/anthropic.rs` read these files. No test calls the API.

Every file here is synthesized from the documented Messages API shapes (response body, `usage`,
`stop_reason: "refusal"` with `stop_details`, and the error body). `test_connection.json` is
meant to be replaced by a real response: the plan's contract check makes the first "Test
connection" call with the owner's key and keeps its response as the fixture.

**Status (2026-10-16): not captured yet.** No key was available when the provider was built,
and the owner chose to ship without the live check. Until it runs, nothing confirms that the API
accepts these request fields as sent: `output_config.format` (`json_schema`),
`output_config.effort: "low"`, `fallbacks: "default"` with the
`anthropic-beta: server-side-fallback-2026-07-01` header, and `cache_control` on the system
block. They follow the docs cited in the plan's Facts. One live call with an invalid dummy key
reached the API over HTTPS and came back 401, which only shows the key check runs first.

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
