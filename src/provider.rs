//! Gateway providers: listing models and querying usage.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use clap::ValueEnum;
use reqwest::StatusCode;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::ProviderConfig;
use crate::verbose::{redact_str, vlog};

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

/// Longest rate-limit wait we accept; a longer one is reported as an error.
const MAX_WAIT: Duration = Duration::from_secs(300);
/// How many times one request is retried after a 429.
const MAX_RETRIES: u32 = 5;

/// The service is rate limiting us and we cannot or will not wait it out.
#[derive(Debug)]
pub struct RateLimitError(String);

impl std::fmt::Display for RateLimitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RateLimitError {}

/// GETs JSON. On a 429, waits as long as the response asks, then retries.
fn get_json(url: &str, api_key: &str, query: &[(&str, &str)]) -> Result<Value> {
    let client = reqwest::blocking::Client::new();
    // Errors and logs show the URL without any credentials embedded in it.
    let shown = redact_str(url);
    let mut retries = 0;
    let response = loop {
        let request = client
            .get(url)
            .bearer_auth(api_key)
            .query(query)
            .build()
            .with_context(|| format!("GET {shown}"))?;
        vlog!("GET {}", crate::verbose::redact(request.url()));
        let started = std::time::Instant::now();
        let response = client.execute(request).with_context(|| format!("GET {shown}"))?;
        vlog!("-> {} in {}ms", response.status(), started.elapsed().as_millis());
        if response.status() != StatusCode::TOO_MANY_REQUESTS {
            break response;
        }
        let wait = retry_delay(retries, response.headers(), Utc::now())
            .with_context(|| format!("GET {shown}: rate limited (429)"))?;
        eprintln!(
            "rate limited (429) by {}; waiting {}s (retry {}/{MAX_RETRIES})",
            shown.split('?').next().unwrap_or(&shown),
            fmt_secs(wait.as_secs_f64()),
            retries + 1
        );
        std::thread::sleep(wait);
        retries += 1;
    };
    response
        .error_for_status()
        .with_context(|| format!("GET {shown}"))?
        .json()
        .with_context(|| format!("parsing response from {shown}"))
}

/// How long to wait before retry number `retries` (0-based) after a 429.
/// Uses the service's Retry-After (seconds or HTTP date) or retry-after-ms,
/// else backs off 1s, 2s, 4s... Errors if the wait is over `MAX_WAIT` or
/// the retries are used up.
fn retry_delay(retries: u32, headers: &HeaderMap, now: DateTime<Utc>) -> Result<Duration> {
    let secs = requested_wait_secs(headers, now).unwrap_or_else(|| f64::from(1u32 << retries.min(16)));
    if secs > MAX_WAIT.as_secs_f64() {
        return Err(RateLimitError(format!(
            "the service asked us to wait {}s, which exceeds our {}s timeout",
            fmt_secs(secs),
            MAX_WAIT.as_secs()
        ))
        .into());
    }
    if retries >= MAX_RETRIES {
        return Err(RateLimitError(format!("still rate limited (429) after {MAX_RETRIES} retries")).into());
    }
    Ok(Duration::from_secs_f64(secs))
}

/// The wait the service asked for, in seconds, if it gave a usable one.
fn requested_wait_secs(headers: &HeaderMap, now: DateTime<Utc>) -> Option<f64> {
    let header = |name: &str| headers.get(name)?.to_str().ok().map(str::trim);
    let non_negative = |n: f64| (n.is_finite() && n >= 0.0).then_some(n);
    header("retry-after")
        .and_then(|v| match v.parse::<f64>() {
            Ok(secs) => non_negative(secs),
            Err(_) => DateTime::parse_from_rfc2822(v)
                .ok()
                .map(|at| (at.with_timezone(&Utc) - now).num_milliseconds().max(0) as f64 / 1000.0),
        })
        .or_else(|| header("retry-after-ms")?.parse::<f64>().ok().and_then(non_negative).map(|ms| ms / 1000.0))
}

/// "12" or "1.5": no trailing ".0".
fn fmt_secs(secs: f64) -> String {
    let text = format!("{secs:.1}");
    text.strip_suffix(".0").unwrap_or(&text).to_string()
}

fn data_items(value: &Value) -> impl Iterator<Item = &Value> {
    value["data"].as_array().into_iter().flatten()
}

/// LiteLLM's /model/info has prices; fall back to /v1/models (no prices).
fn litellm_models(base: &str, api_key: &str) -> Result<Vec<Model>> {
    match get_json(&format!("{base}/model/info"), api_key, &[]) {
        Ok(info) => Ok(parse_litellm_models(&info)),
        // Rate limiting is not a reason to try another endpoint.
        Err(e) if e.is::<RateLimitError>() => Err(e),
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

/// Prices come from the account API (`/v1/serverless-models`), not the
/// OpenAI-style `/models` list, which has none. If that call fails, fall back
/// to the plain list so models still show (without prices).
fn fireworks_models(base: &str, api_key: &str) -> Result<Vec<Model>> {
    match fireworks_priced_models(fireworks_api_root(base), api_key) {
        Ok(models) => Ok(models),
        // Rate limiting is not a reason to try another endpoint.
        Err(e) if e.is::<RateLimitError>() => Err(e),
        Err(e) => {
            vlog!("serverless-models failed ({e:#}); listing models without prices");
            let list = get_json(&format!("{base}/models"), api_key, &[])?;
            Ok(parse_openai_models(&list, |_| (None, None)))
        }
    }
}

/// Safety stop for a server that keeps returning page tokens.
const MAX_PAGES: usize = 100;

fn fireworks_priced_models(root: &str, api_key: &str) -> Result<Vec<Model>> {
    let url = format!("{root}/v1/serverless-models");
    let mut models = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let mut query = vec![("pageSize", "200")];
        if let Some(t) = token.as_deref() {
            query.push(("pageToken", t));
        }
        let page = get_json(&url, api_key, &query)?;
        models.extend(parse_fireworks_models(&page));
        token = next_page_token(&page);
        if token.is_none() {
            break;
        }
    }
    Ok(models)
}

fn next_page_token(page: &Value) -> Option<String> {
    page["nextPageToken"].as_str().filter(|t| !t.is_empty()).map(str::to_string)
}

fn parse_fireworks_models(page: &Value) -> Vec<Model> {
    let items = page["models"].as_array().into_iter().flatten();
    items
        .filter_map(|item| {
            let id = item["name"].as_str()?.to_string();
            let modes = item["serverlessModes"].as_array().map(Vec::as_slice).unwrap_or_default();
            // Show the standard tier; other tiers (priority, fast) cost more.
            let mode = modes
                .iter()
                .find(|m| matches!(m["serviceTier"].as_str().unwrap_or(""), "" | "standard" | "default"))
                .or_else(|| modes.first());
            let price = |wanted: fn(&str) -> bool| {
                let skus = mode?["skuInfos"].as_array()?;
                skus.iter()
                    .find(|s| s["sku"].as_str().is_some_and(|name| wanted(&name.to_lowercase())))
                    .and_then(sku_price_per_million)
            };
            Some(Model {
                id,
                // "(cached)" input is a separate, cheaper SKU; skip it.
                input_price: price(|n| n.contains("input") && !n.contains("(cached)")),
                output_price: price(|n| n.contains("output")),
            })
        })
        .collect()
}

/// USD per 1M tokens from a SKU whose money is `units` + `nanos`/1e9. The
/// docs show the money under both `amount` and `price`.
fn sku_price_per_million(sku: &Value) -> Option<f64> {
    let money = if sku["amount"].is_object() { &sku["amount"] } else { &sku["price"] };
    let units = if money["units"].is_null() { Some(0.0) } else { number(&money["units"]) }?;
    let nanos = number(&money["nanos"]).unwrap_or(0.0);
    Some(units + nanos / 1e9)
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

fn sku(name: &str, units: &str, nanos: u64, key: &str) -> Value {
    json!({"sku": name, key: {"currencyCode": "USD", "units": units, "nanos": nanos}, "unit": "1M tokens"})
}

fn fw_model(name: &str, modes: Value) -> Value {
    json!({"name": name, "serverlessModes": modes})
}

#[test]
fn fireworks_models_read_sku_prices_per_million() {
    let list = json!({"models": [fw_model("accounts/fireworks/models/a", json!([{
        "serviceTier": "",
        "skuInfos": [
            sku("LLM input tokens (cached)", "0", 100_000_000, "amount"),
            sku("LLM input tokens (uncached)", "0", 500_000_000, "amount"),
            sku("LLM output tokens", "2", 250_000_000, "amount"),
        ]}]))]});
    let models = parse_fireworks_models(&list);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "accounts/fireworks/models/a");
    assert!(close(models[0].input_price, 0.5), "{:?}", models[0]);
    assert!(close(models[0].output_price, 2.25), "{:?}", models[0]);
}

#[test]
fn fireworks_models_accept_price_key_and_string_units() {
    let list = json!({"models": [fw_model("m", json!([{
        "skuInfos": [sku("LLM input tokens (uncached)", "1", 0, "price"), sku("LLM output tokens", "3", 0, "price")]}]))]});
    let models = parse_fireworks_models(&list);
    assert!(close(models[0].input_price, 1.0) && close(models[0].output_price, 3.0));
}

#[test]
fn fireworks_models_prefer_the_standard_mode() {
    let list = json!({"models": [fw_model("m", json!([
        {"serviceTier": "priority", "skuInfos": [sku("LLM output tokens", "9", 0, "amount")]},
        {"serviceTier": "standard", "skuInfos": [sku("LLM output tokens", "3", 0, "amount")]},
    ]))]});
    assert!(close(parse_fireworks_models(&list)[0].output_price, 3.0));
}

#[test]
fn fireworks_models_use_first_mode_when_none_is_standard() {
    let list = json!({"models": [fw_model("m", json!([
        {"serviceTier": "fast", "skuInfos": [sku("LLM output tokens", "7", 0, "amount")]},
        {"serviceTier": "priority", "skuInfos": [sku("LLM output tokens", "9", 0, "amount")]},
    ]))]});
    assert!(close(parse_fireworks_models(&list)[0].output_price, 7.0));
}

#[test]
fn fireworks_models_without_modes_have_unknown_prices() {
    let list = json!({"models": [fw_model("m", json!([])), {"name": "n"}, {"nope": 1}]});
    let models = parse_fireworks_models(&list);
    assert_eq!(models.len(), 2);
    assert!(models.iter().all(|m| m.input_price.is_none() && m.output_price.is_none()));
}

#[test]
fn fireworks_next_page_token_ignores_empty() {
    assert_eq!(next_page_token(&json!({"nextPageToken": "abc"})), Some("abc".to_string()));
    assert_eq!(next_page_token(&json!({"nextPageToken": ""})), None);
    assert_eq!(next_page_token(&json!({})), None);
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
    // --- rate limiting ---

    use chrono::{TimeZone, Utc};
    use reqwest::header::{HeaderMap, HeaderValue};

    fn headers(pairs: &[(&'static str, &'static str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_static(value));
        }
        map
    }

    fn now() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 19, 12, 0, 0).unwrap()
    }

    fn wait(retries: u32, pairs: &[(&'static str, &'static str)]) -> Result<Duration> {
        retry_delay(retries, &headers(pairs), now())
    }

    #[test]
    fn retry_after_seconds_is_honoured_exactly() {
        assert_eq!(wait(0, &[("retry-after", "7")]).unwrap(), Duration::from_secs(7));
        assert_eq!(wait(0, &[("retry-after", "0")]).unwrap(), Duration::ZERO);
        assert_eq!(wait(0, &[("retry-after", " 12 ")]).unwrap(), Duration::from_secs(12));
    }

    #[test]
    fn retry_after_may_be_fractional() {
        assert_eq!(wait(0, &[("retry-after", "1.5")]).unwrap(), Duration::from_millis(1500));
    }

    #[test]
    fn retry_after_http_date_is_relative_to_now() {
        let at = wait(0, &[("retry-after", "Mon, 19 Oct 2026 12:00:10 GMT")]);
        assert_eq!(at.unwrap(), Duration::from_secs(10));
    }

    #[test]
    fn retry_after_date_in_the_past_means_no_wait() {
        let at = wait(0, &[("retry-after", "Mon, 19 Oct 2026 11:00:00 GMT")]);
        assert_eq!(at.unwrap(), Duration::ZERO);
    }

    #[test]
    fn retry_after_ms_is_used_when_retry_after_is_missing() {
        assert_eq!(wait(0, &[("retry-after-ms", "2500")]).unwrap(), Duration::from_millis(2500));
    }

    #[test]
    fn retry_after_wins_over_retry_after_ms() {
        let both = [("retry-after", "3"), ("retry-after-ms", "9000")];
        assert_eq!(wait(0, &both).unwrap(), Duration::from_secs(3));
    }

    #[test]
    fn exactly_the_maximum_wait_is_allowed() {
        assert_eq!(wait(0, &[("retry-after", "300")]).unwrap(), MAX_WAIT);
    }

    #[test]
    fn wait_over_the_maximum_is_an_error_that_says_so() {
        for value in ["301", "300.5", "99999999999999999999"] {
            let err = wait(0, &[("retry-after", value)]).unwrap_err();
            let text = err.to_string();
            assert!(text.contains("exceeds") && text.contains("300s timeout"), "{value}: {text}");
            assert!(err.is::<RateLimitError>());
        }
        assert!(wait(0, &[("retry-after", "Tue, 20 Oct 2026 12:00:00 GMT")]).is_err());
        assert!(wait(0, &[("retry-after-ms", "300001")]).is_err());
    }

    #[test]
    fn missing_or_unusable_headers_back_off_exponentially() {
        for headers in [&[][..], &[("retry-after", "soon")], &[("retry-after", "-5")], &[("retry-after", "inf")]] {
            let waits: Vec<_> = (0..MAX_RETRIES).map(|n| wait(n, headers).unwrap().as_secs()).collect();
            assert_eq!(waits, [1, 2, 4, 8, 16], "{headers:?}");
        }
    }

    #[test]
    fn gives_up_after_the_maximum_number_of_retries() {
        let err = wait(MAX_RETRIES, &[("retry-after", "1")]).unwrap_err();
        assert!(err.to_string().contains("after 5 retries"), "{err}");
        assert!(err.is::<RateLimitError>());
    }
}
