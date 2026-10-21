# Project Notes

## Notes to Remember
- Keep all project memories in this file (CLAUDE.md).
- Keep responses concise and in simple English.
- The project is written in Rust.
- Follow TDD from now on: write a failing test first, then the code to pass it, then refactor. Everything must be tested; no code change without a test. Run `cargo test` before every commit.

## Overview
`llmgwcli` is a CLI client for LLM gateways. Supported: LiteLLM and fireworks.ai.

## Config (managed by the tool)
- Default dir: `~/.config/llmgwcli` (override with `--config-dir` or `LLMGWCLI_CONFIG_DIR`).
- Dir is mode 0700; all files are 0600.
- `llmgwcli.key`: random 32-char alphanumeric key, created if missing.
- `credentials.toml`: API keys encrypted with the key (ChaCha20-Poly1305, base64 of nonce+ciphertext). Only used by gateways whose credential store is `file`.
- Credential store is per gateway (`credential_store = "keychain" | "file"` in config.toml). Default: `keychain` on macOS, `file` elsewhere. Configs without the field are read as `file`, so older configs keep working.
- Keychain (`src/keychain.rs`): service `llmgwcli`, account = gateway name, through the `security` command. Secrets are written by piping `add-generic-password -U ...` to `security -i` on stdin so they never show up in `ps`. Missing item = exit code 44.
- Test hook: `LLMGWCLI_SECURITY_BIN` replaces the `security` program (tests use `tests/fake_security.sh`; the e2e tests always set it so they never touch the real keychain).
- `cache/<gateway>.json` (dir 0700, files 0600): per-model price responses, reused for 24 hours (`src/cache.rs`). Only prices are cached (LiteLLM `/model/info`, Fireworks `serverlessModels`: used for `list-models` prices and for the display names `usage` needs); usage figures are always live. Entry = `{url (redacted), fetched_at (unix secs), data}`; a different URL, an age of 24h or more, a future timestamp or an unreadable file is a miss, and cache write errors are only logged. `edit-provider` and `remove-provider` delete the gateway's file. Failed fetches and the no-price fallback lists are not cached. File name is the gateway name with all but `[A-Za-z0-9_-]` percent-encoded.
- `config.toml`: all other settings (`[providers.<name>]` with `type` and `base_url`).
- Format is TOML.
- Documented for users in `doc/config.md`. Keep it in sync when config fields or file formats change. It must keep the notice that config is managed from the command line, not edited by hand.

## Commands
Commands keep the `*-provider` names; "gateway" is the user-facing word.
- `add-provider <name> --type litellm|fireworks [--alias A] [--base-url] [--account-id] [--credential-store keychain|file] [--api-key]` (key is prompted if omitted)
- `edit-provider <name|alias> [--alias A ("" removes)] [--base-url] [--account-id] [--credential-store keychain|file] [--api-key [KEY]]` (`--credential-store` moves the existing key to the new store)
- `remove-provider <name|alias>`
- `list-models [-g GATEWAY] [--filter TEXT] [--json]`: price per 1M tokens; filter is a case-insensitive substring on model id. Rows (and JSON) are sorted by gateway, then model id (case-insensitive), and duplicates (same gateway and id) are shown once.
- `usage [-g GATEWAY] [--json]`: current month (UTC, 1st to 1st of next month) cost and tokens, then a second table "Usage by model" (gateway, model, cost, tokens, requests; skipped when no gateway reports models).
- `--refresh` (global): ignore cached prices and fetch them again (the fresh answer is cached).
- `-v/--verbose` (global): `verbose: ...` lines on stderr only (stdout stays clean): config dir, each gateway (type, URL, credential store), and each HTTP request with status and time. Never log API keys; URLs are redacted with `verbose::redact` so credentials embedded in a base URL do not show up (also in error messages). Log with the `vlog!` macro. `-V` is `--version`.
- `--json` (list-models, usage): stdout is only JSON; errors and rate-limit notices stay on stderr and the exit code is unchanged. list-models prints an array of `{gateway, model, input_per_million, output_per_million}`; usage prints `{start, end, usage: [{gateway, cost_usd, prompt_tokens, completion_tokens, requests}], models: [{gateway, model, cost_usd, prompt_tokens, completion_tokens, requests}]}`. Unknown values are `null` (tables show `-`). Prices are rounded to 6 decimals to drop float noise.
- `completions [bash|zsh]`: prints a completion script (clap_complete); shell comes from `$SHELL` if omitted. Handled before the config dir is opened, so it creates no files. Completions are static (gateway names are not completed).
- `-g/--gateway` takes a name or alias; default and special value `all` queries every gateway in parallel (threads). Failures print to stderr, other results still print, exit code 1.
- `all` is reserved; names and aliases must be unique across both.

## Code layout
- `src/main.rs`: CLI and command handlers
- `src/store.rs`: config/credentials/key files and permissions
- `src/cache.rs`: 24-hour price cache (`GatewayCache::get_or_fetch`, time is passed in so tests control it)
- `src/crypto.rs`: encrypt/decrypt
- `src/keychain.rs`: macOS Keychain via the `security` command
- `src/verbose.rs`: `-v` logging (`vlog!`) and URL redaction
- `src/provider.rs`: provider kinds and model listing

## Testing
- Unit tests live in `#[cfg(test)]` modules in each `src/*.rs` file; end-to-end tests are in `tests/cli.rs`.
- `tests/cli.rs` runs the real binary with `--config-dir` set to a temp dir and a mock HTTP gateway (`Mock`) on 127.0.0.1. No real network or real keys are used.
- Keep pure logic separate from I/O so it can be unit tested (e.g. `parse_*` in `provider.rs`, `month_window`, `format_table`, `model_rows`).
- Table tests check structure (cell order, alignment), not exact spacing, so tweaking column gaps does not break them.
- `tests/fake_security.sh` is a stateful fake `security` (state in `$FAKE_SECURITY_DIR`, logs argv/stdin, `FAKE_SECURITY_FAIL=1` makes it fail). Used by unit and e2e tests.
- `cargo test -- --ignored` also runs `real_keychain_round_trip`, which uses the real macOS keychain (run it by hand after touching `keychain.rs`).
- Not tested: the interactive API-key prompt (needs a TTY) and real gateway responses (only mocks are used).

## Provider notes
- LiteLLM: needs `--base-url`. Models: `GET /model/info` (prices), falls back to `/v1/models` (no prices). Usage: `GET /user/daily/activity?start_date&end_date`, reads `metadata.total_*` for the totals and `results[].breakdown.models.<model>.metrics` (`spend`, `prompt_tokens`, `completion_tokens`, `api_requests`) summed over the days for the per-model table (shape seen on a real server; docs do not say if `end_date` is inclusive).
- Fireworks: default base URL `https://api.fireworks.ai/inference/v1`. Models: `GET https://api.fireworks.ai/v1/accounts/{account_id}/serverlessModels` (needs `account_id`; paged with `pageSize`/`nextPageToken`). Prices come from `serverlessModes[].skuInfos[]` (`amount` = `units` + `nanos`/1e9 USD per 1M tokens; SKUs "LLM input tokens (uncached)" and "LLM output tokens"). The standard mode (`serviceTier` empty or `standard`) is shown, else the first mode. Without an account id, or if the call fails, falls back to `GET /models` (no prices). Verified against the real API. `/v1/serverless-models` and `/v1/accounts/fireworks/models` do not work (404 / no prices).
- Fireworks usage needs `account_id`. Cost: `GET https://api.fireworks.ai/v1/accounts/{id}/billing/summary` (sums every `lineItems[].totalCost`, which is a money object `{units, nanos}`, not a number); tokens and the per-model table: `GET .../billingUsage` (sums `promptTokens`/`completionTokens`; rows in `serverlessCosts`/`dedicatedCosts`/`trainingCosts` are grouped by `modelName`). Per-model cost: the summary's `lineItems` with `groupingKey` `model_bucket` are priced by display name (e.g. "GLM 5.3 Flash"), so the model id (`modelName`) is mapped to its `displayName` from the serverless models list (same paged `/serverlessModels` call as `list-models`; if it fails, usage still prints and per-model costs fall back). `costNanoUsd`/1e9 is only the fallback, because the API often reports 0 for rows the summary does price; 0 is shown as unknown (`-`). Params `startTime`/`endTime` (RFC 3339, end exclusive, max 31 days). Shapes verified against the real API.

## License
- BSD 2-Clause, copyright 2026 Isaac Foraker (`LICENSE`; also set in `Cargo.toml`).

## Rate limits (HTTP 429)
- All GETs go through `get_json` in `provider.rs`. On a 429 it waits as long as the response says, then retries (max 5 retries).
- Wait comes from `Retry-After` (seconds, fractional seconds, or HTTP date), else `retry-after-ms`, else backs off 1s, 2s, 4s, 8s, 16s.
- If the wait is over 300s (`MAX_WAIT`) it fails at once with "the service asked us to wait Ns, which exceeds our 300s timeout"; it does not wait or retry. The error is a `RateLimitError`, and the LiteLLM `/model/info` -> `/models` fallback does not hide it.
- Each wait prints `rate limited (429) by URL; waiting Ns (retry k/5)` on stderr. With `-g all`, gateways wait independently.
