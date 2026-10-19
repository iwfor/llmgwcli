# Project Notes

## Notes to Remember
- Keep all project memories in this file (CLAUDE.md).
- Keep responses concise and in simple English.
- The project is written in Rust.

## Overview
`llmgwcli` is a CLI client for LLM gateways. Supported: LiteLLM and fireworks.ai.

## Config (managed by the tool)
- Default dir: `~/.config/llmgwcli` (override with `--config-dir` or `LLMGWCLI_CONFIG_DIR`).
- Dir is mode 0700; all files are 0600.
- `llmgwcli.key`: random 32-char alphanumeric key, created if missing.
- `credentials.toml`: API keys encrypted with the key (ChaCha20-Poly1305, base64 of nonce+ciphertext).
- `config.toml`: all other settings (`[providers.<name>]` with `type` and `base_url`).
- Format is TOML.

## Commands
Commands keep the `*-provider` names; "gateway" is the user-facing word.
- `add-provider <name> --type litellm|fireworks [--alias A] [--base-url] [--account-id] [--api-key]` (key is prompted if omitted)
- `edit-provider <name|alias> [--alias A ("" removes)] [--base-url] [--account-id] [--api-key [KEY]]`
- `remove-provider <name|alias>`
- `list-models [-g GATEWAY] [--filter TEXT]`: price per 1M tokens; filter is a case-insensitive substring on model id.
- `usage [-g GATEWAY]`: current month (UTC, 1st to 1st of next month) cost and tokens.
- `completions [bash|zsh]`: prints a completion script (clap_complete); shell comes from `$SHELL` if omitted. Handled before the config dir is opened, so it creates no files. Completions are static (gateway names are not completed).
- `-g/--gateway` takes a name or alias; default and special value `all` queries every gateway in parallel (threads). Failures print to stderr, other results still print, exit code 1.
- `all` is reserved; names and aliases must be unique across both.

## Code layout
- `src/main.rs`: CLI and command handlers
- `src/store.rs`: config/credentials/key files and permissions
- `src/crypto.rs`: encrypt/decrypt
- `src/provider.rs`: provider kinds and model listing

## Provider notes
- LiteLLM: needs `--base-url`. Models: `GET /model/info` (prices), falls back to `/v1/models` (no prices). Usage: `GET /user/daily/activity?start_date&end_date`, reads `metadata.total_*` (tested only against a mock; docs do not say if `end_date` is inclusive).
- Fireworks: default base URL `https://api.fireworks.ai/inference/v1`. Models: `GET /models`; the price fields (`pricing.input/output`) are a guess, NOT verified.
- Fireworks usage needs `account_id`. Cost: `GET https://api.fireworks.ai/v1/accounts/{id}/billing/summary` (sums every `totalCost`); tokens: `GET .../billingUsage` (sums `promptTokens`/`completionTokens`). Params `startTime`/`endTime` (RFC 3339, end exclusive, max 31 days). Response shapes are NOT verified; parsing is lenient.

## License
- BSD 2-Clause, copyright 2026 Isaac Foraker (`LICENSE`; also set in `Cargo.toml`).
