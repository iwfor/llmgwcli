mod crypto;
mod keychain;
mod provider;
mod store;

use std::path::PathBuf;
use std::thread;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Months, NaiveDate, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use provider::{Model, ProviderKind, Usage};
use store::{ALL_GATEWAYS, Config, CredentialStore, ProviderConfig, Store};

#[derive(Parser)]
#[command(version, about = "Client for LLM gateways")]
struct Cli {
    /// Config directory (default: ~/.config/llmgwcli)
    #[arg(long, global = true, env = "LLMGWCLI_CONFIG_DIR")]
    config_dir: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

/// Picks which gateway(s) a query command talks to.
#[derive(Args)]
struct GatewayArg {
    /// Gateway name or alias; 'all' queries every gateway in parallel
    #[arg(short, long, default_value = ALL_GATEWAYS)]
    gateway: String,
}

/// How a query command prints its result.
#[derive(Args)]
struct OutputArg {
    /// Print JSON instead of a table. Errors still go to stderr
    #[arg(long)]
    json: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Add a gateway
    AddProvider {
        name: String,
        #[arg(long = "type", value_enum)]
        kind: ProviderKind,
        /// Short name for -g, e.g. 'fw'
        #[arg(long)]
        alias: Option<String>,
        /// Gateway URL (required for litellm)
        #[arg(long)]
        base_url: Option<String>,
        /// Fireworks account id (needed for usage)
        #[arg(long)]
        account_id: Option<String>,
        /// Where to keep the API key (default: keychain on macOS, file elsewhere)
        #[arg(long, value_enum)]
        credential_store: Option<CredentialStore>,
        /// API key (prompted if omitted)
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Change a gateway's alias, URL, account id, credential store or API key
    EditProvider {
        /// Gateway name or alias
        name: String,
        /// New alias (empty string removes it)
        #[arg(long)]
        alias: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long)]
        account_id: Option<String>,
        /// Move the API key to this store (the key is kept)
        #[arg(long, value_enum)]
        credential_store: Option<CredentialStore>,
        /// New API key (prompted if no value is given)
        #[arg(long, num_args = 0..=1)]
        api_key: Option<Option<String>>,
    },
    /// Remove a gateway and its credentials
    RemoveProvider {
        /// Gateway name or alias
        name: String,
    },
    /// List models with prices (USD per 1M tokens)
    ListModels {
        #[command(flatten)]
        gateway: GatewayArg,
        #[command(flatten)]
        output: OutputArg,
        /// Case-insensitive substring match on the model id
        #[arg(long)]
        filter: Option<String>,
    },
    /// Show this month's usage (UTC)
    Usage {
        #[command(flatten)]
        gateway: GatewayArg,
        #[command(flatten)]
        output: OutputArg,
    },
    /// Print a shell completion script
    Completions {
        /// Shell (default: detected from $SHELL)
        #[arg(value_enum)]
        shell: Option<ShellKind>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum ShellKind {
    Bash,
    Zsh,
}

impl ShellKind {
    /// Detects the running shell from the SHELL environment variable.
    fn detect() -> Result<Self> {
        let path = std::env::var("SHELL").context("SHELL is not set; pass bash or zsh")?;
        Self::from_path(&path)
    }

    fn from_path(path: &str) -> Result<Self> {
        match path.rsplit('/').next() {
            Some("bash") => Ok(Self::Bash),
            Some("zsh") => Ok(Self::Zsh),
            _ => bail!("unsupported shell '{path}'; pass bash or zsh"),
        }
    }

    fn generate(self) {
        let shell = match self {
            Self::Bash => Shell::Bash,
            Self::Zsh => Shell::Zsh,
        };
        clap_complete::generate(shell, &mut Cli::command(), "llmgwcli", &mut std::io::stdout());
    }
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    // Needs no config, so do not create the config dir for it.
    if let Command::Completions { shell } = cli.command {
        shell.map_or_else(ShellKind::detect, Ok)?.generate();
        return Ok(());
    }
    let dir = match cli.config_dir {
        Some(dir) => dir,
        None => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?)
            .join(".config/llmgwcli"),
    };
    let store = Store::open(dir)?;
    match cli.command {
        Command::AddProvider { name, kind, alias, base_url, account_id, credential_store, api_key } => {
            let mut config = store.load_config()?;
            config.ensure_free(&name, None)?;
            if let Some(alias) = &alias {
                config.ensure_free(alias, None)?;
                if *alias == name {
                    bail!("alias must differ from the name");
                }
            }
            let base_url = base_url
                .or_else(|| kind.default_base_url().map(String::from))
                .context("--base-url is required for this provider type")?;
            let credential_store = credential_store.unwrap_or_else(CredentialStore::platform_default);
            let api_key = api_key.map_or_else(|| prompt_key(&name), Ok)?;
            store.set_api_key(&name, &api_key, credential_store)?;
            config.providers.insert(
                name.clone(),
                ProviderConfig { kind, base_url, alias, account_id, credential_store },
            );
            store.save_config(&config)?;
            println!("Added gateway '{name}'.");
        }
        Command::EditProvider { name, alias, base_url, account_id, credential_store, api_key } => {
            if [alias.is_some(), base_url.is_some(), account_id.is_some(), credential_store.is_some(), api_key.is_some()]
                .iter()
                .all(|changed| !changed)
            {
                bail!("nothing to change; pass --alias, --base-url, --account-id, --credential-store or --api-key");
            }
            let mut config = store.load_config()?;
            let name = config.resolve(&name)?;
            if let Some(alias) = alias.as_deref().filter(|a| !a.is_empty()) {
                config.ensure_free(alias, Some(&name))?;
                if alias == name {
                    bail!("alias must differ from the name");
                }
            }
            let gateway = config.providers.get_mut(&name).expect("resolved name exists");
            if let Some(alias) = alias {
                gateway.alias = Some(alias).filter(|a| !a.is_empty());
            }
            if let Some(url) = base_url {
                gateway.base_url = url;
            }
            if let Some(id) = account_id {
                gateway.account_id = Some(id);
            }
            let (old_store, new_store) = (gateway.credential_store, credential_store.unwrap_or(gateway.credential_store));
            match api_key {
                Some(key) => {
                    store.set_api_key(&name, &key.map_or_else(|| prompt_key(&name), Ok)?, new_store)?;
                    if new_store != old_store {
                        store.remove_api_key(&name, old_store)?;
                    }
                }
                None if new_store != old_store => store.move_api_key(&name, old_store, new_store)?,
                None => {}
            }
            gateway.credential_store = new_store;
            store.save_config(&config)?;
            println!("Updated gateway '{name}'.");
        }
        Command::RemoveProvider { name } => {
            let mut config = store.load_config()?;
            let name = config.resolve(&name)?;
            let removed = config.providers.remove(&name).expect("resolved name exists");
            store.remove_api_key(&name, removed.credential_store)?;
            store.save_config(&config)?;
            println!("Removed gateway '{name}'.");
        }
        Command::ListModels { gateway, output, filter } => {
            let targets = targets(&store, &gateway.gateway)?;
            let (results, failed) = gather(&targets, |t| provider::list_models(&t.cfg, &t.api_key));
            if output.json {
                print_json(&models_json(results, filter.as_deref()))?;
            } else {
                let rows = model_rows(results, filter.as_deref());
                print_table(&["GATEWAY", "MODEL", "INPUT/1M", "OUTPUT/1M"], rows, 2);
            }
            finish(failed)?;
        }
        Command::Completions { .. } => unreachable!("handled before the store opens"),
        Command::Usage { gateway, output } => {
            let targets = targets(&store, &gateway.gateway)?;
            let (start, end) = current_month();
            let (results, failed) =
                gather(&targets, |t| provider::usage(&t.cfg, &t.api_key, start, end));
            if output.json {
                print_json(&usage_json(&results, start, end))?;
            } else {
                let rows = results.iter().map(|(t, u)| usage_row(t, u)).collect();
                println!("Usage {start} to {end} (end exclusive, UTC)");
                print_table(&["GATEWAY", "COST (USD)", "PROMPT TOK", "COMPLETION TOK", "REQUESTS"], rows, 1);
            }
            finish(failed)?;
        }
    }
    Ok(())
}

/// A gateway to query, with its decrypted API key.
struct Target {
    name: String,
    cfg: ProviderConfig,
    api_key: String,
}

fn targets(store: &Store, gateway: &str) -> Result<Vec<Target>> {
    let config: Config = store.load_config()?;
    config
        .select(gateway)?
        .into_iter()
        .map(|name| {
            let cfg = config.providers[&name].clone();
            let api_key = store
                .api_key(&name, cfg.credential_store)?
                .with_context(|| format!("no API key for '{name}'"))?;
            Ok(Target { name, cfg, api_key })
        })
        .collect()
}

/// Runs `f` for every target in parallel. Failures are reported on stderr;
/// returns the successes (in target order) and the failure count.
fn gather<T: Send>(
    targets: &[Target],
    f: impl Fn(&Target) -> Result<T> + Sync,
) -> (Vec<(&Target, T)>, usize) {
    let results: Vec<_> = thread::scope(|scope| {
        let handles: Vec<_> = targets.iter().map(|t| (t, scope.spawn(|| f(t)))).collect();
        handles
            .into_iter()
            .map(|(t, h)| (t, h.join().unwrap_or_else(|_| Err(anyhow::anyhow!("query panicked")))))
            .collect()
    });
    let mut ok = Vec::new();
    let mut failed = 0;
    for (t, result) in results {
        match result {
            Ok(value) => ok.push((t, value)),
            Err(e) => {
                failed += 1;
                eprintln!("error: gateway '{}': {e:#}", t.name);
            }
        }
    }
    (ok, failed)
}

fn finish(failed: usize) -> Result<()> {
    if failed > 0 {
        bail!("{failed} gateway(s) failed");
    }
    Ok(())
}

/// First day of this month and of next month (UTC).
fn current_month() -> (NaiveDate, NaiveDate) {
    month_window(Utc::now().date_naive())
}

/// First day of the month containing `today`, and of the following month.
fn month_window(today: NaiveDate) -> (NaiveDate, NaiveDate) {
    let start = today.with_day0(0).expect("day 1 exists");
    (start, start + Months::new(1))
}

/// Shows an unknown value as "-".
fn or_dash<T>(value: Option<T>, show: impl Fn(T) -> String) -> String {
    value.map_or_else(|| "-".to_string(), show)
}

/// Every model whose id contains `filter` (case-insensitive), with its gateway.
fn filter_models<'a>(results: Vec<(&'a Target, Vec<Model>)>, filter: Option<&str>) -> Vec<(&'a Target, Model)> {
    let filter = filter.map(str::to_lowercase);
    results
        .into_iter()
        .flat_map(|(t, models)| models.into_iter().map(move |m| (t, m)))
        .filter(|(_, m)| filter.as_ref().is_none_or(|f| m.id.to_lowercase().contains(f)))
        .collect()
}

fn model_rows(results: Vec<(&Target, Vec<Model>)>, filter: Option<&str>) -> Vec<Vec<String>> {
    let price = |p: Option<f64>| or_dash(p, |p| format!("{p:.4}"));
    filter_models(results, filter)
        .into_iter()
        .map(|(t, m)| vec![t.name.clone(), m.id, price(m.input_price), price(m.output_price)])
        .collect()
}

/// Prices are USD per 1M tokens; rounded to drop floating point noise from
/// converting per-token prices.
fn round_price(price: f64) -> f64 {
    (price * 1e6).round() / 1e6
}

fn models_json(results: Vec<(&Target, Vec<Model>)>, filter: Option<&str>) -> serde_json::Value {
    filter_models(results, filter)
        .into_iter()
        .map(|(t, m)| {
            serde_json::json!({
                "gateway": t.name,
                "model": m.id,
                "input_per_million": m.input_price.map(round_price),
                "output_per_million": m.output_price.map(round_price),
            })
        })
        .collect()
}

/// Unknown values are `null`.
fn usage_json(results: &[(&Target, Usage)], start: NaiveDate, end: NaiveDate) -> serde_json::Value {
    let usage: Vec<_> = results
        .iter()
        .map(|(t, u)| {
            serde_json::json!({
                "gateway": t.name,
                "cost_usd": u.cost,
                "prompt_tokens": u.prompt_tokens,
                "completion_tokens": u.completion_tokens,
                "requests": u.requests,
            })
        })
        .collect();
    serde_json::json!({ "start": start.to_string(), "end": end.to_string(), "usage": usage })
}

fn print_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn usage_row(target: &Target, usage: &Usage) -> Vec<String> {
    let count = |n: Option<u64>| or_dash(n, |n| n.to_string());
    vec![
        target.name.clone(),
        or_dash(usage.cost, |c| format!("{c:.4}")),
        count(usage.prompt_tokens),
        count(usage.completion_tokens),
        count(usage.requests),
    ]
}

fn prompt_key(name: &str) -> Result<String> {
    let key = rpassword::prompt_password(format!("API key for '{name}': "))?;
    if key.trim().is_empty() {
        bail!("API key cannot be empty");
    }
    Ok(key.trim().to_string())
}

fn print_table(header: &[&str], rows: Vec<Vec<String>>, left_cols: usize) {
    println!("{}", format_table(header, rows, left_cols));
}

/// Formats a table; the first `left_cols` columns are left-aligned, the rest right-aligned.
fn format_table(header: &[&str], rows: Vec<Vec<String>>, left_cols: usize) -> String {
    let mut table = vec![header.iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    table.extend(rows);
    let widths: Vec<usize> = (0..header.len())
        .map(|c| table.iter().map(|r| r[c].len()).max().unwrap_or(0))
        .collect();
    let lines: Vec<String> = table
        .iter()
        .map(|row| {
            let cells: Vec<String> = row
                .iter()
                .zip(&widths)
                .enumerate()
                .map(|(i, (cell, w))| {
                    if i < left_cols { format!("{cell:<w$}") } else { format!("{cell:>w$}") }
                })
                .collect();
            cells.join("  ").trim_end().to_string()
        })
        .collect();
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn target(name: &str) -> Target {
        Target {
            name: name.into(),
            cfg: ProviderConfig {
                kind: ProviderKind::Litellm,
                base_url: "http://localhost".into(),
                alias: None,
                account_id: None,
                credential_store: CredentialStore::File,
            },
            api_key: "k".into(),
        }
    }

    fn model(id: &str, input: Option<f64>, output: Option<f64>) -> Model {
        Model { id: id.into(), input_price: input, output_price: output }
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("llmgwcli").chain(args.iter().copied()))
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn gateway_defaults_to_all() {
        let Command::Usage { gateway, .. } = parse(&["usage"]).unwrap().command else { panic!() };
        assert_eq!(gateway.gateway, "all");
    }

    #[test]
    fn gateway_short_and_long_flags() {
        for args in [["usage", "-g", "fw"], ["usage", "--gateway", "fw"]] {
            let Command::Usage { gateway, .. } = parse(&args).unwrap().command else { panic!() };
            assert_eq!(gateway.gateway, "fw");
        }
    }

    #[test]
    fn list_models_takes_filter_and_gateway() {
        let cli = parse(&["list-models", "-g", "fw", "--filter", "llama"]).unwrap();
        let Command::ListModels { gateway, filter, .. } = cli.command else { panic!() };
        assert_eq!(gateway.gateway, "fw");
        assert_eq!(filter.as_deref(), Some("llama"));
    }

    #[test]
    fn edit_api_key_flag_with_and_without_value() {
        let Command::EditProvider { api_key, .. } =
            parse(&["edit-provider", "fw", "--api-key"]).unwrap().command else { panic!() };
        assert_eq!(api_key, Some(None));
        let Command::EditProvider { api_key, .. } =
            parse(&["edit-provider", "fw", "--api-key", "abc"]).unwrap().command else { panic!() };
        assert_eq!(api_key, Some(Some("abc".to_string())));
    }

    #[test]
    fn add_provider_requires_a_known_type() {
        assert!(parse(&["add-provider", "x"]).is_err());
        assert!(parse(&["add-provider", "x", "--type", "openai"]).is_err());
        assert!(parse(&["add-provider", "x", "--type", "fireworks"]).is_ok());
    }

    #[test]
    fn completions_shell_is_bash_or_zsh_only() {
        assert!(parse(&["completions"]).is_ok());
        assert!(parse(&["completions", "bash"]).is_ok());
        assert!(parse(&["completions", "zsh"]).is_ok());
        assert!(parse(&["completions", "fish"]).is_err());
    }

    #[test]
    fn shell_detected_from_path() {
        assert!(matches!(ShellKind::from_path("/bin/zsh"), Ok(ShellKind::Zsh)));
        assert!(matches!(ShellKind::from_path("/usr/local/bin/bash"), Ok(ShellKind::Bash)));
        assert!(matches!(ShellKind::from_path("zsh"), Ok(ShellKind::Zsh)));
        let err = ShellKind::from_path("/bin/fish").err().unwrap();
        assert!(err.to_string().contains("unsupported shell"));
        assert!(ShellKind::from_path("").is_err());
    }

    #[test]
    fn month_window_mid_month() {
        assert_eq!(month_window(date(2026, 10, 19)), (date(2026, 10, 1), date(2026, 11, 1)));
    }

    #[test]
    fn month_window_first_and_last_days() {
        assert_eq!(month_window(date(2026, 2, 1)), (date(2026, 2, 1), date(2026, 3, 1)));
        assert_eq!(month_window(date(2024, 2, 29)), (date(2024, 2, 1), date(2024, 3, 1)));
    }

    #[test]
    fn month_window_rolls_over_the_year() {
        assert_eq!(month_window(date(2026, 12, 31)), (date(2026, 12, 1), date(2027, 1, 1)));
    }

    #[test]
    fn or_dash_shows_dash_for_none() {
        assert_eq!(or_dash(None::<u64>, |n| n.to_string()), "-");
        assert_eq!(or_dash(Some(3u64), |n| n.to_string()), "3");
    }

    /// (start, end) byte offsets of each whitespace-separated cell in a line.
    fn spans(line: &str) -> Vec<(usize, usize)> {
        let mut spans = Vec::new();
        let mut start = None;
        for (i, c) in line.char_indices().chain(std::iter::once((line.len(), ' '))) {
            match (c.is_whitespace(), start) {
                (false, None) => start = Some(i),
                (true, Some(s)) => {
                    spans.push((s, i));
                    start = None;
                }
                _ => {}
            }
        }
        spans
    }

    fn sample_table() -> String {
        let rows = vec![
            vec!["fw".into(), "llama".into(), "0.2000".into()],
            vec!["litellm".into(), "x".into(), "10.0000".into()],
        ];
        format_table(&["GATEWAY", "MODEL", "PRICE"], rows, 2)
    }

    #[test]
    fn table_has_header_then_rows_in_order() {
        let table = sample_table();
        let lines: Vec<Vec<&str>> = table.lines().map(|l| l.split_whitespace().collect()).collect();
        assert_eq!(
            lines,
            [
                vec!["GATEWAY", "MODEL", "PRICE"],
                vec!["fw", "llama", "0.2000"],
                vec!["litellm", "x", "10.0000"],
            ]
        );
    }

    #[test]
    fn table_left_columns_start_together_and_right_columns_end_together() {
        let table = sample_table();
        let all: Vec<_> = table.lines().map(spans).collect();
        for col in 0..2 {
            assert!(all.iter().all(|s| s[col].0 == all[0][col].0), "column {col} not left-aligned");
        }
        assert!(all.iter().all(|s| s[2].1 == all[0][2].1), "last column not right-aligned");
    }

    #[test]
    fn table_columns_do_not_overlap() {
        let table = sample_table();
        for line in table.lines() {
            let s = spans(line);
            assert!(s.windows(2).all(|w| w[0].1 < w[1].0), "cells touch: {line:?}");
        }
    }

    #[test]
    fn table_with_no_rows_is_just_the_header() {
        assert_eq!(format_table(&["A", "B"], vec![], 1), "A  B");
    }

    #[test]
    fn model_rows_formats_prices() {
        let t = target("fw");
        let rows = model_rows(vec![(&t, vec![model("m", Some(0.9), None)])], None);
        assert_eq!(rows, [["fw", "m", "0.9000", "-"]]);
    }

    #[test]
    fn model_rows_filter_is_case_insensitive_substring() {
        let (a, b) = (target("a"), target("b"));
        let results = vec![
            (&a, vec![model("Llama-70B", None, None), model("mixtral", None, None)]),
            (&b, vec![model("llama-8b", None, None)]),
        ];
        let rows = model_rows(results, Some("LLAMA"));
        let ids: Vec<_> = rows.iter().map(|r| (r[0].as_str(), r[1].as_str())).collect();
        assert_eq!(ids, [("a", "Llama-70B"), ("b", "llama-8b")]);
    }

    #[test]
    fn model_rows_filter_without_match_is_empty() {
        let t = target("a");
        assert!(model_rows(vec![(&t, vec![model("x", None, None)])], Some("zzz")).is_empty());
    }

    #[test]
    fn usage_row_formats_values_and_unknowns() {
        let t = target("fw");
        let usage = Usage { cost: Some(1.5), prompt_tokens: Some(10), completion_tokens: None, requests: None };
        assert_eq!(usage_row(&t, &usage), ["fw", "1.5000", "10", "-", "-"]);
    }

    #[test]
    fn gather_keeps_order_and_counts_failures() {
        let targets = [target("a"), target("b"), target("c")];
        let (ok, failed) = gather(&targets, |t| {
            if t.name == "b" { anyhow::bail!("boom") } else { Ok(t.name.to_uppercase()) }
        });
        let names: Vec<_> = ok.iter().map(|(t, v)| (t.name.as_str(), v.as_str())).collect();
        assert_eq!(names, [("a", "A"), ("c", "C")]);
        assert_eq!(failed, 1);
    }

    #[test]
    fn gather_survives_a_panicking_query() {
        let targets = [target("a"), target("b")];
        let (ok, failed) = gather(&targets, |t| {
            if t.name == "a" { panic!("oops") }
            Ok(1)
        });
        assert_eq!(ok.len(), 1);
        assert_eq!(failed, 1);
    }

    #[test]
    fn gather_runs_queries_in_parallel() {
        use std::sync::Barrier;
        // Each query waits for the other; this deadlocks if they run one at a time.
        let barrier = Barrier::new(2);
        let targets = [target("a"), target("b")];
        let (ok, failed) = gather(&targets, |_| {
            barrier.wait();
            Ok(())
        });
        assert_eq!((ok.len(), failed), (2, 0));
    }

    #[test]
    fn finish_fails_only_when_something_failed() {
        assert!(finish(0).is_ok());
        assert!(finish(2).unwrap_err().to_string().contains("2 gateway(s) failed"));
    }

    fn store_with_gateways() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let store = Store::open(tmp.path().join("cfg")).unwrap();
        let mut config = Config::default();
        for (name, alias) in [("lite", Some("ll")), ("fireworks", None)] {
            config.providers.insert(
                name.into(),
                ProviderConfig {
                    kind: ProviderKind::Litellm,
                    base_url: "http://x".into(),
                    alias: alias.map(String::from),
                    account_id: None,
                    credential_store: CredentialStore::File,
                },
            );
            store.set_api_key(name, &format!("key-{name}"), CredentialStore::File).unwrap();
        }
        store.save_config(&config).unwrap();
        (tmp, store)
    }

    #[test]
    fn targets_all_loads_decrypted_keys() {
        let (_tmp, store) = store_with_gateways();
        let all = targets(&store, "all").unwrap();
        let pairs: Vec<_> = all.iter().map(|t| (t.name.as_str(), t.api_key.as_str())).collect();
        assert_eq!(pairs, [("fireworks", "key-fireworks"), ("lite", "key-lite")]);
    }

    #[test]
    fn targets_resolves_alias() {
        let (_tmp, store) = store_with_gateways();
        let one = targets(&store, "ll").unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "lite");
    }

    #[test]
    fn targets_fails_for_unknown_gateway_or_missing_key() {
        let (_tmp, store) = store_with_gateways();
        assert!(targets(&store, "nope").is_err());
        store.remove_api_key("lite", CredentialStore::File).unwrap();
        let err = targets(&store, "lite").err().unwrap();
        assert!(err.to_string().contains("no API key for 'lite'"));
    }
    // --- --json output ---

    #[test]
    fn json_flag_defaults_to_off_and_can_be_set() {
        let Command::Usage { output, .. } = parse(&["usage"]).unwrap().command else { panic!() };
        assert!(!output.json);
        let Command::Usage { output, .. } = parse(&["usage", "--json"]).unwrap().command else { panic!() };
        assert!(output.json);
        let Command::ListModels { output, .. } =
            parse(&["list-models", "--json", "-g", "fw"]).unwrap().command else { panic!() };
        assert!(output.json);
    }

    #[test]
    fn models_json_has_one_object_per_model_with_null_for_unknown_prices() {
        let t = target("fw");
        let results = vec![(&t, vec![model("a", Some(0.9), Some(1.8)), model("b", None, None)])];
        assert_eq!(
            models_json(results, None),
            serde_json::json!([
                {"gateway": "fw", "model": "a", "input_per_million": 0.9, "output_per_million": 1.8},
                {"gateway": "fw", "model": "b", "input_per_million": null, "output_per_million": null},
            ])
        );
    }

    #[test]
    fn models_json_applies_the_filter() {
        let t = target("fw");
        let results = vec![(&t, vec![model("Llama", None, None), model("mixtral", None, None)])];
        let json = models_json(results, Some("llama"));
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["model"], "Llama");
    }

    #[test]
    fn models_json_is_an_empty_array_when_nothing_matches() {
        let t = target("fw");
        assert_eq!(models_json(vec![(&t, vec![model("x", None, None)])], Some("zzz")), serde_json::json!([]));
        assert_eq!(models_json(vec![], None), serde_json::json!([]));
    }

    #[test]
    fn models_json_prices_have_no_floating_point_noise() {
        let t = target("fw");
        // 0.0000009 USD per token * 1e6 is 0.8999999999999999 in floating point.
        let price = 0.0000009_f64 * 1e6;
        let json = models_json(vec![(&t, vec![model("a", Some(price), None)])], None);
        assert_eq!(json[0]["input_per_million"], 0.9);
    }

    #[test]
    fn usage_json_has_the_period_and_one_object_per_gateway() {
        let (a, b) = (target("a"), target("b"));
        let full = Usage { cost: Some(12.5), prompt_tokens: Some(10), completion_tokens: Some(5), requests: Some(2) };
        let results = vec![(&a, full), (&b, Usage::default())];
        assert_eq!(
            usage_json(&results, date(2026, 10, 1), date(2026, 11, 1)),
            serde_json::json!({
                "start": "2026-10-01",
                "end": "2026-11-01",
                "usage": [
                    {"gateway": "a", "cost_usd": 12.5, "prompt_tokens": 10, "completion_tokens": 5, "requests": 2},
                    {"gateway": "b", "cost_usd": null, "prompt_tokens": null, "completion_tokens": null, "requests": null},
                ],
            })
        );
    }

    #[test]
    fn usage_json_with_no_results_has_an_empty_list() {
        let json = usage_json(&[], date(2026, 10, 1), date(2026, 11, 1));
        assert_eq!(json["usage"], serde_json::json!([]));
    }
    #[test]
    fn version_flag_exits_with_the_version_text() {
        for flag in ["--version", "-V"] {
            let err = parse(&[flag]).err().expect("version exits early");
            assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
            assert!(err.to_string().contains(env!("CARGO_PKG_VERSION")), "{err}");
        }
    }
}
