//! On-disk config: config.toml, credentials.toml (encrypted) and llmgwcli.key.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rand::RngExt;
use rand::distr::Alphanumeric;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::crypto;
use crate::provider::ProviderKind;

const KEY_LEN: usize = 32;

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
}

impl Store {
    /// Opens the store and makes sure the directory and key file exist
    /// with user-only permissions.
    pub fn open(dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
        let store = Store { dir };
        store.ensure_key()?;
        Ok(store)
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

    pub fn api_key(&self, provider: &str) -> Result<Option<String>> {
        let creds: Credentials = read_toml(&self.path("credentials.toml"))?;
        creds
            .providers
            .get(provider)
            .map(|enc| crypto::decrypt(&self.key()?, enc))
            .transpose()
    }

    pub fn set_api_key(&self, provider: &str, api_key: &str) -> Result<()> {
        let path = self.path("credentials.toml");
        let mut creds: Credentials = read_toml(&path)?;
        let enc = crypto::encrypt(&self.key()?, api_key)?;
        creds.providers.insert(provider.to_string(), enc);
        write_toml(&path, &creds)
    }

    pub fn remove_api_key(&self, provider: &str) -> Result<()> {
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
fn write_private(path: &Path, data: &[u8]) -> Result<()> {
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
