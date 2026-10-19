# Configuration files

> **Do not edit these files by hand.** llmgwcli is designed so that its
> configuration is managed completely from the command line
> (`add-provider`, `edit-provider`, `remove-provider`). This page describes
> the files so you know what the tool stores and where. The format may change
> between versions, and a hand edit can break your setup or leave API keys
> unreadable.

## Location

Files live in `~/.config/llmgwcli` by default. Use `--config-dir DIR` or the
`LLMGWCLI_CONFIG_DIR` environment variable to use a different directory.

llmgwcli creates the directory if needed. It is readable only by you
(mode `0700`), and so are all the files in it (mode `0600`). The tool
re-applies the mode on the directory and on `llmgwcli.key` each time it runs,
and sets `0600` on the other files whenever it writes them.

| File | Contents |
|------|----------|
| `config.toml` | Gateways: type, URL, alias, account id, credential store |
| `credentials.toml` | API keys for gateways that use the `file` credential store, encrypted |
| `llmgwcli.key` | Random 32-character key that encrypts `credentials.toml` |

All files are TOML except `llmgwcli.key`, which is plain text.

## config.toml

One `[providers.NAME]` table per gateway. `NAME` is the gateway name you gave
to `add-provider`.

```toml
[providers.fireworks]
type = "fireworks"
base_url = "https://api.fireworks.ai/inference/v1"
alias = "fw"
account_id = "my-account"
credential_store = "keychain"

[providers.mylite]
type = "litellm"
base_url = "http://localhost:4000"
credential_store = "file"
```

| Field | Required | Meaning | Set with |
|-------|----------|---------|----------|
| `type` | yes | `litellm` or `fireworks` | `add-provider --type` |
| `base_url` | yes | Gateway URL. Fireworks defaults to `https://api.fireworks.ai/inference/v1`; LiteLLM has no default | `--base-url` |
| `alias` | no | Short name accepted anywhere a gateway name is, such as `-g fw` | `--alias` (`edit-provider --alias ""` removes it) |
| `account_id` | no | Fireworks account id, needed for `usage` | `--account-id` |
| `credential_store` | no | Where the API key is kept: `keychain` or `file`. A gateway without this field uses `file` | `--credential-store` |

Rules for names and aliases:

- A name or alias must be unique across all gateways, names and aliases together.
- `all` is reserved. `-g all` means every gateway.

The default `credential_store` for a new gateway is `keychain` on macOS and
`file` everywhere else. `edit-provider NAME --credential-store ...` moves the
existing key to the other store.

## Credentials

The API key for each gateway is kept in the store named by its
`credential_store`. The key itself is never written to `config.toml`.

### Keychain (macOS default)

The key is kept in the Apple Keychain, using the `security` command:

- service: `llmgwcli`
- account: the gateway name

llmgwcli sends the key to `security` over standard input, so it never appears
in the process list. You can see or remove an entry in Keychain Access, or with
`security find-generic-password -s llmgwcli -a NAME`.

Nothing for these gateways is written to `credentials.toml`.

### File

The key is encrypted and stored in `credentials.toml`:

```toml
[providers]
fireworks = "uJ3p...base64..."
```

Each value is base64 of a 12-byte random nonce followed by the
ChaCha20-Poly1305 ciphertext. The encryption key is the 32 characters in
`llmgwcli.key`. A fresh nonce is used every time a key is saved.

### llmgwcli.key

A random 32-character letters-and-digits key. llmgwcli creates it the first
time it runs and never changes it afterwards.

- If you delete it, llmgwcli makes a new one, and any keys already in
  `credentials.toml` can no longer be decrypted. Run
  `edit-provider NAME --api-key` for each affected gateway to enter its key again.
- Keep it as private as the credentials file. Anyone who has both files can
  read the API keys.
- It only protects keys in `credentials.toml`. Gateways that use the keychain
  do not depend on it.

The encryption protects against casual exposure, such as a backup that holds
`credentials.toml` but not `llmgwcli.key`. It does not protect against someone
who can read both files as you. The macOS Keychain is the stronger choice,
which is why it is the default there.

## Backups and moving to another machine

- Back up the whole directory, including `llmgwcli.key`, to keep `file`
  credentials usable.
- Keychain items are not in the directory. After copying the directory to
  another Mac, run `edit-provider NAME --api-key` for each keychain gateway.
