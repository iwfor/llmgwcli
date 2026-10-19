//! End-to-end tests: run the real binary against a mock HTTP gateway.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::thread;

use tempfile::TempDir;

const LITELLM_MODELS: &str = r#"{"data": [
    {"model_name": "llama-70b", "model_info": {"input_cost_per_token": 0.0000009, "output_cost_per_token": 0.0000018}},
    {"model_name": "mixtral", "model_info": {}}]}"#;
const LITELLM_USAGE: &str = r#"{"metadata": {"total_spend": 12.3456,
    "total_prompt_tokens": 1000, "total_completion_tokens": 500, "total_api_requests": 42}}"#;
const FIREWORKS_SUMMARY: &str = r#"{"lineItems": [{"totalCost": "1.5"}, {"totalCost": "0.25"}]}"#;
const FIREWORKS_TOKENS: &str = r#"{"rows": [{"promptTokens": "300", "completionTokens": "30"}]}"#;

/// One canned HTTP response.
#[derive(Clone)]
struct Reply {
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
}

impl Reply {
    fn ok(body: &'static str) -> Reply {
        Reply { status: "200 OK", headers: vec![], body }
    }

    fn rate_limited(headers: &[(&'static str, &'static str)]) -> Reply {
        Reply { status: "429 Too Many Requests", headers: headers.to_vec(), body: "{}" }
    }
}

/// A tiny HTTP server. `routes` map a path fragment to a JSON body.
/// Requests without `Authorization: Bearer good-key` get a 401.
struct Mock {
    base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Mock {
    fn start(routes: &[(&'static str, &'static str)]) -> Mock {
        let replies: Vec<_> = routes.iter().map(|(path, body)| (*path, vec![Reply::ok(body)])).collect();
        Mock::start_replies(&replies)
    }

    /// Like `start`, but each path gets a list of replies served in order;
    /// the last one repeats.
    fn start_replies(routes: &[(&'static str, Vec<Reply>)]) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&requests);
        let routes = routes.to_vec();
        thread::spawn(move || {
            let mut served = vec![0usize; routes.len()];
            for stream in listener.incoming() {
                let mut stream = stream.unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut authorized = false;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    authorized |= line.to_lowercase().trim() == "authorization: bearer good-key";
                }
                let path = request_line.split(' ').nth(1).unwrap_or("").to_string();
                log.lock().unwrap().push(path.clone());
                let reply = if !authorized {
                    Reply { status: "401 Unauthorized", headers: vec![], body: "{}" }
                } else {
                    match routes.iter().position(|(fragment, _)| path.contains(fragment)) {
                        Some(i) => {
                            let replies = &routes[i].1;
                            let reply = replies[served[i].min(replies.len() - 1)].clone();
                            served[i] += 1;
                            reply
                        }
                        None => Reply { status: "404 Not Found", headers: vec![], body: "{}" },
                    }
                };
                let extra: String = reply.headers.iter().map(|(k, v)| format!("{k}: {v}\r\n")).collect();
                let _ = write!(
                    stream,
                    "HTTP/1.1 {}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.status,
                    reply.body.len(),
                    reply.body
                );
            }
        });
        Mock { base, requests }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

const FAKE_SECURITY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake_security.sh");

struct Env {
    tmp: TempDir,
}

impl Env {
    fn new() -> Env {
        Env { tmp: TempDir::new().unwrap() }
    }

    fn dir(&self) -> std::path::PathBuf {
        self.tmp.path().join("cfg")
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_llmgwcli"));
        cmd.arg("--config-dir").arg(self.dir()).args(args);
        cmd.env_remove("LLMGWCLI_CONFIG_DIR");
        // Never touch the real keychain: use the fake `security` script.
        cmd.env("LLMGWCLI_SECURITY_BIN", FAKE_SECURITY);
        cmd.env("FAKE_SECURITY_DIR", self.keychain_dir());
        cmd
    }

    fn keychain_dir(&self) -> std::path::PathBuf {
        self.tmp.path().join("keychain")
    }

    /// The secret the fake keychain holds for a gateway, if any.
    fn keychain_secret(&self, account: &str) -> Option<String> {
        std::fs::read_to_string(self.keychain_dir().join(format!("llmgwcli.{account}"))).ok()
    }

    fn keychain_log(&self, name: &str) -> String {
        std::fs::read_to_string(self.keychain_dir().join(name)).unwrap_or_default()
    }

    fn credentials(&self) -> String {
        std::fs::read_to_string(self.dir().join("credentials.toml")).unwrap_or_default()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// Runs and asserts success.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(out.status.success(), "{args:?} failed: {}", text(&out.stderr));
        text(&out.stdout)
    }

    /// Runs, asserts failure and returns stderr.
    fn fails(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(!out.status.success(), "{args:?} should fail; stdout: {}", text(&out.stdout));
        text(&out.stderr)
    }

    fn add_litellm(&self, name: &str, mock: &Mock, key: &str, extra: &[&str]) {
        let mut args = vec!["add-provider", name, "--type", "litellm", "--base-url", &mock.base, "--api-key", key];
        args.extend(extra);
        self.ok(&args);
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.dir().join("config.toml")).unwrap()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn add_creates_private_encrypted_config() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "file", "--api-key", "sk-plain"]);
    let dir = env.dir();
    assert_eq!(mode(&dir), 0o700);
    for file in ["config.toml", "credentials.toml", "llmgwcli.key"] {
        assert_eq!(mode(&dir.join(file)), 0o600, "{file}");
    }
    let creds = std::fs::read_to_string(dir.join("credentials.toml")).unwrap();
    assert!(!creds.contains("sk-plain"));
    assert!(env.config().contains("https://api.fireworks.ai/inference/v1"));
}

#[test]
fn config_dir_can_come_from_the_environment() {
    let env = Env::new();
    let dir = env.tmp.path().join("from-env");
    let out = Command::new(env!("CARGO_BIN_EXE_llmgwcli"))
        .env("LLMGWCLI_CONFIG_DIR", &dir)
        .args(["add-provider", "fw", "--type", "fireworks", "--credential-store", "file", "--api-key", "k"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert!(dir.join("config.toml").exists());
}

#[test]
fn add_litellm_requires_base_url() {
    let env = Env::new();
    let err = env.fails(&["add-provider", "lite", "--type", "litellm", "--api-key", "k"]);
    assert!(err.contains("--base-url is required"), "{err}");
}

#[test]
fn add_rejects_duplicates_reserved_and_alias_clashes() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--alias", "f", "--api-key", "k"]);
    let add = |name: &str, alias: Option<&str>| {
        let mut args = vec!["add-provider", name, "--type", "fireworks", "--api-key", "k"];
        if let Some(alias) = alias {
            args.extend(["--alias", alias]);
        }
        env.fails(&args)
    };
    assert!(add("fw", None).contains("already used"));
    assert!(add("f", None).contains("already used"));
    assert!(add("other", Some("fw")).contains("already used"));
    assert!(add("other", Some("f")).contains("already used"));
    assert!(add("all", None).contains("reserved"));
    assert!(add("other", Some("all")).contains("reserved"));
    assert!(add("same", Some("same")).contains("alias must differ"));
}

#[test]
fn edit_changes_fields_and_resolves_alias() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--alias", "f", "--api-key", "k"]);
    env.ok(&["edit-provider", "f", "--base-url", "http://new", "--account-id", "acct", "--api-key", "k2"]);
    let config = env.config();
    assert!(config.contains("base_url = \"http://new\""));
    assert!(config.contains("account_id = \"acct\""));
}

#[test]
fn edit_sets_and_clears_alias() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--api-key", "k"]);
    env.ok(&["edit-provider", "fw", "--alias", "f"]);
    assert!(env.config().contains("alias = \"f\""));
    env.ok(&["edit-provider", "f", "--alias", ""]);
    assert!(!env.config().contains("alias"));
    env.fails(&["edit-provider", "f", "--base-url", "x"]);
}

#[test]
fn edit_rejects_taken_alias_but_allows_keeping_own() {
    let env = Env::new();
    env.ok(&["add-provider", "a", "--type", "fireworks", "--alias", "x", "--api-key", "k"]);
    env.ok(&["add-provider", "b", "--type", "fireworks", "--api-key", "k"]);
    assert!(env.fails(&["edit-provider", "b", "--alias", "x"]).contains("already used"));
    assert!(env.fails(&["edit-provider", "b", "--alias", "all"]).contains("reserved"));
    env.ok(&["edit-provider", "a", "--alias", "x"]);
}

#[test]
fn edit_needs_a_change_and_a_known_gateway() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--api-key", "k"]);
    assert!(env.fails(&["edit-provider", "fw"]).contains("nothing to change"));
    assert!(env.fails(&["edit-provider", "nope", "--base-url", "x"]).contains("unknown gateway"));
}

#[test]
fn remove_deletes_config_and_credentials() {
    let env = Env::new();
    env.ok(&["add-provider", "a", "--type", "fireworks", "--alias", "x", "--credential-store", "file", "--api-key", "k"]);
    env.ok(&["add-provider", "b", "--type", "fireworks", "--credential-store", "file", "--api-key", "k"]);
    env.ok(&["remove-provider", "x"]);
    let config = env.config();
    assert!(!config.contains("providers.a]") && config.contains("providers.b]"));
    let creds = std::fs::read_to_string(env.dir().join("credentials.toml")).unwrap();
    assert_eq!(creds.matches(" = ").count(), 1, "{creds}");
    assert!(env.fails(&["remove-provider", "a"]).contains("unknown gateway"));
}

#[test]
fn list_models_shows_prices_and_filters() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    env.add_litellm("lite", &mock, "good-key", &["--alias", "ll"]);
    let all = env.ok(&["list-models"]);
    assert!(all.starts_with("GATEWAY"), "{all}");
    assert!(all.contains("llama-70b") && all.contains("0.9000") && all.contains("1.8000"));
    assert!(all.contains("mixtral"));
    let filtered = env.ok(&["list-models", "-g", "ll", "--filter", "LLAMA"]);
    assert!(filtered.contains("llama-70b") && !filtered.contains("mixtral"), "{filtered}");
}

#[test]
fn list_models_falls_back_without_prices() {
    let env = Env::new();
    let mock = Mock::start(&[("/models", r#"{"data": [{"id": "gpt-x"}]}"#)]);
    env.add_litellm("lite", &mock, "good-key", &[]);
    let out = env.ok(&["list-models"]);
    assert!(out.contains("gpt-x"), "{out}");
    let row = out.lines().find(|l| l.contains("gpt-x")).unwrap();
    assert!(row.trim_end().ends_with("-  -") || row.trim_end().ends_with('-'), "{row}");
    let requests = mock.requests();
    assert!(requests.iter().any(|p| p.contains("/model/info")));
    assert!(requests.iter().any(|p| p.ends_with("/models")));
}

#[test]
fn list_models_reports_http_errors() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    env.add_litellm("lite", &mock, "bad-key", &[]);
    let err = env.fails(&["list-models"]);
    assert!(err.contains("gateway 'lite'") && err.contains("401"), "{err}");
}

#[test]
fn list_models_without_gateways_fails() {
    let env = Env::new();
    assert!(env.fails(&["list-models"]).contains("no gateways configured"));
    assert!(env.fails(&["list-models", "-g", "nope"]).contains("unknown gateway"));
}

#[test]
fn usage_litellm_prints_month_totals() {
    let env = Env::new();
    let mock = Mock::start(&[("/user/daily/activity", LITELLM_USAGE)]);
    env.add_litellm("lite", &mock, "good-key", &[]);
    let out = env.ok(&["usage", "-g", "lite"]);
    assert!(out.contains("12.3456") && out.contains("1000") && out.contains("500") && out.contains("42"), "{out}");
    let request = mock.requests().pop().unwrap();
    let month = chrono::Utc::now().format("%Y-%m-01").to_string();
    assert!(request.contains(&format!("start_date={month}")), "{request}");
    assert!(request.contains("end_date="), "{request}");
}

#[test]
fn usage_fireworks_uses_account_billing_endpoints() {
    let env = Env::new();
    let mock = Mock::start(&[
        ("/billing/summary", FIREWORKS_SUMMARY),
        ("/billingUsage", FIREWORKS_TOKENS),
    ]);
    env.ok(&[
        "add-provider", "fw", "--type", "fireworks", "--api-key", "good-key",
        "--base-url", &format!("{}/inference/v1", mock.base), "--account-id", "acct",
    ]);
    let out = env.ok(&["usage"]);
    assert!(out.contains("1.7500") && out.contains("300") && out.contains("30"), "{out}");
    let requests = mock.requests();
    assert!(requests.iter().all(|p| p.starts_with("/v1/accounts/acct/")), "{requests:?}");
    assert!(requests.iter().any(|p| p.contains("startTime=") && p.contains("T00%3A00%3A00Z")));
}

#[test]
fn usage_fireworks_without_account_id_fails() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--api-key", "k"]);
    assert!(env.fails(&["usage"]).contains("no account id"));
}

#[test]
fn usage_all_prints_good_gateways_and_fails_on_bad_ones() {
    let env = Env::new();
    let mock = Mock::start(&[("/user/daily/activity", LITELLM_USAGE)]);
    env.add_litellm("good", &mock, "good-key", &[]);
    env.add_litellm("bad", &mock, "bad-key", &[]);
    let out = env.run(&["usage", "-g", "all"]);
    assert!(!out.status.success());
    assert!(text(&out.stdout).contains("good") && text(&out.stdout).contains("12.3456"));
    let err = text(&out.stderr);
    assert!(err.contains("gateway 'bad'") && err.contains("1 gateway(s) failed"), "{err}");
}

#[test]
fn completions_work_without_creating_config() {
    let env = Env::new();
    for shell in ["bash", "zsh"] {
        let out = env.ok(&["completions", shell]);
        assert!(out.contains("llmgwcli"), "{shell}");
        assert!(out.contains("list-models"), "{shell}");
    }
    assert!(!env.dir().exists());
}

#[test]
fn completions_detect_shell_from_environment() {
    let env = Env::new();
    let run = |shell: Option<&str>| {
        let mut cmd = env.cmd(&["completions"]);
        cmd.env_remove("SHELL");
        if let Some(shell) = shell {
            cmd.env("SHELL", shell);
        }
        cmd.output().unwrap()
    };
    assert!(text(&run(Some("/bin/zsh")).stdout).starts_with("#compdef"));
    assert!(text(&run(Some("/bin/bash")).stdout).contains("complete -"));
    let fish = run(Some("/usr/bin/fish"));
    assert!(!fish.status.success() && text(&fish.stderr).contains("unsupported shell"));
    let unset = run(None);
    assert!(!unset.status.success() && text(&unset.stderr).contains("SHELL is not set"));
}

#[test]
fn explicit_shell_overrides_environment() {
    let env = Env::new();
    let out = env.cmd(&["completions", "bash"]).env("SHELL", "/bin/zsh").output().unwrap();
    assert!(!text(&out.stdout).starts_with("#compdef"));
}

// --- credential stores ---

/// Adds a litellm gateway pointing at `mock`, using the given credential store.
fn add_with_store(env: &Env, mock: &Mock, key: &str, store: &str) {
    env.add_litellm("lite", mock, key, &["--credential-store", store]);
}

#[test]
fn default_credential_store_depends_on_the_platform() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--api-key", "good-key"]);
    let expected = if cfg!(target_os = "macos") { "keychain" } else { "file" };
    assert!(env.config().contains(&format!("credential_store = \"{expected}\"")), "{}", env.config());
}

#[test]
fn keychain_store_keeps_the_key_out_of_our_files() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "keychain", "--api-key", "sk-in-keychain"]);
    assert_eq!(env.keychain_secret("fw").as_deref(), Some("sk-in-keychain"));
    assert!(env.config().contains("credential_store = \"keychain\""));
    assert!(!env.credentials().contains("fw"), "{}", env.credentials());
    assert!(!env.keychain_log("argv.log").contains("sk-in-keychain"), "secret leaked into argv");
    assert!(env.keychain_log("stdin.log").contains("sk-in-keychain"));
}

#[test]
fn file_store_flag_skips_the_keychain() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "file", "--api-key", "sk-in-file"]);
    assert!(env.config().contains("credential_store = \"file\""));
    assert!(env.credentials().contains("fw = "));
    assert!(!env.credentials().contains("sk-in-file"));
    assert_eq!(env.keychain_secret("fw"), None);
    assert_eq!(env.keychain_log("argv.log"), "", "security must not be called");
}

#[test]
fn unknown_credential_store_is_rejected() {
    let env = Env::new();
    let err = env.fails(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "vault", "--api-key", "k"]);
    assert!(err.contains("possible values"), "{err}");
}

#[test]
fn queries_read_the_key_from_the_gateways_own_store() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    add_with_store(&env, &mock, "good-key", "keychain");
    assert!(env.ok(&["list-models"]).contains("llama-70b"));
    let file_env = Env::new();
    add_with_store(&file_env, &mock, "good-key", "file");
    assert!(file_env.ok(&["list-models"]).contains("llama-70b"));
}

#[test]
fn gateways_can_mix_credential_stores() {
    let env = Env::new();
    let mock = Mock::start(&[("/user/daily/activity", LITELLM_USAGE)]);
    env.add_litellm("one", &mock, "good-key", &["--credential-store", "keychain"]);
    env.add_litellm("two", &mock, "good-key", &["--credential-store", "file"]);
    let out = env.ok(&["usage"]);
    assert!(out.contains("one") && out.contains("two"), "{out}");
}

#[test]
fn missing_keychain_entry_is_reported() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    add_with_store(&env, &mock, "good-key", "keychain");
    std::fs::remove_file(env.keychain_dir().join("llmgwcli.lite")).unwrap();
    assert!(env.fails(&["list-models"]).contains("no API key for 'lite'"));
}

#[test]
fn legacy_config_without_credential_store_uses_the_file() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    add_with_store(&env, &mock, "good-key", "file");
    let stripped: String = env.config().lines().filter(|l| !l.starts_with("credential_store")).map(|l| format!("{l}\n")).collect();
    std::fs::write(env.dir().join("config.toml"), stripped).unwrap();
    assert!(env.ok(&["list-models"]).contains("llama-70b"));
}

#[test]
fn remove_deletes_the_keychain_entry() {
    let env = Env::new();
    env.ok(&["add-provider", "a", "--type", "fireworks", "--credential-store", "keychain", "--api-key", "k1"]);
    env.ok(&["add-provider", "b", "--type", "fireworks", "--credential-store", "keychain", "--api-key", "k2"]);
    env.ok(&["remove-provider", "a"]);
    assert_eq!(env.keychain_secret("a"), None);
    assert_eq!(env.keychain_secret("b").as_deref(), Some("k2"));
}

#[test]
fn edit_api_key_updates_the_keychain_entry() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    add_with_store(&env, &mock, "bad-key", "keychain");
    assert!(env.fails(&["list-models"]).contains("401"));
    env.ok(&["edit-provider", "lite", "--api-key", "good-key"]);
    assert_eq!(env.keychain_secret("lite").as_deref(), Some("good-key"));
    assert!(env.ok(&["list-models"]).contains("llama-70b"));
}

#[test]
fn edit_credential_store_moves_the_key_both_ways() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    add_with_store(&env, &mock, "good-key", "keychain");

    env.ok(&["edit-provider", "lite", "--credential-store", "file"]);
    assert_eq!(env.keychain_secret("lite"), None);
    assert!(env.credentials().contains("lite = "));
    assert!(env.config().contains("credential_store = \"file\""));
    assert!(env.ok(&["list-models"]).contains("llama-70b"));

    env.ok(&["edit-provider", "lite", "--credential-store", "keychain"]);
    assert_eq!(env.keychain_secret("lite").as_deref(), Some("good-key"));
    assert!(!env.credentials().contains("lite = "));
    assert!(env.ok(&["list-models"]).contains("llama-70b"));
}

#[test]
fn edit_credential_store_with_new_key_stores_it_in_the_new_place_only() {
    let env = Env::new();
    let mock = Mock::start(&[("/model/info", LITELLM_MODELS)]);
    add_with_store(&env, &mock, "bad-key", "keychain");
    env.ok(&["edit-provider", "lite", "--credential-store", "file", "--api-key", "good-key"]);
    assert_eq!(env.keychain_secret("lite"), None);
    assert!(env.ok(&["list-models"]).contains("llama-70b"));
}

#[test]
fn edit_to_the_current_store_changes_nothing() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "keychain", "--api-key", "k"]);
    env.ok(&["edit-provider", "fw", "--credential-store", "keychain"]);
    assert_eq!(env.keychain_secret("fw").as_deref(), Some("k"));
}

#[test]
fn failed_keychain_write_does_not_add_the_gateway() {
    let env = Env::new();
    let out = env
        .cmd(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "keychain", "--api-key", "k"])
        .env("FAKE_SECURITY_FAIL", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("security failed"), "{}", text(&out.stderr));
    let config = std::fs::read_to_string(env.dir().join("config.toml")).unwrap_or_default();
    assert!(!config.contains("fw"), "{config}");
}

#[test]
fn failed_move_keeps_the_old_key_and_config() {
    let env = Env::new();
    env.ok(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "file", "--api-key", "k"]);
    let out = env
        .cmd(&["edit-provider", "fw", "--credential-store", "keychain"])
        .env("FAKE_SECURITY_FAIL", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(env.config().contains("credential_store = \"file\""));
    assert!(env.credentials().contains("fw = "));
}

#[test]
fn missing_security_command_gives_a_clear_error() {
    let env = Env::new();
    let out = env
        .cmd(&["add-provider", "fw", "--type", "fireworks", "--credential-store", "keychain", "--api-key", "k"])
        .env("LLMGWCLI_SECURITY_BIN", "/nonexistent/security")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(text(&out.stderr).contains("running /nonexistent/security"), "{}", text(&out.stderr));
}

// --- rate limiting (429) ---

fn rate_limit_mock(first: Reply) -> Mock {
    Mock::start_replies(&[("/model/info", vec![first, Reply::ok(LITELLM_MODELS)])])
}

#[test]
fn waits_for_retry_after_then_succeeds() {
    let env = Env::new();
    let mock = rate_limit_mock(Reply::rate_limited(&[("Retry-After", "1")]));
    env.add_litellm("lite", &mock, "good-key", &["--credential-store", "file"]);
    let started = std::time::Instant::now();
    let out = env.run(&["list-models"]);
    let waited = started.elapsed();
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(text(&out.stdout).contains("llama-70b"));
    assert!(waited >= std::time::Duration::from_secs(1), "only waited {waited:?}");
    assert!(text(&out.stderr).contains("rate limited"), "{}", text(&out.stderr));
    assert_eq!(mock.requests().len(), 2, "{:?}", mock.requests());
}

#[test]
fn rate_limit_without_a_header_backs_off_and_retries() {
    let env = Env::new();
    let mock = rate_limit_mock(Reply::rate_limited(&[]));
    env.add_litellm("lite", &mock, "good-key", &["--credential-store", "file"]);
    let started = std::time::Instant::now();
    assert!(env.ok(&["list-models"]).contains("llama-70b"));
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
}

#[test]
fn wait_longer_than_300_seconds_fails_at_once_without_retrying() {
    let env = Env::new();
    let mock = rate_limit_mock(Reply::rate_limited(&[("Retry-After", "301")]));
    env.add_litellm("lite", &mock, "good-key", &["--credential-store", "file"]);
    let started = std::time::Instant::now();
    let err = env.fails(&["list-models"]);
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "should not wait");
    assert!(err.contains("301s") && err.contains("exceeds our 300s timeout"), "{err}");
    assert!(err.contains("gateway 'lite'"), "{err}");
    // No retry, and no silent fallback to /models either.
    assert_eq!(mock.requests().len(), 1, "{:?}", mock.requests());
}

#[test]
fn usage_also_honours_rate_limits() {
    let env = Env::new();
    let mock = Mock::start_replies(&[(
        "/user/daily/activity",
        vec![Reply::rate_limited(&[("Retry-After", "1")]), Reply::ok(LITELLM_USAGE)],
    )]);
    env.add_litellm("lite", &mock, "good-key", &["--credential-store", "file"]);
    assert!(env.ok(&["usage"]).contains("12.3456"));
    assert_eq!(mock.requests().len(), 2);
}

#[test]
fn one_rate_limited_gateway_does_not_block_the_others() {
    let env = Env::new();
    let slow = Mock::start_replies(&[(
        "/user/daily/activity",
        vec![Reply::rate_limited(&[("Retry-After", "301")])],
    )]);
    let fast = Mock::start(&[("/user/daily/activity", LITELLM_USAGE)]);
    env.add_litellm("slow", &slow, "good-key", &["--credential-store", "file"]);
    env.add_litellm("fast", &fast, "good-key", &["--credential-store", "file"]);
    let out = env.run(&["usage"]);
    assert!(!out.status.success());
    assert!(text(&out.stdout).contains("fast") && text(&out.stdout).contains("12.3456"));
    assert!(text(&out.stderr).contains("gateway 'slow'") && text(&out.stderr).contains("exceeds"));
}
