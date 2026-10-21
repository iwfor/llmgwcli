//! On-disk cache for per-model prices: one file per gateway in
//! `<config dir>/cache/`. Only prices are cached (never usage figures), so a
//! day-old entry can only mean a day-old price list.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::write_private;
use crate::verbose::{redact_str, vlog};

/// How long a cached response is used before it is fetched again.
const TTL: Duration = Duration::hours(24);

#[derive(Serialize, Deserialize)]
struct Entry {
    /// The request the data answers, without credentials. A changed URL
    /// (new base URL or account) is a different request.
    url: String,
    /// Unix seconds.
    fetched_at: i64,
    data: Value,
}

/// The cached responses of one gateway.
pub struct GatewayCache {
    path: PathBuf,
    /// Ignore what is cached (the new response still replaces it).
    refresh: bool,
}

impl GatewayCache {
    pub fn new(config_dir: &Path, gateway: &str, refresh: bool) -> Self {
        let path = config_dir.join("cache").join(format!("{}.json", file_stem(gateway)));
        GatewayCache { path, refresh }
    }

    /// The cached response for `url` if it is under 24 hours old, else the
    /// result of `fetch`, which is then cached. A cache that cannot be read or
    /// written is skipped, never an error.
    pub fn get_or_fetch(&self, url: &str, now: DateTime<Utc>, fetch: impl FnOnce() -> Result<Value>) -> Result<Value> {
        let url = redact_str(url).to_string();
        if !self.refresh {
            if let Some(data) = self.read(&url, now) {
                return Ok(data);
            }
        }
        let data = fetch()?;
        if let Err(e) = self.write(&Entry { url, fetched_at: now.timestamp(), data: data.clone() }) {
            vlog!("could not write {}: {e:#}", self.path.display());
        }
        Ok(data)
    }

    /// Forgets the gateway's cached responses.
    pub fn remove(&self) {
        if let Err(e) = fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                vlog!("could not remove {}: {e}", self.path.display());
            }
        }
    }

    fn read(&self, url: &str, now: DateTime<Utc>) -> Option<Value> {
        let entry: Entry = serde_json::from_str(&fs::read_to_string(&self.path).ok()?).ok()?;
        let age = Duration::seconds(now.timestamp() - entry.fetched_at);
        // An entry from the future means the clock moved; do not trust it.
        let fresh = age >= Duration::zero() && age < TTL;
        (entry.url == url && fresh).then(|| {
            vlog!("using cached response for {url} (fetched {}s ago)", age.num_seconds());
            entry.data
        })
    }

    fn write(&self, entry: &Entry) -> Result<()> {
        let dir = self.path.parent().expect("cache file has a directory");
        fs::create_dir_all(dir)?;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
        write_private(&self.path, serde_json::to_string(entry)?.as_bytes())
    }
}

/// A file name for a gateway name: letters, digits, `-` and `_` stay, every
/// other byte becomes `%XX`, so names cannot leave the cache directory.
fn file_stem(name: &str) -> String {
    name.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    use chrono::{DateTime, Duration, TimeZone, Utc};
    use serde_json::{Value, json};
    use tempfile::TempDir;

    use super::*;

    const URL: &str = "https://api.example.com/v1/models";

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).unwrap()
    }

    fn cache(dir: &TempDir, refresh: bool) -> GatewayCache {
        GatewayCache::new(dir.path(), "fw", refresh)
    }

    /// Fetches through `cache`, returning the value and how many real fetches ran.
    fn fetch(cache: &GatewayCache, url: &str, now: DateTime<Utc>, value: Value) -> (Value, u32) {
        let calls = Cell::new(0);
        let got = cache
            .get_or_fetch(url, now, || {
                calls.set(calls.get() + 1);
                Ok(value)
            })
            .unwrap();
        (got, calls.get())
    }

    #[test]
    fn second_call_is_served_from_the_cache() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        assert_eq!(fetch(&c, URL, t0(), json!({"v": 1})), (json!({"v": 1}), 1));
        assert_eq!(fetch(&c, URL, t0() + Duration::hours(1), json!({"v": 2})), (json!({"v": 1}), 0));
    }

    #[test]
    fn entries_expire_after_24_hours() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        fetch(&c, URL, t0(), json!(1));
        let almost = t0() + Duration::hours(24) - Duration::seconds(1);
        assert_eq!(fetch(&c, URL, almost, json!(2)), (json!(1), 0));
        assert_eq!(fetch(&c, URL, t0() + Duration::hours(24), json!(3)), (json!(3), 1));
        // The refetch starts a new 24 hours.
        assert_eq!(fetch(&c, URL, t0() + Duration::hours(30), json!(4)), (json!(3), 0));
    }

    #[test]
    fn a_different_url_is_a_miss() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        fetch(&c, URL, t0(), json!(1));
        assert_eq!(fetch(&c, "https://other.example.com/v1/models", t0(), json!(2)), (json!(2), 1));
    }

    #[test]
    fn gateways_do_not_share_entries() {
        let dir = TempDir::new().unwrap();
        fetch(&GatewayCache::new(dir.path(), "a", false), URL, t0(), json!("a"));
        let (got, calls) = fetch(&GatewayCache::new(dir.path(), "b", false), URL, t0(), json!("b"));
        assert_eq!((got, calls), (json!("b"), 1));
    }

    #[test]
    fn refresh_skips_the_cache_but_updates_it() {
        let dir = TempDir::new().unwrap();
        fetch(&cache(&dir, false), URL, t0(), json!(1));
        assert_eq!(fetch(&cache(&dir, true), URL, t0(), json!(2)), (json!(2), 1));
        assert_eq!(fetch(&cache(&dir, false), URL, t0(), json!(3)), (json!(2), 0));
    }

    #[test]
    fn failed_fetches_are_returned_and_not_cached() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        let err = c.get_or_fetch(URL, t0(), || anyhow::bail!("boom")).unwrap_err();
        assert_eq!(err.to_string(), "boom");
        assert_eq!(fetch(&c, URL, t0(), json!(1)), (json!(1), 1));
    }

    #[test]
    fn a_failed_fetch_keeps_an_expired_entry_out_of_use() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        fetch(&c, URL, t0(), json!(1));
        let later = t0() + Duration::hours(25);
        assert!(c.get_or_fetch(URL, later, || anyhow::bail!("down")).is_err());
    }

    #[test]
    fn corrupt_files_count_as_a_miss() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        fetch(&c, URL, t0(), json!(1));
        std::fs::write(&c.path, "{not json").unwrap();
        assert_eq!(fetch(&c, URL, t0(), json!(2)), (json!(2), 1));
    }

    #[test]
    fn entries_from_the_future_count_as_a_miss() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        fetch(&c, URL, t0() + Duration::hours(2), json!(1));
        assert_eq!(fetch(&c, URL, t0(), json!(2)), (json!(2), 1));
    }

    #[test]
    fn files_are_private() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        fetch(&c, URL, t0(), json!(1));
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&c.path), 0o600);
        assert_eq!(mode(c.path.parent().unwrap()), 0o700);
    }

    #[test]
    fn credentials_in_the_url_are_not_written() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        let url = "https://user:hunter2@api.example.com/v1/models";
        fetch(&c, url, t0(), json!(1));
        let text = std::fs::read_to_string(&c.path).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        // The same URL still hits.
        assert_eq!(fetch(&c, url, t0(), json!(2)), (json!(1), 0));
    }

    #[test]
    fn unwritable_cache_does_not_fail_the_call() {
        let dir = TempDir::new().unwrap();
        // A file where the cache directory should be.
        std::fs::write(dir.path().join("cache"), "in the way").unwrap();
        let c = cache(&dir, false);
        assert_eq!(fetch(&c, URL, t0(), json!(1)), (json!(1), 1));
        assert_eq!(fetch(&c, URL, t0(), json!(2)), (json!(2), 1));
    }

    #[test]
    fn remove_deletes_the_entry_and_tolerates_a_missing_one() {
        let dir = TempDir::new().unwrap();
        let c = cache(&dir, false);
        c.remove();
        fetch(&c, URL, t0(), json!(1));
        c.remove();
        assert!(!c.path.exists());
        assert_eq!(fetch(&c, URL, t0(), json!(2)), (json!(2), 1));
    }

    #[test]
    fn odd_gateway_names_stay_inside_the_cache_directory() {
        let dir = TempDir::new().unwrap();
        for name in ["../escape", "a/b", "..", "with space", "ünï"] {
            let path = GatewayCache::new(dir.path(), name, false).path;
            assert_eq!(path.parent().unwrap(), dir.path().join("cache"), "{name}");
        }
        let (a, b) = (GatewayCache::new(dir.path(), "a/b", false).path, GatewayCache::new(dir.path(), "a_b", false).path);
        assert_ne!(a, b);
    }
}
