//! On-disk config: config.toml, credentials.toml (encrypted) and llmgwcli.key.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::RngExt;
use rand::distr::Alphanumeric;
use clap::ValueEnum;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::crypto;
use crate::keychain::Keychain;
use crate::provider::ProviderKind;

const KEY_LEN: usize = 32;

/// Where a gateway's API key is kept.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum CredentialStore {
    /// The macOS Keychain, through the `security` command.
    Keychain,
    /// Encrypted in credentials.toml. Also what configs without this setting use.
    #[default]
    File,
}

impl CredentialStore {
    /// The Keychain on macOS, the credentials file elsewhere.
    pub fn platform_default() -> Self {
        if cfg!(target_os = "macos") { Self::Keychain } else { Self::File }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    pub base_url: String,
    /// Short name usable anywhere a gateway name is accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// Fireworks account slug, needed for billing/usage queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Where this gateway's API key lives.
    #[serde(default)]
    pub credential_store: CredentialStore,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// Reserved gateway selector meaning every configured gateway.
pub const ALL_GATEWAYS: &str = "all";

impl Config {
    /// Finds a gateway's canonical name from its name or alias.
    pub fn resolve(&self, name_or_alias: &str) -> Result<String> {
        self.providers
            .iter()
            .find(|(name, p)| *name == name_or_alias || p.alias.as_deref() == Some(name_or_alias))
            .map(|(name, _)| name.clone())
            .with_context(|| format!("unknown gateway '{name_or_alias}'"))
    }

    /// Canonical names selected by a `-g` value; `all` selects every gateway.
    pub fn select(&self, gateway: &str) -> Result<Vec<String>> {
        if gateway == ALL_GATEWAYS {
            anyhow::ensure!(!self.providers.is_empty(), "no gateways configured; use add-provider");
            return Ok(self.providers.keys().cloned().collect());
        }
        Ok(vec![self.resolve(gateway)?])
    }

    /// Fails if `id` is reserved or already used as a name or alias
    /// (by any gateway except `except`).
    pub fn ensure_free(&self, id: &str, except: Option<&str>) -> Result<()> {
        anyhow::ensure!(id != ALL_GATEWAYS, "'{ALL_GATEWAYS}' is reserved");
        let taken = self.providers.iter().any(|(name, p)| {
            Some(name.as_str()) != except && (name == id || p.alias.as_deref() == Some(id))
        });
        anyhow::ensure!(!taken, "'{id}' is already used as a gateway name or alias");
        Ok(())
    }
}

/// Provider name -> encrypted API key.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Credentials {
    #[serde(default)]
    providers: BTreeMap<String, String>,
}

pub struct Store {
    dir: PathBuf,
    keychain: Keychain,
}

impl Store {
    /// Opens the store and makes sure the directory and key file exist
    /// with user-only permissions.
    pub fn open(dir: PathBuf) -> Result<Self> {
        Self::open_with_keychain(dir, Keychain::new())
    }

    pub fn open_with_keychain(dir: PathBuf, keychain: Keychain) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let store = Store { dir, keychain };
        store.ensure_key()?;
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    fn ensure_key(&self) -> Result<()> {
        let path = self.path("llmgwcli.key");
        if path.exists() {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            return Ok(());
        }
        let key: String = rand::rng()
            .sample_iter(Alphanumeric)
            .take(KEY_LEN)
            .map(char::from)
            .collect();
        write_private(&path, key.as_bytes())
    }

    fn key(&self) -> Result<String> {
        let key = fs::read_to_string(self.path("llmgwcli.key"))?;
        let key = key.trim().to_string();
        anyhow::ensure!(key.len() == KEY_LEN, "llmgwcli.key must be {KEY_LEN} characters");
        Ok(key)
    }

    pub fn load_config(&self) -> Result<Config> {
        read_toml(&self.path("config.toml"))
    }

    pub fn save_config(&self, config: &Config) -> Result<()> {
        write_toml(&self.path("config.toml"), config)
    }

    pub fn api_key(&self, provider: &str, backend: CredentialStore) -> Result<Option<String>> {
        match backend {
            CredentialStore::Keychain => self.keychain.get(provider),
            CredentialStore::File => self.file_api_key(provider),
        }
    }

    pub fn set_api_key(&self, provider: &str, api_key: &str, backend: CredentialStore) -> Result<()> {
        match backend {
            CredentialStore::Keychain => self.keychain.set(provider, api_key),
            CredentialStore::File => self.set_file_api_key(provider, api_key),
        }
    }

    pub fn remove_api_key(&self, provider: &str, backend: CredentialStore) -> Result<()> {
        match backend {
            CredentialStore::Keychain => self.keychain.remove(provider),
            CredentialStore::File => self.remove_file_api_key(provider),
        }
    }

    /// Copies the key to the new backend, then removes it from the old one.
    /// The old key is kept if the write fails.
    pub fn move_api_key(&self, provider: &str, from: CredentialStore, to: CredentialStore) -> Result<()> {
        let key = self
            .api_key(provider, from)?
            .with_context(|| format!("no API key stored for '{provider}' to move"))?;
        self.set_api_key(provider, &key, to)?;
        self.remove_api_key(provider, from)
    }

    fn file_api_key(&self, provider: &str) -> Result<Option<String>> {
        let creds: Credentials = read_toml(&self.path("credentials.toml"))?;
        creds
            .providers
            .get(provider)
            .map(|enc| crypto::decrypt(&self.key()?, enc))
            .transpose()
    }

    fn set_file_api_key(&self, provider: &str, api_key: &str) -> Result<()> {
        let path = self.path("credentials.toml");
        let mut creds: Credentials = read_toml(&path)?;
        let enc = crypto::encrypt(&self.key()?, api_key)?;
        creds.providers.insert(provider.to_string(), enc);
        write_toml(&path, &creds)
    }

    fn remove_file_api_key(&self, provider: &str) -> Result<()> {
        let path = self.path("credentials.toml");
        let mut creds: Credentials = read_toml(&path)?;
        creds.providers.remove(provider);
        write_toml(&path, &creds)
    }
}

/// Reads a TOML file, returning the default value if it does not exist.
fn read_toml<T: DeserializeOwned + Default>(path: &Path) -> Result<T> {
    match fs::read_to_string(path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn write_toml<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_private(path, toml::to_string_pretty(value)?.as_bytes())
}

/// Writes a file readable only by the current user.
pub(crate) fn write_private(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("writing {}", path.display()))?;
    file.write_all(data)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keychain::Keychain;
    use tempfile::TempDir;

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_security.sh");

    /// Store whose keychain is the fake `security` script, with its state in `keychain/`.
    fn open() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let keychain = Keychain::with_program(FAKE)
            .with_env("FAKE_SECURITY_DIR", tmp.path().join("keychain"));
        let store = Store::open_with_keychain(tmp.path().join("cfg"), keychain).unwrap();
        (tmp, store)
    }

    fn gateway(alias: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind: ProviderKind::Litellm,
            base_url: "http://localhost:4000".into(),
            alias: alias.map(String::from),
            account_id: None,
            credential_store: CredentialStore::File,
        }
    }

    fn config() -> Config {
        let mut config = Config::default();
        config.providers.insert("lite".into(), gateway(Some("ll")));
        config.providers.insert("other".into(), gateway(None));
        config
    }

    #[test]
    fn open_creates_private_dir_and_key() {
        let (tmp, store) = open();
        let dir = tmp.path().join("cfg");
        assert_eq!(mode(&dir), 0o700);
        let key = fs::read_to_string(dir.join("llmgwcli.key")).unwrap();
        assert_eq!(key.len(), 32);
        assert!(key.chars().all(|c| c.is_ascii_alphanumeric()));
        assert_eq!(mode(&dir.join("llmgwcli.key")), 0o600);
        assert_eq!(store.key().unwrap(), key);
    }

    #[test]
    fn open_keeps_existing_key() {
        let (tmp, store) = open();
        let first = store.key().unwrap();
        let again = Store::open_with_keychain(tmp.path().join("cfg"), Keychain::new()).unwrap();
        assert_eq!(again.key().unwrap(), first);
    }

    #[test]
    fn open_tightens_loose_permissions() {
        let (tmp, _store) = open();
        let dir = tmp.path().join("cfg");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(dir.join("llmgwcli.key"), fs::Permissions::from_mode(0o644)).unwrap();
        Store::open_with_keychain(dir.clone(), Keychain::new()).unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("llmgwcli.key")), 0o600);
    }

    #[test]
    fn bad_key_length_is_rejected() {
        let (tmp, store) = open();
        fs::write(tmp.path().join("cfg/llmgwcli.key"), "short").unwrap();
        assert!(store.key().is_err());
    }

    #[test]
    fn missing_config_is_empty() {
        let (_tmp, store) = open();
        assert!(store.load_config().unwrap().providers.is_empty());
    }

    #[test]
    fn config_round_trips_and_is_private() {
        let (tmp, store) = open();
        store.save_config(&config()).unwrap();
        let loaded = store.load_config().unwrap();
        assert_eq!(loaded.providers.len(), 2);
        assert_eq!(loaded.providers["lite"].alias.as_deref(), Some("ll"));
        assert_eq!(loaded.providers["lite"].kind, ProviderKind::Litellm);
        assert_eq!(mode(&tmp.path().join("cfg/config.toml")), 0o600);
    }

    #[test]
    fn config_omits_unset_optional_fields() {
        let (tmp, store) = open();
        store.save_config(&config()).unwrap();
        let text = fs::read_to_string(tmp.path().join("cfg/config.toml")).unwrap();
        assert!(text.contains("type = \"litellm\""));
        assert!(!text.contains("account_id"));
    }

    #[test]
    fn invalid_config_reports_the_file() {
        let (tmp, store) = open();
        fs::write(tmp.path().join("cfg/config.toml"), "providers = [").unwrap();
        let err = store.load_config().unwrap_err();
        assert!(format!("{err:#}").contains("config.toml"));
    }

    #[test]
    fn api_key_is_encrypted_and_round_trips() {
        let (tmp, store) = open();
        store.set_api_key("lite", "sk-plain-secret", CredentialStore::File).unwrap();
        assert_eq!(store.api_key("lite", CredentialStore::File).unwrap().as_deref(), Some("sk-plain-secret"));
        let text = fs::read_to_string(tmp.path().join("cfg/credentials.toml")).unwrap();
        assert!(!text.contains("sk-plain-secret"));
        assert_eq!(mode(&tmp.path().join("cfg/credentials.toml")), 0o600);
    }

    #[test]
    fn api_key_missing_is_none() {
        let (_tmp, store) = open();
        assert_eq!(store.api_key("nope", CredentialStore::File).unwrap(), None);
    }

    #[test]
    fn set_api_key_replaces_and_keeps_others() {
        let (_tmp, store) = open();
        store.set_api_key("a", "one", CredentialStore::File).unwrap();
        store.set_api_key("b", "two", CredentialStore::File).unwrap();
        store.set_api_key("a", "three", CredentialStore::File).unwrap();
        assert_eq!(store.api_key("a", CredentialStore::File).unwrap().as_deref(), Some("three"));
        assert_eq!(store.api_key("b", CredentialStore::File).unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn remove_api_key_only_removes_that_gateway() {
        let (_tmp, store) = open();
        store.set_api_key("a", "one", CredentialStore::File).unwrap();
        store.set_api_key("b", "two", CredentialStore::File).unwrap();
        store.remove_api_key("a", CredentialStore::File).unwrap();
        assert_eq!(store.api_key("a", CredentialStore::File).unwrap(), None);
        assert_eq!(store.api_key("b", CredentialStore::File).unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn api_key_fails_with_a_different_key_file() {
        let (tmp, store) = open();
        store.set_api_key("a", "one", CredentialStore::File).unwrap();
        fs::write(tmp.path().join("cfg/llmgwcli.key"), "A".repeat(32)).unwrap();
        assert!(store.api_key("a", CredentialStore::File).is_err());
    }

    #[test]
    fn resolve_accepts_name_or_alias() {
        let config = config();
        assert_eq!(config.resolve("lite").unwrap(), "lite");
        assert_eq!(config.resolve("ll").unwrap(), "lite");
        assert!(config.resolve("nope").unwrap_err().to_string().contains("unknown gateway 'nope'"));
    }

    #[test]
    fn select_all_returns_every_gateway_sorted() {
        assert_eq!(config().select("all").unwrap(), ["lite", "other"]);
    }

    #[test]
    fn select_single_gateway_by_alias() {
        assert_eq!(config().select("ll").unwrap(), ["lite"]);
    }

    #[test]
    fn select_all_with_no_gateways_fails() {
        assert!(Config::default().select("all").is_err());
    }

    #[test]
    fn select_unknown_fails() {
        assert!(config().select("nope").is_err());
    }

    #[test]
    fn ensure_free_rejects_reserved_and_taken() {
        let config = config();
        assert!(config.ensure_free("all", None).is_err());
        assert!(config.ensure_free("lite", None).is_err());
        assert!(config.ensure_free("ll", None).is_err());
        assert!(config.ensure_free("fresh", None).is_ok());
    }

    #[test]
    fn ensure_free_ignores_the_excepted_gateway() {
        let config = config();
        assert!(config.ensure_free("ll", Some("lite")).is_ok());
        assert!(config.ensure_free("ll", Some("other")).is_err());
    }
    use CredentialStore::{File, Keychain as InKeychain};

    #[test]
    fn platform_default_is_keychain_only_on_macos() {
        let expected = if cfg!(target_os = "macos") { InKeychain } else { File };
        assert_eq!(CredentialStore::platform_default(), expected);
    }

    #[test]
    fn config_without_credential_store_reads_as_file() {
        let (tmp, store) = open();
        fs::write(
            tmp.path().join("cfg/config.toml"),
            "[providers.old]\ntype = \"litellm\"\nbase_url = \"http://x\"\n",
        )
        .unwrap();
        assert_eq!(store.load_config().unwrap().providers["old"].credential_store, File);
    }

    #[test]
    fn credential_store_is_saved_in_config() {
        let (tmp, store) = open();
        let mut config = config();
        config.providers.get_mut("lite").unwrap().credential_store = InKeychain;
        store.save_config(&config).unwrap();
        let text = fs::read_to_string(tmp.path().join("cfg/config.toml")).unwrap();
        assert!(text.contains("credential_store = \"keychain\""), "{text}");
        assert_eq!(store.load_config().unwrap().providers["lite"].credential_store, InKeychain);
    }

    #[test]
    fn keychain_secret_stays_out_of_the_credentials_file() {
        let (tmp, store) = open();
        store.set_api_key("a", "sk-in-keychain", InKeychain).unwrap();
        assert_eq!(store.api_key("a", InKeychain).unwrap().as_deref(), Some("sk-in-keychain"));
        assert_eq!(store.api_key("a", File).unwrap(), None);
        let creds = fs::read_to_string(tmp.path().join("cfg/credentials.toml")).unwrap_or_default();
        assert!(!creds.contains("sk-in-keychain") && !creds.contains("a ="), "{creds}");
    }

    #[test]
    fn file_secret_stays_out_of_the_keychain() {
        let (tmp, store) = open();
        store.set_api_key("a", "sk-in-file", File).unwrap();
        assert_eq!(store.api_key("a", InKeychain).unwrap(), None);
        assert!(!tmp.path().join("keychain").join("llmgwcli.a").exists());
    }

    #[test]
    fn remove_api_key_uses_the_given_backend() {
        let (_tmp, store) = open();
        store.set_api_key("a", "kc", InKeychain).unwrap();
        store.set_api_key("a", "file", File).unwrap();
        store.remove_api_key("a", InKeychain).unwrap();
        assert_eq!(store.api_key("a", InKeychain).unwrap(), None);
        assert_eq!(store.api_key("a", File).unwrap().as_deref(), Some("file"));
    }

    #[test]
    fn move_api_key_between_backends() {
        let (_tmp, store) = open();
        store.set_api_key("a", "sk-move", File).unwrap();
        store.move_api_key("a", File, InKeychain).unwrap();
        assert_eq!(store.api_key("a", InKeychain).unwrap().as_deref(), Some("sk-move"));
        assert_eq!(store.api_key("a", File).unwrap(), None);
        store.move_api_key("a", InKeychain, File).unwrap();
        assert_eq!(store.api_key("a", File).unwrap().as_deref(), Some("sk-move"));
        assert_eq!(store.api_key("a", InKeychain).unwrap(), None);
    }

    #[test]
    fn move_api_key_without_a_stored_key_fails() {
        let (_tmp, store) = open();
        let err = store.move_api_key("a", File, InKeychain).unwrap_err();
        assert!(err.to_string().contains("no API key"), "{err}");
        assert_eq!(store.api_key("a", InKeychain).unwrap(), None);
    }

    #[test]
    fn failed_keychain_write_keeps_the_old_key() {
        let (tmp, store) = open();
        store.set_api_key("a", "sk-keep", File).unwrap();
        let broken = Keychain::with_program(FAKE)
            .with_env("FAKE_SECURITY_DIR", tmp.path().join("keychain"))
            .with_env("FAKE_SECURITY_FAIL", "1");
        let store = Store::open_with_keychain(tmp.path().join("cfg"), broken).unwrap();
        assert!(store.move_api_key("a", File, InKeychain).is_err());
        assert_eq!(store.api_key("a", File).unwrap().as_deref(), Some("sk-keep"));
    }
}
