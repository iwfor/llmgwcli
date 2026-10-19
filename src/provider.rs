//! Gateway providers: listing models and querying usage.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::ProviderConfig;

const FIREWORKS_API: &str = "https://api.fireworks.ai";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Litellm,
    Fireworks,
}

impl ProviderKind {
    pub fn default_base_url(self) -> Option<&'static str> {
        match self {
            ProviderKind::Litellm => None,
            ProviderKind::Fireworks => Some("https://api.fireworks.ai/inference/v1"),
        }
    }
}

#[derive(Debug)]
pub struct Model {
    pub id: String,
    /// USD per 1M tokens.
    pub input_price: Option<f64>,
    pub output_price: Option<f64>,
}

/// Usage over a date range. Fields a gateway does not report are `None`.
#[derive(Debug, Default)]
pub struct Usage {
    /// USD.
    pub cost: Option<f64>,
    pub prompt_tokens: Option<u64>,
    pub completion_tokens: Option<u64>,
    pub requests: Option<u64>,
}

pub fn list_models(cfg: &ProviderConfig, api_key: &str) -> Result<Vec<Model>> {
    let base = cfg.base_url.trim_end_matches('/');
    match cfg.kind {
        ProviderKind::Litellm => litellm_models(base, api_key),
        ProviderKind::Fireworks => fireworks_models(base, api_key),
    }
}

/// Usage from `start` (inclusive) to `end` (exclusive), in UTC.
pub fn usage(cfg: &ProviderConfig, api_key: &str, start: NaiveDate, end: NaiveDate) -> Result<Usage> {
    match cfg.kind {
        ProviderKind::Litellm => litellm_usage(cfg.base_url.trim_end_matches('/'), api_key, start, end),
        ProviderKind::Fireworks => fireworks_usage(cfg, api_key, start, end),
    }
}

fn get_json(url: &str, api_key: &str, query: &[(&str, &str)]) -> Result<Value> {
    reqwest::blocking::Client::new()
        .get(url)
        .bearer_auth(api_key)
        .query(query)
        .send()
        .and_then(|r| r.error_for_status())
        .with_context(|| format!("GET {url}"))?
        .json()
        .with_context(|| format!("parsing response from {url}"))
}

fn data_items(value: &Value) -> impl Iterator<Item = &Value> {
    value["data"].as_array().into_iter().flatten()
}

/// LiteLLM's /model/info has prices; fall back to /v1/models (no prices).
fn litellm_models(base: &str, api_key: &str) -> Result<Vec<Model>> {
    if let Ok(info) = get_json(&format!("{base}/model/info"), api_key, &[]) {
        return Ok(data_items(&info)
            .filter_map(|item| {
                let id = item["model_name"].as_str()?.to_string();
                let per_m = |field: &str| number(&item["model_info"][field]).map(|p| p * 1e6);
                Some(Model {
                    id,
                    input_price: per_m("input_cost_per_token"),
                    output_price: per_m("output_cost_per_token"),
                })
            })
            .collect());
    }
    openai_style_models(base, api_key, |_| (None, None))
}

fn fireworks_models(base: &str, api_key: &str) -> Result<Vec<Model>> {
    // Fireworks reports prices per token in "pricing" when available.
    openai_style_models(base, api_key, |item| {
        let per_m = |field: &str| number(&item["pricing"][field]).map(|p| p * 1e6);
        (per_m("input"), per_m("output"))
    })
}

fn openai_style_models(
    base: &str,
    api_key: &str,
    prices: impl Fn(&Value) -> (Option<f64>, Option<f64>),
) -> Result<Vec<Model>> {
    let list = get_json(&format!("{base}/models"), api_key, &[])?;
    Ok(data_items(&list)
        .filter_map(|item| {
            let (input_price, output_price) = prices(item);
            Some(Model { id: item["id"].as_str()?.to_string(), input_price, output_price })
        })
        .collect())
}

fn litellm_usage(base: &str, api_key: &str, start: NaiveDate, end: NaiveDate) -> Result<Usage> {
    let (start, end) = (start.to_string(), end.to_string());
    let report = get_json(
        &format!("{base}/user/daily/activity"),
        api_key,
        &[("start_date", &start), ("end_date", &end)],
    )?;
    let totals = &report["metadata"];
    Ok(Usage {
        cost: number(&totals["total_spend"]),
        prompt_tokens: number(&totals["total_prompt_tokens"]).map(|n| n as u64),
        completion_tokens: number(&totals["total_completion_tokens"]).map(|n| n as u64),
        requests: number(&totals["total_api_requests"]).map(|n| n as u64),
    })
}

fn fireworks_usage(cfg: &ProviderConfig, api_key: &str, start: NaiveDate, end: NaiveDate) -> Result<Usage> {
    let account = cfg
        .account_id
        .as_deref()
        .context("no account id; set it with edit-provider --account-id")?;
    let base = format!("{FIREWORKS_API}/v1/accounts/{account}");
    let (start, end) = (format!("{start}T00:00:00Z"), format!("{end}T00:00:00Z"));
    let range = [("startTime", start.as_str()), ("endTime", end.as_str())];
    let summary = get_json(&format!("{base}/billing/summary"), api_key, &range)?;
    let tokens = get_json(&format!("{base}/billingUsage"), api_key, &range)?;
    Ok(Usage {
        cost: sum_field(&summary, "totalCost"),
        prompt_tokens: sum_field(&tokens, "promptTokens").map(|n| n as u64),
        completion_tokens: sum_field(&tokens, "completionTokens").map(|n| n as u64),
        requests: None,
    })
}

/// Prices may arrive as JSON numbers or strings.
fn number(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

/// Sums every numeric `name` field found anywhere in `value`.
fn sum_field(value: &Value, name: &str) -> Option<f64> {
    let add = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(x + y),
        (x, None) => x,
        (None, y) => y,
    };
    match value {
        Value::Object(map) => map.iter().fold(None, |acc, (key, v)| {
            add(acc, if key == name { number(v) } else { sum_field(v, name) })
        }),
        Value::Array(items) => items.iter().fold(None, |acc, v| add(acc, sum_field(v, name))),
        _ => None,
    }
}
