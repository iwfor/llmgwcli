//! `-v/--verbose` diagnostics. They go to stderr so stdout stays clean for tables and JSON.
//! Never log API keys or URL credentials.

use std::sync::atomic::{AtomicBool, Ordering};

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn set(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Prints `verbose: ...` on stderr when -v/--verbose is on.
macro_rules! vlog {
    ($($arg:tt)*) => {
        if $crate::verbose::enabled() {
            eprintln!("verbose: {}", format_args!($($arg)*));
        }
    };
}
pub(crate) use vlog;

/// A URL without any user name or password.
pub fn redact(url: &reqwest::Url) -> String {
    let mut url = url.clone();
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.to_string()
}

/// Like `redact`, for a URL held as text. Text that is not a URL is not shown.
pub fn redact_str(url: &str) -> String {
    reqwest::Url::parse(url).map_or_else(|_| "<invalid url>".to_string(), |u| redact(&u))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn redacted(url: &str) -> String {
        redact(&reqwest::Url::parse(url).unwrap())
    }

    #[test]
    fn redact_removes_user_and_password() {
        assert_eq!(redacted("http://user:secret@host:4000/a?x=1"), "http://host:4000/a?x=1");
        assert_eq!(redacted("https://token@host/a"), "https://host/a");
    }

    #[test]
    fn redact_str_hides_credentials_and_unparseable_text() {
        assert_eq!(redact_str("http://u:p@h/x"), "http://h/x");
        assert_eq!(redact_str("not a url with a secret"), "<invalid url>");
    }

    #[test]
    fn redact_leaves_other_urls_alone() {
        assert_eq!(redacted("http://127.0.0.1:4000/model/info"), "http://127.0.0.1:4000/model/info");
        assert_eq!(
            redacted("https://api.fireworks.ai/v1/accounts/a/billing/summary?startTime=2026-10-01T00%3A00%3A00Z"),
            "https://api.fireworks.ai/v1/accounts/a/billing/summary?startTime=2026-10-01T00%3A00%3A00Z"
        );
    }
}
