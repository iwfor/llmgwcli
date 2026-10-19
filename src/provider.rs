//! Gateway providers: listing models and querying usage.

use anyhow::{Context, Result};
use chrono::NaiveDate;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::ProviderConfig;

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
    match get_json(&format!("{base}/model/info"), api_key, &[]) {
        Ok(info) => Ok(parse_litellm_models(&info)),
        Err(_) => Ok(parse_openai_models(&get_json(&format!("{base}/models"), api_key, &[])?, |_| (None, None))),
    }
}

fn parse_litellm_models(info: &Value) -> Vec<Model> {
    data_items(info)
        .filter_map(|item| {
            let id = item["model_name"].as_str()?.to_string();
            let per_m = |field: &str| number(&item["model_info"][field]).map(|p| p * 1e6);
            Some(Model {
                id,
                input_price: per_m("input_cost_per_token"),
                output_price: per_m("output_cost_per_token"),
            })
        })
        .collect()
}

fn fireworks_models(base: &str, api_key: &str) -> Result<Vec<Model>> {
    let list = get_json(&format!("{base}/models"), api_key, &[])?;
    Ok(parse_openai_models(&list, fireworks_prices))
}

/// Fireworks reports prices per token in "pricing" when available.
fn fireworks_prices(item: &Value) -> (Option<f64>, Option<f64>) {
    let per_m = |field: &str| number(&item["pricing"][field]).map(|p| p * 1e6);
    (per_m("input"), per_m("output"))
}

fn parse_openai_models(
    list: &Value,
    prices: impl Fn(&Value) -> (Option<f64>, Option<f64>),
) -> Vec<Model> {
    data_items(list)
        .filter_map(|item| {
            let (input_price, output_price) = prices(item);
            Some(Model { id: item["id"].as_str()?.to_string(), input_price, output_price })
        })
        .collect()
}

fn litellm_usage(base: &str, api_key: &str, start: NaiveDate, end: NaiveDate) -> Result<Usage> {
    let (start, end) = (start.to_string(), end.to_string());
    let report = get_json(
        &format!("{base}/user/daily/activity"),
        api_key,
        &[("start_date", &start), ("end_date", &end)],
    )?;
    Ok(parse_litellm_usage(&report))
}

fn parse_litellm_usage(report: &Value) -> Usage {
    let totals = &report["metadata"];
    Usage {
        cost: number(&totals["total_spend"]),
        prompt_tokens: number(&totals["total_prompt_tokens"]).map(|n| n as u64),
        completion_tokens: number(&totals["total_completion_tokens"]).map(|n| n as u64),
        requests: number(&totals["total_api_requests"]).map(|n| n as u64),
    }
}

/// The account API lives at the host root, e.g. https://api.fireworks.ai,
/// while the configured base URL ends in /inference/v1.
fn fireworks_api_root(base_url: &str) -> &str {
    let root = base_url.split("/inference").next().unwrap_or(base_url);
    root.trim_end_matches('/')
}

fn fireworks_usage(cfg: &ProviderConfig, api_key: &str, start: NaiveDate, end: NaiveDate) -> Result<Usage> {
    let account = cfg
        .account_id
        .as_deref()
        .context("no account id; set it with edit-provider --account-id")?;
    let base = format!("{}/v1/accounts/{account}", fireworks_api_root(&cfg.base_url));
    let (start, end) = (format!("{start}T00:00:00Z"), format!("{end}T00:00:00Z"));
    let range = [("startTime", start.as_str()), ("endTime", end.as_str())];
    let summary = get_json(&format!("{base}/billing/summary"), api_key, &range)?;
    let tokens = get_json(&format!("{base}/billingUsage"), api_key, &range)?;
    Ok(parse_fireworks_usage(&summary, &tokens))
}

fn parse_fireworks_usage(summary: &Value, tokens: &Value) -> Usage {
    Usage {
        cost: sum_field(summary, "totalCost"),
        prompt_tokens: sum_field(tokens, "promptTokens").map(|n| n as u64),
        completion_tokens: sum_field(tokens, "completionTokens").map(|n| n as u64),
        requests: None,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn close(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-9)
    }

    #[test]
    fn default_base_urls() {
        assert_eq!(ProviderKind::Litellm.default_base_url(), None);
        assert_eq!(
            ProviderKind::Fireworks.default_base_url(),
            Some("https://api.fireworks.ai/inference/v1")
        );
    }

    #[test]
    fn kind_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&ProviderKind::Litellm).unwrap(), "\"litellm\"");
        assert_eq!(serde_json::to_string(&ProviderKind::Fireworks).unwrap(), "\"fireworks\"");
    }

    #[test]
    fn number_accepts_numbers_and_strings() {
        assert_eq!(number(&json!(1.5)), Some(1.5));
        assert_eq!(number(&json!("2.5")), Some(2.5));
        assert_eq!(number(&json!("abc")), None);
        assert_eq!(number(&json!(null)), None);
        assert_eq!(number(&json!({})), None);
    }

    #[test]
    fn sum_field_adds_nested_values() {
        let v = json!({
            "items": [{"cost": "1.5"}, {"cost": 2}, {"other": 9}],
            "deep": {"x": [{"cost": 0.5}]},
        });
        assert_eq!(sum_field(&v, "cost"), Some(4.0));
    }

    #[test]
    fn sum_field_none_when_absent_or_not_numeric() {
        assert_eq!(sum_field(&json!({"a": 1}), "cost"), None);
        assert_eq!(sum_field(&json!({"cost": {"units": 1}}), "cost"), None);
        assert_eq!(sum_field(&json!(5), "cost"), None);
    }

    #[test]
    fn litellm_models_convert_price_to_per_million() {
        let info = json!({"data": [
            {"model_name": "llama", "model_info": {
                "input_cost_per_token": 0.0000009, "output_cost_per_token": 0.0000018}},
            {"model_name": "free", "model_info": {}},
            {"no_name": true},
        ]});
        let models = parse_litellm_models(&info);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "llama");
        assert!(close(models[0].input_price, 0.9));
        assert!(close(models[0].output_price, 1.8));
        assert_eq!(models[1].input_price, None);
    }

    #[test]
    fn litellm_models_empty_when_no_data() {
        assert!(parse_litellm_models(&json!({})).is_empty());
    }

    #[test]
    fn fireworks_models_read_pricing_when_present() {
        let list = json!({"data": [
            {"id": "accounts/fireworks/models/a", "pricing": {"input": "0.0000002", "output": 0.0000004}},
            {"id": "plain"},
            {"object": "model"},
        ]});
        let models = parse_openai_models(&list, fireworks_prices);
        assert_eq!(models.len(), 2);
        assert!(close(models[0].input_price, 0.2));
        assert!(close(models[0].output_price, 0.4));
        assert_eq!(models[1].output_price, None);
    }

    #[test]
    fn litellm_usage_reads_metadata_totals() {
        let usage = parse_litellm_usage(&json!({"metadata": {
            "total_spend": 12.5, "total_prompt_tokens": 1000,
            "total_completion_tokens": 500, "total_api_requests": 42}}));
        assert_eq!(usage.cost, Some(12.5));
        assert_eq!(usage.prompt_tokens, Some(1000));
        assert_eq!(usage.completion_tokens, Some(500));
        assert_eq!(usage.requests, Some(42));
    }

    #[test]
    fn litellm_usage_missing_metadata_is_unknown() {
        let usage = parse_litellm_usage(&json!({}));
        assert_eq!(usage.cost, None);
        assert_eq!(usage.requests, None);
    }

    #[test]
    fn fireworks_usage_sums_costs_and_string_tokens() {
        let summary = json!({"lineItems": [{"totalCost": "1.25"}, {"totalCost": 0.75}]});
        let tokens = json!({"serverlessCosts": [
            {"promptTokens": "100", "completionTokens": "10"},
            {"promptTokens": "200", "completionTokens": "20"}]});
        let usage = parse_fireworks_usage(&summary, &tokens);
        assert_eq!(usage.cost, Some(2.0));
        assert_eq!(usage.prompt_tokens, Some(300));
        assert_eq!(usage.completion_tokens, Some(30));
        assert_eq!(usage.requests, None);
    }

    #[test]
    fn fireworks_api_root_strips_inference_path() {
        assert_eq!(
            fireworks_api_root("https://api.fireworks.ai/inference/v1"),
            "https://api.fireworks.ai"
        );
        assert_eq!(fireworks_api_root("http://127.0.0.1:9/inference/v1/"), "http://127.0.0.1:9");
        assert_eq!(fireworks_api_root("https://example.com/"), "https://example.com");
    }

    #[test]
    fn fireworks_usage_requires_account_id() {
        let cfg = ProviderConfig {
            kind: ProviderKind::Fireworks,
            base_url: "http://127.0.0.1:1/inference/v1".into(),
            alias: None,
            account_id: None,
            credential_store: crate::store::CredentialStore::File,
        };
        let day = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        let err = fireworks_usage(&cfg, "k", day, day).unwrap_err();
        assert!(err.to_string().contains("account id"));
    }
}
