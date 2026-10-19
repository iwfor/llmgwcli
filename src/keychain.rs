//! macOS Keychain access through the `security` command.
//!
//! Secrets are written by piping a command to `security -i` so they never
//! appear in the process argument list.

#[cfg(test)]
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

const SERVICE: &str = "llmgwcli";
/// Exit code `security` uses when the item does not exist.
const NOT_FOUND: i32 = 44;

pub struct Keychain {
    program: PathBuf,
    #[cfg(test)]
    env: Vec<(String, OsString)>,
}

impl Keychain {
    /// Uses `security` from PATH, or the program in LLMGWCLI_SECURITY_BIN (for tests).
    pub fn new() -> Self {
        Self::with_program(std::env::var_os("LLMGWCLI_SECURITY_BIN").unwrap_or_else(|| "security".into()))
    }

    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Keychain {
            program: program.into(),
            #[cfg(test)]
            env: Vec::new(),
        }
    }

    /// Extra environment for the spawned program (tests run in parallel, so no global env).
    #[cfg(test)]
    pub fn with_env(mut self, key: &str, value: impl Into<OsString>) -> Self {
        self.env.push((key.to_string(), value.into()));
        self
    }

    fn command(&self) -> Command {
        let cmd = Command::new(&self.program);
        #[cfg(test)]
        let mut cmd = cmd;
        #[cfg(test)]
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        cmd
    }

    fn run(&self, mut cmd: Command) -> Result<Output> {
        cmd.output().with_context(|| format!("running {}", self.program.display()))
    }

    pub fn get(&self, account: &str) -> Result<Option<String>> {
        let mut cmd = self.command();
        cmd.args(["find-generic-password", "-s", SERVICE, "-a", account, "-w"]);
        let out = self.run(cmd)?;
        match out.status.code() {
            Some(0) => {
                let secret = String::from_utf8(out.stdout).context("keychain value is not UTF-8")?;
                Ok(Some(secret.strip_suffix('\n').unwrap_or(&secret).to_string()))
            }
            Some(NOT_FOUND) => Ok(None),
            _ => bail!("{}", failure(&out)),
        }
    }

    /// Adds the item, or updates it if it already exists.
    pub fn set(&self, account: &str, secret: &str) -> Result<()> {
        let line = format!(
            "add-generic-password -U -s {} -a {} -w {}\n",
            quote(SERVICE)?,
            quote(account)?,
            quote(secret)?
        );
        let mut cmd = self.command();
        let mut child = cmd
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", self.program.display()))?;
        let written = child.stdin.take().expect("stdin is piped").write_all(line.as_bytes());
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("{}", failure(&out));
        }
        written.context("sending the secret to security")
    }

    pub fn remove(&self, account: &str) -> Result<()> {
        let mut cmd = self.command();
        cmd.args(["delete-generic-password", "-s", SERVICE, "-a", account]);
        let out = self.run(cmd)?;
        match out.status.code() {
            Some(0) | Some(NOT_FOUND) => Ok(()),
            _ => bail!("{}", failure(&out)),
        }
    }
}

/// Describes a failed `security` call from its stderr (never includes the secret).
fn failure(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        format!("security failed ({})", out.status)
    } else {
        format!("security failed: {stderr}")
    }
}

/// Quotes a value for a `security -i` command line.
fn quote(value: &str) -> Result<String> {
    if value.contains(['\n', '\r', '\0']) {
        bail!("value cannot contain a newline or NUL");
    }
    Ok(format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    const FAKE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_security.sh");

    /// A keychain backed by the fake `security` script; state lives in the temp dir.
    fn fake() -> (TempDir, Keychain) {
        let tmp = TempDir::new().unwrap();
        // Tests run in parallel, so pass the state dir to the script as part of the command.
        let keychain = Keychain::with_program(FAKE).with_env("FAKE_SECURITY_DIR", tmp.path());
        (tmp, keychain)
    }

    fn log(tmp: &TempDir, name: &str) -> String {
        fs::read_to_string(tmp.path().join(name)).unwrap_or_default()
    }

    #[test]
    fn get_missing_is_none() {
        let (_tmp, kc) = fake();
        assert_eq!(kc.get("nope").unwrap(), None);
    }

    #[test]
    fn set_then_get_round_trips() {
        let (_tmp, kc) = fake();
        kc.set("fw", "sk-secret").unwrap();
        assert_eq!(kc.get("fw").unwrap().as_deref(), Some("sk-secret"));
    }

    #[test]
    fn set_replaces_and_accounts_are_separate() {
        let (_tmp, kc) = fake();
        kc.set("a", "one").unwrap();
        kc.set("b", "two").unwrap();
        kc.set("a", "three").unwrap();
        assert_eq!(kc.get("a").unwrap().as_deref(), Some("three"));
        assert_eq!(kc.get("b").unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn remove_deletes_only_that_account() {
        let (_tmp, kc) = fake();
        kc.set("a", "one").unwrap();
        kc.set("b", "two").unwrap();
        kc.remove("a").unwrap();
        assert_eq!(kc.get("a").unwrap(), None);
        assert_eq!(kc.get("b").unwrap().as_deref(), Some("two"));
    }

    #[test]
    fn remove_missing_is_ok() {
        let (_tmp, kc) = fake();
        kc.remove("nope").unwrap();
    }

    #[test]
    fn secret_is_sent_on_stdin_never_in_arguments() {
        let (tmp, kc) = fake();
        kc.set("fw", "sk-very-secret").unwrap();
        assert!(!log(&tmp, "argv.log").contains("sk-very-secret"));
        assert!(log(&tmp, "stdin.log").contains("-w \"sk-very-secret\""));
    }

    #[test]
    fn set_uses_the_llmgwcli_service_and_updates_in_place() {
        let (tmp, kc) = fake();
        kc.set("fw", "x").unwrap();
        let sent = log(&tmp, "stdin.log");
        assert!(sent.starts_with("add-generic-password -U "), "{sent}");
        assert!(sent.contains("-s \"llmgwcli\" -a \"fw\""), "{sent}");
    }

    #[test]
    fn quote_escapes_quotes_and_backslashes() {
        assert_eq!(quote("plain").unwrap(), "\"plain\"");
        assert_eq!(quote("a b\"c\\d'e").unwrap(), "\"a b\\\"c\\\\d'e\"");
    }

    #[test]
    fn quote_rejects_newlines_and_nul() {
        assert!(quote("a\nb").is_err());
        assert!(quote("a\rb").is_err());
        assert!(quote("a\0b").is_err());
    }

    #[test]
    fn set_rejects_unquotable_secret_without_calling_security() {
        let (tmp, kc) = fake();
        assert!(kc.set("fw", "bad\nsecret").is_err());
        assert_eq!(log(&tmp, "argv.log"), "");
    }

    #[test]
    fn failures_are_reported_without_the_secret() {
        let (_tmp, kc) = fake();
        let kc = kc.with_env("FAKE_SECURITY_FAIL", "1");
        for err in [
            kc.get("fw").unwrap_err(),
            kc.set("fw", "sk-secret").unwrap_err(),
            kc.remove("fw").unwrap_err(),
        ] {
            let text = format!("{err:#}");
            assert!(text.contains("fake failure"), "{text}");
            assert!(!text.contains("sk-secret"), "{text}");
        }
    }

    #[test]
    fn missing_program_gives_a_clear_error() {
        let kc = Keychain::with_program("/nonexistent/security");
        let err = format!("{:#}", kc.get("fw").unwrap_err());
        assert!(err.contains("running /nonexistent/security"), "{err}");
    }

    /// Talks to the real macOS keychain. Run by hand: cargo test -- --ignored
    #[test]
    #[ignore]
    #[cfg(target_os = "macos")]
    fn real_keychain_round_trip() {
        let kc = Keychain::new();
        let account = format!("llmgwcli-live-test-{}", std::process::id());
        let secret = "a b\"c\\d'e $x `y`";
        kc.set(&account, secret).unwrap();
        assert_eq!(kc.get(&account).unwrap().as_deref(), Some(secret));
        kc.set(&account, "second").unwrap();
        assert_eq!(kc.get(&account).unwrap().as_deref(), Some("second"));
        kc.remove(&account).unwrap();
        assert_eq!(kc.get(&account).unwrap(), None);
        kc.remove(&account).unwrap();
    }
}
