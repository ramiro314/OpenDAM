# `claude` CLI fixtures

The Claude-plan tests in `src/autotag/claude_cli.rs` replay these files through a stub `claude`
script on a test `PATH`. No test runs the real CLI.

Each `.jsonl` file is the stdout of one headless run, one JSON line per event. Each `auth_*.json`
file is the output of `claude auth status --json`.

## Recorded (claude 2.1.296, 2026-10-16)

These come from the installed CLI, run from an environment holding only `HOME`, `PATH`, `USER`
and `LANG`, with the production flags:

```sh
claude -p --input-format stream-json --output-format stream-json --verbose \
  --json-schema <schema> --model <model> --tools "" --safe-mode \
  --no-session-persistence --system-prompt-file system.txt
```

| File | Probe |
|---|---|
| `text.jsonl` | Haiku 5.5, text only. Answered `{"existing":["t3","t5"]}`. Its `rate_limit_event` is a real `allowed_warning` (76% of the seven-day window) |
| `image.jsonl` | Haiku 5.5, a base64 PNG `image` block (a red square with a white circle) plus text. Answered `t3`, `t5` (Color > Red, Shape > Circle) |
| `model_not_found.jsonl` | An unknown model. Assistant `error: "model_not_found"`; the result is `subtype: "success"` with `is_error: true` and `api_error_status: 404`; exit 1 |
| `inherited_api_key.jsonl` | `ANTHROPIC_API_KEY` set to an invalid key. `init` reports `apiKeySource: "ANTHROPIC_API_KEY"`, then `authentication_failed` (401); exit 1 |
| `logged_out.jsonl` | `CLAUDE_CONFIG_DIR` pointed at an empty directory. `authentication_failed`, "Not logged in · Please run /login", no `api_error_status`; exit 1 |
| `auth_logged_in.json` | `claude auth status --json` from the cleaned environment: `authMethod: "claude.ai"` |
| `auth_logged_out.json` | The same with the empty `CLAUDE_CONFIG_DIR`: `loggedIn: false`, `authMethod: "none"` |
| `auth_api_key.json` | The same with `ANTHROPIC_API_KEY` set: `authMethod: "api_key"`, `apiKeySource: "ANTHROPIC_API_KEY"` |

Scrubbed before committing: the `init` line's `cwd`, socket path and personal lists
(`slash_commands`, `skills`, `agents`, `plugins`), and the auth status's email, organization and
directories.

Two facts the probes added to the plan's:
- **`init` comes after the first user message.** With stdin open and nothing written, the CLI
  printed nothing for 15 s. So the billing guard can't hold the message back until `init` is
  checked. The cleaned environment is what keeps an API key away from the child; the `init`
  check catches a key that reaches it anyway, and pauses the run.
- **`--system-prompt-file` works** with `-p` and stream-json, though `--help` names it only
  under `--bare`.

## Synthesized

Built from the recorded shapes (`text.jsonl`'s `init` and `rate_limit_event`,
`model_not_found.jsonl`'s error lines) and the documented types in the
[Agent SDK TypeScript reference](https://code.claude.com/docs/en/agent-sdk/typescript):

| File | What it stands for |
|---|---|
| `rate_limit_rejected.jsonl` | `rate_limit_info.status: "rejected"` with `resetsAt`, then `rate_limit` (429) |
| `rate_limit_credits_required.jsonl` | `rejected` with `errorCode: "credits_required"` (subscription usage exhausted) and no `resetsAt` |
| `text_allowed.jsonl` | `text.jsonl` with a plain `allowed` rate-limit event |
| `rate_limit.jsonl` | A `rate_limit` (429) error with no plan-limit event |
| `overloaded.jsonl` | `overloaded` (529) |
| `authentication_failed.jsonl` | `authentication_failed` (401) with a revoked login |
| `error_max_structured_output_retries.jsonl` | The error subtype, with `errors: [...]` and no `structured_output` |
