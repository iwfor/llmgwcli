# llmgwcli

A command-line client for LLM gateways. Supports [LiteLLM](https://docs.litellm.ai/) and [Fireworks AI](https://fireworks.ai/).

## Build

```
cargo build --release
```

## Usage

```
# Add gateways (the API key is prompted if --api-key is omitted)
llmgwcli add-provider fireworks --type fireworks --alias fw --account-id my-account
llmgwcli add-provider mylite --type litellm --alias ll --base-url http://localhost:4000

# Change or remove a gateway (name or alias)
llmgwcli edit-provider fw --api-key
llmgwcli remove-provider ll

# List models with prices (USD per 1M tokens)
llmgwcli list-models -g fw --filter llama

# This month's usage (UTC)
llmgwcli usage -g all

# Shell completions (bash or zsh; detected from $SHELL if omitted)
source <(llmgwcli completions)
```

`-g/--gateway` takes a gateway name or alias. The default, `all`, queries every gateway in parallel.

## Configuration

Files live in `~/.config/llmgwcli` (override with `--config-dir` or `LLMGWCLI_CONFIG_DIR`). The tool manages them; the directory and files are readable only by you.

| File | Contents |
|------|----------|
| `config.toml` | Gateways, URLs, aliases |
| `credentials.toml` | API keys for gateways that use the `file` credential store, encrypted |
| `llmgwcli.key` | Random 32-character key used to encrypt `credentials.toml` |

### Where API keys are kept

Each gateway keeps its API key in one of two places:

- `keychain` (default on macOS): the Apple Keychain, through the `security` command. The key never goes into our files and is never passed as a command-line argument.
- `file` (default elsewhere): encrypted in `credentials.toml`.

Pick one with `--credential-store keychain|file` on `add-provider`. Move an existing key with `edit-provider NAME --credential-store file`. Gateways added before this option existed keep using the file.

If you use the `file` store, keep `llmgwcli.key` safe. Without it, stored API keys cannot be decrypted.

## Status

Fireworks model prices and Fireworks usage parsing are not yet verified against the real API.

## License

BSD 2-Clause. See [LICENSE](LICENSE).
