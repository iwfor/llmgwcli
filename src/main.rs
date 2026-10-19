mod crypto;
mod provider;
mod store;

use std::path::PathBuf;
use std::thread;

use anyhow::{Context, Result, bail};
use chrono::{Datelike, Months, NaiveDate, Utc};
use clap::{Args, CommandFactory, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;

use provider::{ProviderKind, Usage};
use store::{ALL_GATEWAYS, Config, ProviderConfig, Store};

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
        /// API key (prompted if omitted)
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Change a gateway's alias, URL, account id or API key
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
        /// Case-insensitive substring match on the model id
        #[arg(long)]
        filter: Option<String>,
    },
    /// Show this month's usage (UTC)
    Usage {
        #[command(flatten)]
        gateway: GatewayArg,
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
        Command::AddProvider { name, kind, alias, base_url, account_id, api_key } => {
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
            let api_key = api_key.map_or_else(|| prompt_key(&name), Ok)?;
            store.set_api_key(&name, &api_key)?;
            config
                .providers
                .insert(name.clone(), ProviderConfig { kind, base_url, alias, account_id });
            store.save_config(&config)?;
            println!("Added gateway '{name}'.");
        }
        Command::EditProvider { name, alias, base_url, account_id, api_key } => {
            if alias.is_none() && base_url.is_none() && account_id.is_none() && api_key.is_none() {
                bail!("nothing to change; pass --alias, --base-url, --account-id or --api-key");
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
            if let Some(key) = api_key {
                store.set_api_key(&name, &key.map_or_else(|| prompt_key(&name), Ok)?)?;
            }
            store.save_config(&config)?;
            println!("Updated gateway '{name}'.");
        }
        Command::RemoveProvider { name } => {
            let mut config = store.load_config()?;
            let name = config.resolve(&name)?;
            config.providers.remove(&name);
            store.remove_api_key(&name)?;
            store.save_config(&config)?;
            println!("Removed gateway '{name}'.");
        }
        Command::ListModels { gateway, filter } => {
            let targets = targets(&store, &gateway.gateway)?;
            let filter = filter.map(|f| f.to_lowercase());
            let (results, failed) = gather(&targets, |t| provider::list_models(&t.cfg, &t.api_key));
            let price = |p: Option<f64>| p.map_or("-".to_string(), |p| format!("{p:.4}"));
            let rows = results
                .into_iter()
                .flat_map(|(t, models)| models.into_iter().map(move |m| (t, m)))
                .filter(|(_, m)| filter.as_ref().is_none_or(|f| m.id.to_lowercase().contains(f)))
                .map(|(t, m)| vec![t.name.clone(), m.id, price(m.input_price), price(m.output_price)])
                .collect();
            print_table(&["GATEWAY", "MODEL", "INPUT/1M", "OUTPUT/1M"], rows, 2);
            finish(failed)?;
        }
        Command::Completions { .. } => unreachable!("handled before the store opens"),
        Command::Usage { gateway } => {
            let targets = targets(&store, &gateway.gateway)?;
            let (start, end) = current_month();
            let (results, failed) =
                gather(&targets, |t| provider::usage(&t.cfg, &t.api_key, start, end));
            let count = |n: Option<u64>| n.map_or("-".to_string(), |n| n.to_string());
            let rows = results
                .into_iter()
                .map(|(t, u): (&Target, Usage)| {
                    vec![
                        t.name.clone(),
                        u.cost.map_or("-".to_string(), |c| format!("{c:.4}")),
                        count(u.prompt_tokens),
                        count(u.completion_tokens),
                        count(u.requests),
                    ]
                })
                .collect();
            println!("Usage {start} to {end} (end exclusive, UTC)");
            print_table(&["GATEWAY", "COST (USD)", "PROMPT TOK", "COMPLETION TOK", "REQUESTS"], rows, 1);
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
            let api_key = store
                .api_key(&name)?
                .with_context(|| format!("no API key for '{name}'"))?;
            let cfg = config.providers[&name].clone();
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
    let start = Utc::now().date_naive().with_day0(0).expect("day 1 exists");
    (start, start + Months::new(1))
}

fn prompt_key(name: &str) -> Result<String> {
    let key = rpassword::prompt_password(format!("API key for '{name}': "))?;
    if key.trim().is_empty() {
        bail!("API key cannot be empty");
    }
    Ok(key.trim().to_string())
}

/// Prints a table; the first `left_cols` columns are left-aligned, the rest right-aligned.
fn print_table(header: &[&str], rows: Vec<Vec<String>>, left_cols: usize) {
    let mut table = vec![header.iter().map(|h| h.to_string()).collect::<Vec<_>>()];
    table.extend(rows);
    let widths: Vec<usize> = (0..header.len())
        .map(|c| table.iter().map(|r| r[c].len()).max().unwrap_or(0))
        .collect();
    for row in &table {
        let cells: Vec<String> = row
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(i, (cell, w))| {
                if i < left_cols { format!("{cell:<w$}") } else { format!("{cell:>w$}") }
            })
            .collect();
        println!("{}", cells.join("  ").trim_end());
    }
}
