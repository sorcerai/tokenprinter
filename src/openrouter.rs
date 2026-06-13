//! OpenRouter spend-receipt source.
//!
//! Fetches data from three endpoints:
//!   - `/credits`   (required) — total credits purchased and total usage
//!   - `/key`       (required) — key metadata including daily/weekly/monthly usage
//!   - `/activity`  (best-effort) — per-model breakdown; on 403 or parse error, silently skipped.
//!     NOTE: The /activity success shape is unverified (management keys only). We parse
//!     defensively: if `data` is an array of objects, we pull optional fields `model` (string),
//!     `usage` (f64 cost), and token counts trying both `prompt_tokens`/`completion_tokens` and
//!     `tokens_prompt`/`tokens_completion`. Any mismatch or error returns an empty vec.

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

/// Parse the /activity response body (best-effort; returns empty on any error or mismatch).
/// NOTE: Success shape is unverified (requires management key). We parse defensively:
///   data is expected to be an array of objects with optional fields:
///   model (string), usage (f64 cost), and token counts via
///   `prompt_tokens`/`completion_tokens` or `tokens_prompt`/`tokens_completion`.
pub fn parse_activity(body: &str) -> Vec<ModelRow> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let arr = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return vec![],
    };
    let mut rows = Vec::new();
    for item in arr {
        let obj = match item.as_object() {
            Some(o) => o,
            None => continue,
        };
        let model = obj.get("model").and_then(|m| m.as_str()).unwrap_or("unknown").to_string();
        let cost = obj.get("usage").and_then(|u| u.as_f64()).unwrap_or(0.0);
        // Try both token field naming conventions
        let prompt_tokens = obj
            .get("prompt_tokens")
            .or_else(|| obj.get("tokens_prompt"))
            .and_then(|t| t.as_u64());
        let completion_tokens = obj
            .get("completion_tokens")
            .or_else(|| obj.get("tokens_completion"))
            .and_then(|t| t.as_u64());
        rows.push((model, prompt_tokens, completion_tokens, cost));
    }
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

    if !stmt.models.is_empty() {
        push(&mut o, rule('-'));
        push(&mut o, " MODEL BREAKDOWN".into());
        push(&mut o, rule('-'));
        for (model, pt, ct, cost) in &stmt.models {
            push(&mut o, format!("  {}", trunc_pub(model, 46)));
            if let Some(p) = pt {
                push(&mut o, lr("    Prompt tokens", &format!("{} ", commafy(*p))));
            }
            if let Some(c) = ct {
                push(&mut o, lr("    Completion tokens", &format!("{} ", commafy(*c))));
            }
            push(&mut o, lr("    Cost", &format!("{} ", money(*cost))));
        }
    }

    push(&mut o, rule('-'));
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

    const ACTIVITY_MGMT_JSON: &str = r#"{
        "data": [
            {
                "model": "anthropic/claude-opus-4",
                "usage": 5.12,
                "prompt_tokens": 10000,
                "completion_tokens": 500
            },
            {
                "model": "openai/gpt-4o",
                "usage": 1.50,
                "tokens_prompt": 3000,
                "tokens_completion": 200
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
        let rows = parse_activity(ACTIVITY_MGMT_JSON);
        assert_eq!(rows.len(), 2);
        let (model, pt, ct, cost) = &rows[0];
        assert_eq!(model, "anthropic/claude-opus-4");
        assert_eq!(*pt, Some(10000));
        assert_eq!(*ct, Some(500));
        assert!((cost - 5.12).abs() < 1e-9);
        // Second row uses tokens_prompt / tokens_completion naming
        let (model2, pt2, ct2, cost2) = &rows[1];
        assert_eq!(model2, "openai/gpt-4o");
        assert_eq!(*pt2, Some(3000));
        assert_eq!(*ct2, Some(200));
        assert!((cost2 - 1.50).abs() < 1e-9);
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
        stmt.models = vec![
            ("anthropic/claude-opus-4-5-20251101".into(), Some(100_000), Some(5_000), 1.23),
            ("openai/gpt-4o".into(), None, None, 0.45),
        ];
        let s = render_statement_text(&stmt, sample_when());
        assert!(s.contains("MODEL BREAKDOWN"));
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
