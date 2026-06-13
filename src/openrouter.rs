//! OpenRouter spend-receipt source.
//!
//! Fetches data from three endpoints:
//!   - `/credits`   (required) — total credits purchased and total usage (lifetime)
//!   - `/key`       (required) — key metadata including daily/weekly/monthly usage
//!   - `/activity`  (best-effort) — per-model breakdown; on 403 or parse error, silently skipped.
//!     Requires a management key. Returns daily rows (~30 days × models × endpoints).
//!     We aggregate by `model`, summing prompt_tokens, completion_tokens, and usage (USD cost)
//!     across all rows, then sort by total cost descending.
//!     NOTE: /activity covers the last ~30 days; /credits total_usage is lifetime.

use anyhow::{anyhow, Context};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::render::{center, commafy, lr, money, qr_raster, rule, trunc_pub};

const OR_BASE: &str = "https://openrouter.ai/api/v1";
/// Timeout in milliseconds for HTTP operations.
const TIMEOUT_MS: u64 = 20_000;

// ── Public types ────────────────────────────────────────────────────────────

/// Per-model activity row from /activity (management keys only).
/// Fields: (model_name, prompt_tokens, completion_tokens, cost_usd)
pub type ModelRow = (String, Option<u64>, Option<u64>, f64);

#[derive(Debug)]
pub struct OpenRouterStatement {
    pub total_credits: f64,
    pub total_usage: f64,
    pub remaining: f64,
    pub key_label: String,
    pub usage_daily: f64,
    pub usage_weekly: f64,
    pub usage_monthly: f64,
    pub limit_remaining: Option<f64>,
    /// Per-model breakdown from /activity. Empty if unavailable (non-management key returns 403).
    pub models: Vec<ModelRow>,
}

// ── HTTP fetch ───────────────────────────────────────────────────────────────

fn build_agent() -> ureq::Agent {
    use std::time::Duration;
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_millis(TIMEOUT_MS))
        .timeout_read(Duration::from_millis(TIMEOUT_MS))
        .build()
}

fn get_json(agent: &ureq::Agent, url: &str, key: &str) -> anyhow::Result<String> {
    let resp = agent
        .get(url)
        .set("Authorization", &format!("Bearer {key}"))
        .call()
        .map_err(|e| anyhow!("GET {url} failed: {e}"))?;
    resp.into_string().context("reading response body")
}

/// Attempt /activity; returns None on 403 or any HTTP error (caller skips silently).
fn get_activity_json(agent: &ureq::Agent, url: &str, key: &str) -> Option<String> {
    match agent
        .get(url)
        .set("Authorization", &format!("Bearer {key}"))
        .call()
    {
        Ok(resp) => resp.into_string().ok(),
        Err(ureq::Error::Status(403, _)) => None,
        Err(_) => None,
    }
}

pub fn fetch_statement(key: &str) -> anyhow::Result<OpenRouterStatement> {
    let agent = build_agent();

    let credits_body = get_json(&agent, &format!("{OR_BASE}/credits"), key)?;
    let (total_credits, total_usage) =
        parse_credits(&credits_body).context("parsing /credits response")?;

    let key_body = get_json(&agent, &format!("{OR_BASE}/key"), key)?;
    let ki = parse_key(&key_body).context("parsing /key response")?;

    let models = match get_activity_json(&agent, &format!("{OR_BASE}/activity"), key) {
        Some(body) => parse_activity(&body),
        None => vec![],
    };

    Ok(OpenRouterStatement {
        total_credits,
        total_usage,
        remaining: total_credits - total_usage,
        key_label: ki.label,
        usage_daily: ki.usage_daily,
        usage_weekly: ki.usage_weekly,
        usage_monthly: ki.usage_monthly,
        limit_remaining: ki.limit_remaining,
        models,
    })
}

// ── Pure parsing functions (unit-tested without network) ────────────────────

/// Parse the /credits response body.
/// Expected: `{"data":{"total_credits":f64,"total_usage":f64}}`
pub fn parse_credits(body: &str) -> anyhow::Result<(f64, f64)> {
    let v: Value = serde_json::from_str(body).context("credits JSON parse")?;
    let data = v.get("data").ok_or_else(|| anyhow!("credits: missing 'data'"))?;
    let total_credits = data
        .get("total_credits")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| anyhow!("credits: missing total_credits"))?;
    let total_usage = data
        .get("total_usage")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| anyhow!("credits: missing total_usage"))?;
    Ok((total_credits, total_usage))
}

#[derive(Debug)]
pub struct KeyInfo {
    pub label: String,
    pub usage_daily: f64,
    pub usage_weekly: f64,
    pub usage_monthly: f64,
    pub limit_remaining: Option<f64>,
}

/// Parse the /key response body.
pub fn parse_key(body: &str) -> anyhow::Result<KeyInfo> {
    let v: Value = serde_json::from_str(body).context("key JSON parse")?;
    let data = v.get("data").ok_or_else(|| anyhow!("key: missing 'data'"))?;
    let label = data
        .get("label")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let usage_daily = data.get("usage_daily").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let usage_weekly = data.get("usage_weekly").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let usage_monthly = data.get("usage_monthly").and_then(|x| x.as_f64()).unwrap_or(0.0);
    let limit_remaining = data.get("limit_remaining").and_then(|x| x.as_f64());
    Ok(KeyInfo { label, usage_daily, usage_weekly, usage_monthly, limit_remaining })
}

/// Parse the /activity response body and aggregate by model (best-effort).
///
/// The real shape (management key only) is:
///   `{"data": [...rows]}` where each row has:
///   - `model`              (string)  — e.g. "anthropic/claude-sonnet-4.6"
///   - `prompt_tokens`      (u64)     — input tokens for this day/endpoint slice
///   - `completion_tokens`  (u64)     — output tokens
///   - `usage`              (f64)     — USD cost
///     (plus date, model_permaslug, endpoint_id, provider_name, etc. — ignored)
///
/// We aggregate: for each unique `model`, sum prompt_tokens, completion_tokens, usage across
/// all ~30 days × endpoints rows. Result is sorted by total cost descending.
/// Returns an empty Vec on any error, missing `data`, non-array body, or 403 response.
pub fn parse_activity(body: &str) -> Vec<ModelRow> {
    use std::collections::HashMap;

    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let arr = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return vec![],
    };

    // Accumulate: model → (prompt_tokens, completion_tokens, cost)
    let mut agg: HashMap<String, (u64, u64, f64)> = HashMap::new();

    for item in arr {
        let obj = match item.as_object() {
            Some(o) => o,
            None => continue,
        };
        // Skip rows with no model name
        let model = match obj.get("model").and_then(|m| m.as_str()) {
            Some(m) if !m.is_empty() => m.to_string(),
            _ => continue,
        };
        let cost = obj.get("usage").and_then(|u| u.as_f64()).unwrap_or(0.0);
        let pt = obj.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        let ct = obj.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);

        let entry = agg.entry(model).or_insert((0, 0, 0.0));
        entry.0 += pt;
        entry.1 += ct;
        entry.2 += cost;
    }

    let mut rows: Vec<ModelRow> = agg
        .into_iter()
        .map(|(model, (pt, ct, cost))| {
            let prompt = if pt > 0 { Some(pt) } else { None };
            let completion = if ct > 0 { Some(ct) } else { None };
            (model, prompt, completion, cost)
        })
        .collect();

    // Sort by cost descending
    rows.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    rows
}

// ── Receipt rendering ────────────────────────────────────────────────────────

/// Render a human-readable 48-col receipt for an OpenRouter statement.
/// Every line is guaranteed ≤48 characters.
pub fn render_statement_text(stmt: &OpenRouterStatement, when: DateTime<Utc>) -> String {
    let mut o = String::new();
    let push = |o: &mut String, l: String| {
        o.push_str(&l);
        o.push('\n');
    };

    push(&mut o, center("OPENROUTER"));
    push(&mut o, rule('='));
    push(&mut o, center("TOKEN PRINTER"));
    push(&mut o, rule('='));
    push(&mut o, lr(" Source", "OpenRouter "));
    push(&mut o, lr(" Key", &format!("{} ", trunc_pub(&stmt.key_label, 28))));
    push(&mut o, lr(" Date", &format!("{} ", when.format("%Y-%m-%d %H:%M:%S"))));

    // MODEL BREAKDOWN — aggregated from /activity (last ~30 days).
    // Note: /activity covers ~last 30 days; CREDITS total_usage below is lifetime.
    if !stmt.models.is_empty() {
        push(&mut o, rule('-'));
        push(&mut o, " MODEL BREAKDOWN  (last 30 days)".into());
        push(&mut o, rule('-'));
        let top: Vec<_> = stmt.models.iter().take(10).collect();
        let rest = stmt.models.len().saturating_sub(10);
        let rest_cost: f64 = stmt.models.iter().skip(10).map(|(_, _, _, c)| c).sum();
        for (model, pt, ct, cost) in &top {
            // Model name: truncate to leave room for cost on the right
            // Name col: 48 - 1(space) - cost_width. Cost " $XX.XX" ≤ 8 chars → name ≤ 39.
            let cost_str = format!("{} ", money(*cost));
            let name_budget = 48usize
                .saturating_sub(1) // leading space
                .saturating_sub(cost_str.chars().count());
            let name = trunc_pub(model, name_budget);
            push(&mut o, lr(&format!(" {name}"), &cost_str));
            // Compact tokens line: "  P: 1,234 + C: 567"
            match (pt, ct) {
                (Some(p), Some(c)) => {
                    let tok_line = format!("  {} + {} tok", commafy(*p), commafy(*c));
                    // Truncate if needed (≤48 chars)
                    let tok_trunc: String = tok_line.chars().take(48).collect();
                    push(&mut o, tok_trunc);
                }
                (Some(p), None) => {
                    let tok_line = format!("  {} prompt tok", commafy(*p));
                    let tok_trunc: String = tok_line.chars().take(48).collect();
                    push(&mut o, tok_trunc);
                }
                (None, Some(c)) => {
                    let tok_line = format!("  {} completion tok", commafy(*c));
                    let tok_trunc: String = tok_line.chars().take(48).collect();
                    push(&mut o, tok_trunc);
                }
                (None, None) => {}
            }
        }
        if rest > 0 {
            push(&mut o, lr(&format!("  +{rest} more"), &format!("{} ", money(rest_cost))));
        }
    }

    push(&mut o, rule('-'));
    // NOTE: /activity covers last ~30 days; CREDITS total_usage below is lifetime.
    push(&mut o, " CREDITS".into());
    push(&mut o, rule('-'));
    push(&mut o, lr("   Total credits", &format!("{} ", money(stmt.total_credits))));
    push(&mut o, lr("   Total used", &format!("{} ", money(stmt.total_usage))));
    push(&mut o, lr("   Remaining", &format!("{} ", money(stmt.remaining))));
    if let Some(lr_val) = stmt.limit_remaining {
        push(&mut o, lr("   Limit remaining", &format!("{} ", money(lr_val))));
    }
    push(&mut o, lr("   Spent today", &format!("{} ", money(stmt.usage_daily))));
    push(&mut o, lr("   This week", &format!("{} ", money(stmt.usage_weekly))));
    push(&mut o, lr("   This month", &format!("{} ", money(stmt.usage_monthly))));

    push(&mut o, rule('='));
    push(&mut o, lr(" TOTAL USED", &format!("{} ", money(stmt.total_usage))));
    push(&mut o, rule('='));
    push(&mut o, center("Thank you for vibe coding!"));
    push(&mut o, center("*** NO REFUNDS ON TOKENS ***"));

    o
}

/// Render a Star Line printer byte stream (ESC@ init + text + optional QR + cut).
/// `qr_data`: if Some, encode as QR after the text body.
pub fn render_statement_bytes(
    stmt: &OpenRouterStatement,
    when: DateTime<Utc>,
    qr_data: Option<&str>,
) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&[0x1b, 0x40]); // ESC @ init
    b.extend_from_slice(render_statement_text(stmt, when).as_bytes());
    b.extend_from_slice(b"\n\n\n");

    if let Some(data) = qr_data {
        if !data.is_empty() {
            let raster = qr_raster(data);
            if !raster.is_empty() {
                b.extend_from_slice(&raster);
                // Feed past cutter gap
                b.extend_from_slice(b"\n\n\n\n\n\n");
            }
        }
    }

    b.extend_from_slice(&[0x1b, 0x64, 0x02]); // ESC d 2 full cut
    b
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    // Verified JSON shapes from the real API (no network calls).

    const CREDITS_JSON: &str =
        r#"{"data":{"total_credits":150,"total_usage":137.141032391}}"#;

    const KEY_JSON: &str = r#"{
        "data": {
            "label": "test-key",
            "is_management_key": false,
            "is_provisioning_key": false,
            "limit": null,
            "limit_remaining": null,
            "usage": 137.141032391,
            "usage_daily": 0.0,
            "usage_weekly": 0.0,
            "usage_monthly": 137.141032391,
            "byok_usage": 0.0,
            "is_free_tier": false,
            "expires_at": null
        }
    }"#;

    // Verified real /activity field names (management key, ~30 days of daily rows).
    // Each row: model, prompt_tokens, completion_tokens, usage (USD cost), plus
    // date, model_permaslug, endpoint_id, provider_name, byok_usage_inference,
    // byok_requests, reasoning_tokens, requests — all ignored by parse_activity.
    const ACTIVITY_MGMT_JSON: &str = r#"{
        "data": [
            {
                "date": "2026-06-12 00:00:00",
                "model_permaslug": "anthropic/claude-opus-4-20260117",
                "model": "anthropic/claude-opus-4",
                "provider_name": "Anthropic",
                "endpoint_id": "ep-1",
                "usage": 5.12,
                "prompt_tokens": 10000,
                "completion_tokens": 500,
                "reasoning_tokens": 0,
                "requests": 10,
                "byok_usage_inference": 0,
                "byok_requests": 0
            },
            {
                "date": "2026-06-12 00:00:00",
                "model_permaslug": "openai/gpt-4o-2024-11-20",
                "model": "openai/gpt-4o",
                "provider_name": "OpenAI",
                "endpoint_id": "ep-2",
                "usage": 1.50,
                "prompt_tokens": 3000,
                "completion_tokens": 200,
                "reasoning_tokens": 0,
                "requests": 5,
                "byok_usage_inference": 0,
                "byok_requests": 0
            }
        ]
    }"#;

    const ACTIVITY_403_JSON: &str =
        r#"{"error":{"message":"Only management keys can fetch activity for an account","code":403}}"#;

    #[test]
    fn parse_credits_extracts_values() {
        let (credits, usage) = parse_credits(CREDITS_JSON).unwrap();
        assert!((credits - 150.0).abs() < 1e-9);
        assert!((usage - 137.141032391).abs() < 1e-6);
    }

    #[test]
    fn parse_credits_missing_data_errors() {
        assert!(parse_credits(r#"{"other":{}}"#).is_err());
    }

    #[test]
    fn parse_key_extracts_fields() {
        let ki = parse_key(KEY_JSON).unwrap();
        assert_eq!(ki.label, "test-key");
        assert!((ki.usage_monthly - 137.141032391).abs() < 1e-6);
        assert_eq!(ki.limit_remaining, None);
        assert!((ki.usage_daily - 0.0).abs() < 1e-9);
    }

    #[test]
    fn parse_key_missing_label_defaults_empty() {
        let body = r#"{"data":{"usage_daily":1.0,"usage_weekly":2.0,"usage_monthly":3.0}}"#;
        let ki = parse_key(body).unwrap();
        assert_eq!(ki.label, "");
    }

    #[test]
    fn parse_activity_management_key_shape() {
        // Single-row-per-model fixture — verifies field extraction and cost-desc sort.
        let rows = parse_activity(ACTIVITY_MGMT_JSON);
        assert_eq!(rows.len(), 2);
        // Sorted by cost descending: claude-opus-4 ($5.12) before gpt-4o ($1.50)
        let (model, pt, ct, cost) = &rows[0];
        assert_eq!(model, "anthropic/claude-opus-4");
        assert_eq!(*pt, Some(10000));
        assert_eq!(*ct, Some(500));
        assert!((cost - 5.12).abs() < 1e-9);
        let (model2, pt2, ct2, cost2) = &rows[1];
        assert_eq!(model2, "openai/gpt-4o");
        assert_eq!(*pt2, Some(3000));
        assert_eq!(*ct2, Some(200));
        assert!((cost2 - 1.50).abs() < 1e-9);
    }

    /// Fixture: 4 rows across 2 days and 2 models — tests aggregation and sort.
    const ACTIVITY_AGG_JSON: &str = r#"{
        "data": [
            {
                "date": "2026-06-11 00:00:00",
                "model": "anthropic/claude-sonnet-4.6",
                "endpoint_id": "ep-a",
                "usage": 3.00,
                "prompt_tokens": 20000,
                "completion_tokens": 800,
                "requests": 4
            },
            {
                "date": "2026-06-12 00:00:00",
                "model": "anthropic/claude-sonnet-4.6",
                "endpoint_id": "ep-a",
                "usage": 5.50,
                "prompt_tokens": 35000,
                "completion_tokens": 1200,
                "requests": 7
            },
            {
                "date": "2026-06-11 00:00:00",
                "model": "openai/gpt-4o",
                "endpoint_id": "ep-b",
                "usage": 1.00,
                "prompt_tokens": 4000,
                "completion_tokens": 300,
                "requests": 2
            },
            {
                "date": "2026-06-12 00:00:00",
                "model": "openai/gpt-4o",
                "endpoint_id": "ep-b",
                "usage": 0.75,
                "prompt_tokens": 3000,
                "completion_tokens": 250,
                "requests": 2
            }
        ]
    }"#;

    #[test]
    fn parse_activity_aggregates_by_model() {
        let rows = parse_activity(ACTIVITY_AGG_JSON);
        // 4 raw rows → 2 models
        assert_eq!(rows.len(), 2, "expected 2 aggregated models");

        // Sorted by cost descending: claude-sonnet-4.6 ($8.50) before gpt-4o ($1.75)
        let (model0, pt0, ct0, cost0) = &rows[0];
        assert_eq!(model0, "anthropic/claude-sonnet-4.6");
        assert_eq!(*pt0, Some(20000 + 35000), "prompt tokens should be summed");
        assert_eq!(*ct0, Some(800 + 1200), "completion tokens should be summed");
        assert!((cost0 - 8.50).abs() < 1e-9, "cost should be summed: got {cost0}");

        let (model1, pt1, ct1, cost1) = &rows[1];
        assert_eq!(model1, "openai/gpt-4o");
        assert_eq!(*pt1, Some(4000 + 3000));
        assert_eq!(*ct1, Some(300 + 250));
        assert!((cost1 - 1.75).abs() < 1e-9, "cost should be summed: got {cost1}");
    }

    #[test]
    fn parse_activity_403_body_returns_empty() {
        // 403 body has no 'data' array — silently return empty
        let rows = parse_activity(ACTIVITY_403_JSON);
        assert!(rows.is_empty());
    }

    #[test]
    fn parse_activity_garbage_returns_empty() {
        assert!(parse_activity("not json at all").is_empty());
        assert!(parse_activity(r#"{"data": "string not array"}"#).is_empty());
    }

    fn sample_statement() -> OpenRouterStatement {
        OpenRouterStatement {
            total_credits: 150.0,
            total_usage: 137.141032391,
            remaining: 12.858967609,
            key_label: "my-test-key".into(),
            usage_daily: 0.0,
            usage_weekly: 0.0,
            usage_monthly: 137.141032391,
            limit_remaining: None,
            models: vec![],
        }
    }

    fn sample_when() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 13, 16, 40, 0).unwrap()
    }

    #[test]
    fn render_text_contains_key_sections() {
        let s = render_statement_text(&sample_statement(), sample_when());
        assert!(s.contains("OPENROUTER"));
        assert!(s.contains("TOKEN PRINTER"));
        assert!(s.contains("CREDITS"));
        assert!(s.contains("TOTAL USED"));
        assert!(s.contains("$150.00"));
        assert!(s.contains("$137.14"));
    }

    #[test]
    fn render_text_all_lines_fit_48_cols() {
        let s = render_statement_text(&sample_statement(), sample_when());
        for line in s.lines() {
            assert!(
                line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn render_text_with_models_fits_48_cols() {
        let mut stmt = sample_statement();
        // 12 models to exercise top-10 cap and "+N more" line
        stmt.models = vec![
            ("anthropic/claude-sonnet-4.6".into(), Some(55_000), Some(2_000), 8.50),
            ("anthropic/claude-opus-4-5-20251101".into(), Some(100_000), Some(5_000), 3.23),
            ("openai/gpt-4o".into(), None, None, 1.45),
            ("openai/gpt-4o-mini".into(), Some(8_000), None, 0.90),
            ("meta-llama/llama-3-70b".into(), Some(5_000), Some(200), 0.50),
            ("google/gemini-pro".into(), Some(3_000), Some(150), 0.30),
            ("mistralai/mistral-7b".into(), Some(2_000), Some(100), 0.15),
            ("cohere/command-r".into(), Some(1_000), Some(50), 0.10),
            ("anthropic/claude-haiku-3.5".into(), Some(500), Some(25), 0.08),
            ("perplexity/sonar".into(), Some(300), Some(10), 0.05),
            ("qwen/qwen2-72b".into(), Some(200), Some(5), 0.03),
            ("deepseek/deepseek-r1".into(), Some(100), Some(2), 0.01),
        ];
        let s = render_statement_text(&stmt, sample_when());
        assert!(s.contains("MODEL BREAKDOWN"), "should contain MODEL BREAKDOWN");
        assert!(s.contains("last 30 days"), "should mention last 30 days");
        assert!(s.contains("+2 more"), "should have +2 more line for models beyond top 10");
        for line in s.lines() {
            assert!(
                line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn render_bytes_starts_init_ends_cut() {
        let b = render_statement_bytes(&sample_statement(), sample_when(), None);
        assert_eq!(&b[0..2], &[0x1b, 0x40]);
        assert_eq!(&b[b.len() - 3..], &[0x1b, 0x64, 0x02]);
    }

    #[test]
    fn render_bytes_with_qr_includes_raster_cmd() {
        let b = render_statement_bytes(
            &sample_statement(),
            sample_when(),
            Some("https://openrouter.ai/activity"),
        );
        assert!(b.windows(3).any(|w| w == [0x1b, 0x1d, 0x53]),
            "expected raster command in output");
    }
}
