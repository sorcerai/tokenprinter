//! OpenRouter spend-receipt source.
//!
//! Fetches data from three endpoints:
//!   - `/credits`   (required) — total credits purchased and total usage (lifetime)
//!   - `/key`       (required) — key metadata including daily/weekly/monthly usage
//!   - `/activity`  (best-effort) — per-model breakdown; on 403 or parse error, silently skipped.
//!     Requires a management key. Returns daily rows (~30 days × models × endpoints).
//!     We filter rows by the requested window (last N days or a specific date), aggregate by
//!     `model`, summing prompt_tokens, completion_tokens, and usage (USD cost) across those rows,
//!     then sort by total cost descending.
//!     NOTE: /activity covers the last ~30 days; /credits total_usage is lifetime.
//!
//! Council-pass improvements (FIX 1–4):
//!   FIX 1: Windows anchor on max(activity date), not Utc::now. Future-dated rows are quarantined.
//!   FIX 2: "Last 24h" renamed to "Latest day (<date>)"; 7d/30d also anchored to data.
//!   FIX 3: Unavailable activity is distinct from zero spend; no misleading $0.00 period lines.
//!   FIX 4: Dollar sums use integer micro-dollars to avoid float drift.

use anyhow::{anyhow, Context};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::Value;

use crate::render::{center, commafy, lr, money, qr_raster, rule, trunc_pub};

const OR_BASE: &str = "https://openrouter.ai/api/v1";
/// Timeout in milliseconds for HTTP operations.
const TIMEOUT_MS: u64 = 20_000;

// ── Public types ────────────────────────────────────────────────────────────

/// Activity window: either the last N days or a specific calendar date.
#[derive(Debug, Clone)]
pub enum ActivityWindow {
    /// Include rows from the last N days relative to the data anchor.
    /// anchor - (N-1) days ≤ row_date ≤ anchor.
    LastDays(u32),
    /// Include only rows whose date matches exactly.
    Day(NaiveDate),
}

impl ActivityWindow {
    /// Human-readable label for the MODEL BREAKDOWN header.
    pub fn label(&self) -> String {
        match self {
            ActivityWindow::LastDays(n) => format!("last {n} days"),
            ActivityWindow::Day(d) => d.format("%Y-%m-%d").to_string(),
        }
    }

    /// True if this row's date falls within the window, given the data anchor.
    ///
    /// `anchor` is the max(date) across all non-future activity rows.
    pub fn includes(&self, row_date: NaiveDate, anchor: NaiveDate) -> bool {
        match self {
            ActivityWindow::LastDays(n) => {
                let cutoff = anchor - chrono::Duration::days((*n as i64).saturating_sub(1));
                row_date >= cutoff && row_date <= anchor
            }
            ActivityWindow::Day(d) => row_date == *d,
        }
    }
}

/// Raw activity row from /activity, before aggregation.
#[derive(Debug, Clone)]
pub struct ActivityRow {
    /// "YYYY-MM-DD" (first 10 chars of the API's "YYYY-MM-DD HH:MM:SS" date field).
    pub date: String,
    pub model: String,
    pub prompt: u64,
    pub completion: u64,
    pub cost: f64,
}

/// Per-model activity row after aggregation.
/// Fields: (model_name, prompt_tokens, completion_tokens, cost_usd)
pub type ModelRow = (String, Option<u64>, Option<u64>, f64);

/// Account-level period spend, derived from /activity rows. All values anchored to data.
#[derive(Debug, Default)]
pub struct PeriodSpend {
    /// Sum of costs on the anchor date (latest non-future date in activity data).
    pub latest_day: f64,
    /// The anchor date itself (for labeling).
    pub anchor_date: NaiveDate,
    /// Sum of costs for rows in [anchor-6, anchor].
    pub last_7d: f64,
    /// Sum of costs for rows in [anchor-29, anchor].
    pub last_30d: f64,
}

#[derive(Debug)]
pub struct OpenRouterStatement {
    pub total_credits: f64,
    pub total_usage: f64,
    pub remaining: f64,
    pub key_label: String,
    /// Key-scoped usage (from /key); kept for reference but not surfaced as account spend.
    pub usage_daily: f64,
    pub usage_weekly: f64,
    pub usage_monthly: f64,
    pub limit_remaining: Option<f64>,
    /// Per-model breakdown from /activity (filtered to window). Empty if unavailable.
    pub models: Vec<ModelRow>,
    /// Whether /activity was available (management key). Drives rendering.
    pub activity_available: bool,
    /// Account-accurate period spend (from activity rows). None when activity unavailable.
    pub period_spend: Option<PeriodSpend>,
    /// The window label, e.g. "last 30 days" or "2026-06-12".
    pub window_label: String,
    /// The data anchor date (max non-future date across all activity rows).
    /// None when activity is unavailable or there are no rows.
    pub anchor_date: Option<NaiveDate>,
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

pub fn fetch_statement(key: &str, window: ActivityWindow) -> anyhow::Result<OpenRouterStatement> {
    let agent = build_agent();
    let window_label = window.label();

    let credits_body = get_json(&agent, &format!("{OR_BASE}/credits"), key)?;
    let (total_credits, total_usage) =
        parse_credits(&credits_body).context("parsing /credits response")?;

    let key_body = get_json(&agent, &format!("{OR_BASE}/key"), key)?;
    let ki = parse_key(&key_body).context("parsing /key response")?;

    let (models, period_spend, activity_available, anchor_date) =
        match get_activity_json(&agent, &format!("{OR_BASE}/activity"), key) {
            Some(body) => {
                let raw = parse_activity_rows(&body);
                if raw.is_empty() {
                    // /activity returned 200 but no usable rows (empty data)
                    (vec![], None, false, None)
                } else {
                    // FIX 1: compute anchor = max(date), excluding future rows
                    let today = Utc::now().date_naive();
                    let tomorrow = today + chrono::Duration::days(1);
                    let anchor = raw
                        .iter()
                        .filter_map(|r| NaiveDate::parse_from_str(&r.date, "%Y-%m-%d").ok())
                        .filter(|d| *d < tomorrow)
                        .max();

                    match anchor {
                        None => (vec![], None, false, None),
                        Some(anchor) => {
                            let spend = compute_period_spend(&raw, anchor);
                            // Filter to window, anchored on data
                            let filtered: Vec<ActivityRow> = raw
                                .into_iter()
                                .filter(|r| {
                                    NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
                                        .map(|d| window.includes(d, anchor))
                                        .unwrap_or(false)
                                })
                                .collect();
                            let models = aggregate_rows(filtered);
                            (models, Some(spend), true, Some(anchor))
                        }
                    }
                }
            }
            None => (vec![], None, false, None),
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
        activity_available,
        period_spend,
        window_label,
        anchor_date,
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

/// Parse the /activity response body into raw rows (best-effort; skips malformed rows).
///
/// The real shape (management key only) is:
///   `{"data": [...rows]}` where each row has:
///   - `date`               (string)  — "YYYY-MM-DD HH:MM:SS"; we take the first 10 chars
///   - `model`              (string)  — e.g. "anthropic/claude-sonnet-4.6"
///   - `prompt_tokens`      (u64)     — input tokens for this day/endpoint slice
///   - `completion_tokens`  (u64)     — output tokens
///   - `usage`              (f64)     — USD cost
///
/// Returns an empty Vec on any error, missing `data`, or non-array body.
pub fn parse_activity_rows(body: &str) -> Vec<ActivityRow> {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let arr = match v.get("data").and_then(|d| d.as_array()) {
        Some(a) => a,
        None => return vec![],
    };

    arr.iter()
        .filter_map(|item| {
            let obj = item.as_object()?;
            let model = obj.get("model").and_then(|m| m.as_str()).filter(|m| !m.is_empty())?;
            // Take the first 10 chars of the date string ("YYYY-MM-DD")
            let date_raw = obj.get("date").and_then(|d| d.as_str()).unwrap_or("");
            let date: String = date_raw.chars().take(10).collect();
            // Validate date is parseable; skip row if malformed
            if NaiveDate::parse_from_str(&date, "%Y-%m-%d").is_err() {
                return None;
            }
            let cost = obj.get("usage").and_then(|u| u.as_f64()).unwrap_or(0.0);
            let prompt = obj.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
            let completion = obj.get("completion_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
            Some(ActivityRow { date, model: model.to_string(), prompt, completion, cost })
        })
        .collect()
}

/// FIX 4: Convert a dollar amount to integer micro-dollars for exact summation.
/// `(v * 1_000_000.0).round() as i64`
#[inline]
fn to_micros(v: f64) -> i64 {
    (v * 1_000_000.0).round() as i64
}

/// FIX 4: Convert micro-dollars back to f64 for display.
#[inline]
fn from_micros(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

/// Aggregate raw rows by model, summing tokens and cost. Returns rows sorted by cost descending.
/// FIX 4: Uses integer micro-dollar accumulation for exact summation.
pub fn aggregate_rows(rows: Vec<ActivityRow>) -> Vec<ModelRow> {
    use std::collections::HashMap;

    // (prompt_tokens, completion_tokens, cost_microdollars)
    let mut agg: HashMap<String, (u64, u64, i64)> = HashMap::new();
    for row in rows {
        let entry = agg.entry(row.model).or_insert((0, 0, 0));
        entry.0 += row.prompt;
        entry.1 += row.completion;
        entry.2 += to_micros(row.cost);
    }

    let mut result: Vec<ModelRow> = agg
        .into_iter()
        .map(|(model, (pt, ct, cost_micros))| {
            let prompt = if pt > 0 { Some(pt) } else { None };
            let completion = if ct > 0 { Some(ct) } else { None };
            (model, prompt, completion, from_micros(cost_micros))
        })
        .collect();

    result.sort_by(|a, b| b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal));
    result
}

/// Compute account-wide period spend from all (unfiltered) activity rows.
/// FIX 1: Anchored on `anchor` (max non-future date), NOT Utc::now.
/// FIX 2: "last_24h" renamed to "latest_day" reflecting the anchor date.
/// FIX 4: Uses integer micro-dollar accumulation.
pub fn compute_period_spend(rows: &[ActivityRow], anchor: NaiveDate) -> PeriodSpend {
    let cutoff_7d = anchor - chrono::Duration::days(6);
    let cutoff_30d = anchor - chrono::Duration::days(29);

    let mut latest_day_micros: i64 = 0;
    let mut last_7d_micros: i64 = 0;
    let mut last_30d_micros: i64 = 0;

    for row in rows {
        let d = match NaiveDate::parse_from_str(&row.date, "%Y-%m-%d") {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Skip future-dated rows (they were excluded from anchor computation too)
        let tomorrow = Utc::now().date_naive() + chrono::Duration::days(1);
        if d >= tomorrow {
            continue;
        }
        let micros = to_micros(row.cost);
        if d == anchor {
            latest_day_micros += micros;
        }
        if d >= cutoff_7d && d <= anchor {
            last_7d_micros += micros;
        }
        if d >= cutoff_30d && d <= anchor {
            last_30d_micros += micros;
        }
    }

    PeriodSpend {
        latest_day: from_micros(latest_day_micros),
        anchor_date: anchor,
        last_7d: from_micros(last_7d_micros),
        last_30d: from_micros(last_30d_micros),
    }
}

/// Legacy compatibility: aggregate ALL rows (no window filter). Kept for tests that call it directly.
pub fn parse_activity(body: &str) -> Vec<ModelRow> {
    aggregate_rows(parse_activity_rows(body))
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

    // FIX 1: Surface the anchor date so staleness is visible.
    if let Some(anchor) = stmt.anchor_date {
        push(&mut o, lr(" Activity through", &format!("{} ", anchor.format("%Y-%m-%d"))));
    }

    // MODEL BREAKDOWN — aggregated from /activity, filtered to the requested window.
    // Note: /activity covers ~last 30 days; CREDITS total_usage below is lifetime.
    push(&mut o, rule('-'));
    let breakdown_header = format!(" MODEL BREAKDOWN  ({})", stmt.window_label);
    let breakdown_trunc: String = breakdown_header.chars().take(48).collect();
    push(&mut o, breakdown_trunc);
    push(&mut o, rule('-'));

    if !stmt.activity_available {
        // FIX 3: Clearly indicate unavailability instead of showing $0 rows.
        push(&mut o, " unavailable — mgmt key required".into());
    } else if stmt.models.is_empty() {
        push(&mut o, " (no activity in window)".into());
    } else {
        let top: Vec<_> = stmt.models.iter().take(10).collect();
        let rest = stmt.models.len().saturating_sub(10);
        let rest_cost: f64 = stmt.models.iter().skip(10).map(|(_, _, _, c)| c).sum();
        for (model, pt, ct, cost) in &top {
            let cost_str = format!("{} ", money(*cost));
            let name_budget = 48usize
                .saturating_sub(1)
                .saturating_sub(cost_str.chars().count());
            let name = trunc_pub(model, name_budget);
            push(&mut o, lr(&format!(" {name}"), &cost_str));
            match (pt, ct) {
                (Some(p), Some(c)) => {
                    let tok_line = format!("  {} + {} tok", commafy(*p), commafy(*c));
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
        // FIX 2: Footer note that model breakdown is usage activity, not billing invoice.
        push(&mut o, rule('-'));
        push(&mut o, "(usage activity; see CREDITS for billing)".into());
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

    // FIX 2 + FIX 3: Period spend only when activity is available.
    // Labels are anchored to data, not wall clock.
    if let Some(spend) = &stmt.period_spend {
        let latest_label = format!("Latest day ({})", spend.anchor_date.format("%Y-%m-%d"));
        // Truncate label to fit 48-col receipt (leaving room for value)
        let val_latest = format!("{} ", money(spend.latest_day));
        let lbl_budget = 48usize.saturating_sub(val_latest.chars().count()).saturating_sub(1);
        let latest_label_trunc: String = format!("   {latest_label}").chars().take(lbl_budget + 3).collect();
        push(&mut o, lr(&latest_label_trunc, &val_latest));
        push(&mut o, lr("   Last 7d", &format!("{} ", money(spend.last_7d))));
        push(&mut o, lr("   Last 30d", &format!("{} ", money(spend.last_30d))));
    }
    // FIX 3: When activity is unavailable, period spend lines are OMITTED entirely.
    // We do not fall back to key-scoped usage_daily/weekly/monthly as account figures.

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

    /// Multi-day, multi-model fixture for window-filtering tests.
    /// Dates: 2026-06-10, 2026-06-11, 2026-06-12 — three distinct days.
    const ACTIVITY_MULTIDAY_JSON: &str = r#"{
        "data": [
            {
                "date": "2026-06-10 00:00:00",
                "model": "anthropic/claude-sonnet-4.6",
                "usage": 2.00,
                "prompt_tokens": 15000,
                "completion_tokens": 600
            },
            {
                "date": "2026-06-10 00:00:00",
                "model": "openai/gpt-4o",
                "usage": 0.50,
                "prompt_tokens": 2000,
                "completion_tokens": 100
            },
            {
                "date": "2026-06-11 00:00:00",
                "model": "anthropic/claude-sonnet-4.6",
                "usage": 3.00,
                "prompt_tokens": 20000,
                "completion_tokens": 800
            },
            {
                "date": "2026-06-12 00:00:00",
                "model": "anthropic/claude-sonnet-4.6",
                "usage": 5.50,
                "prompt_tokens": 35000,
                "completion_tokens": 1200
            },
            {
                "date": "2026-06-12 00:00:00",
                "model": "openai/gpt-4o",
                "usage": 0.75,
                "prompt_tokens": 3000,
                "completion_tokens": 250
            }
        ]
    }"#;

    /// Fixture with a far-future date that should be quarantined from anchor computation.
    const ACTIVITY_FUTURE_DATE_JSON: &str = r#"{
        "data": [
            {
                "date": "2026-06-10 00:00:00",
                "model": "anthropic/claude-sonnet-4.6",
                "usage": 2.00,
                "prompt_tokens": 15000,
                "completion_tokens": 600
            },
            {
                "date": "2026-06-12 00:00:00",
                "model": "openai/gpt-4o",
                "usage": 0.75,
                "prompt_tokens": 3000,
                "completion_tokens": 250
            },
            {
                "date": "2099-12-31 00:00:00",
                "model": "garbage/future-model",
                "usage": 999.99,
                "prompt_tokens": 1000000,
                "completion_tokens": 500000
            }
        ]
    }"#;

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

    // ── parse_activity_rows tests ─────────────────────────────────────────────

    #[test]
    fn parse_activity_rows_extracts_correct_fields() {
        let rows = parse_activity_rows(ACTIVITY_MGMT_JSON);
        assert_eq!(rows.len(), 2, "should parse 2 rows");
        // Verify date truncation (first 10 chars of "2026-06-12 00:00:00")
        assert!(rows.iter().all(|r| r.date == "2026-06-12"));
        // Find claude-opus-4 row
        let opus = rows.iter().find(|r| r.model == "anthropic/claude-opus-4").unwrap();
        assert_eq!(opus.prompt, 10000);
        assert_eq!(opus.completion, 500);
        assert!((opus.cost - 5.12).abs() < 1e-9);
    }

    #[test]
    fn parse_activity_rows_multiday_fixture_parses_all() {
        let rows = parse_activity_rows(ACTIVITY_MULTIDAY_JSON);
        assert_eq!(rows.len(), 5, "should parse all 5 rows from multiday fixture");
        // Verify all three dates are represented
        assert!(rows.iter().any(|r| r.date == "2026-06-10"));
        assert!(rows.iter().any(|r| r.date == "2026-06-11"));
        assert!(rows.iter().any(|r| r.date == "2026-06-12"));
    }

    #[test]
    fn parse_activity_rows_skips_malformed_missing_model() {
        let json = r#"{"data": [
            {"date": "2026-06-12 00:00:00", "usage": 1.0, "prompt_tokens": 100, "completion_tokens": 50},
            {"date": "2026-06-12 00:00:00", "model": "good/model", "usage": 2.0, "prompt_tokens": 200, "completion_tokens": 80}
        ]}"#;
        let rows = parse_activity_rows(json);
        // Row without model is skipped; row with model is included
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].model, "good/model");
    }

    #[test]
    fn parse_activity_rows_garbage_returns_empty() {
        assert!(parse_activity_rows("not json").is_empty());
        assert!(parse_activity_rows(r#"{"data": "not an array"}"#).is_empty());
    }

    // ── aggregate_rows tests ──────────────────────────────────────────────────

    #[test]
    fn aggregate_rows_sums_same_model_across_days() {
        // Two rows for same model on different days → aggregated into one.
        let rows = vec![
            ActivityRow { date: "2026-06-11".into(), model: "a/model".into(), prompt: 1000, completion: 100, cost: 1.0 },
            ActivityRow { date: "2026-06-12".into(), model: "a/model".into(), prompt: 2000, completion: 200, cost: 2.5 },
            ActivityRow { date: "2026-06-12".into(), model: "b/model".into(), prompt: 500, completion: 50, cost: 0.5 },
        ];
        let result = aggregate_rows(rows);
        assert_eq!(result.len(), 2);
        // Sorted by cost desc: a/model (3.5) before b/model (0.5)
        let (model0, pt0, ct0, cost0) = &result[0];
        assert_eq!(model0, "a/model");
        assert_eq!(*pt0, Some(3000));
        assert_eq!(*ct0, Some(300));
        assert!((cost0 - 3.5).abs() < 1e-9);
    }

    // ── FIX 1: Anchor tests ───────────────────────────────────────────────────

    #[test]
    fn anchor_is_max_non_future_date() {
        // The fixture has 2026-06-10, 2026-06-12, and 2099-12-31 (future/garbage).
        // Anchor must be 2026-06-12, not 2099-12-31.
        let rows = parse_activity_rows(ACTIVITY_FUTURE_DATE_JSON);
        let today = Utc::now().date_naive();
        let tomorrow = today + chrono::Duration::days(1);
        let anchor = rows
            .iter()
            .filter_map(|r| NaiveDate::parse_from_str(&r.date, "%Y-%m-%d").ok())
            .filter(|d| *d < tomorrow)
            .max()
            .expect("should have a valid anchor");
        assert_eq!(anchor, NaiveDate::from_ymd_opt(2026, 6, 12).unwrap(),
            "anchor should be 2026-06-12, not the future-dated row");
    }

    #[test]
    fn future_row_does_not_become_anchor() {
        // Explicit: a fixture where the ONLY non-future data is 2026-06-10.
        let json = r#"{"data": [
            {"date": "2026-06-10 00:00:00", "model": "a/b", "usage": 1.0,
             "prompt_tokens": 100, "completion_tokens": 50},
            {"date": "2099-01-01 00:00:00", "model": "c/d", "usage": 999.0,
             "prompt_tokens": 1000, "completion_tokens": 500}
        ]}"#;
        let rows = parse_activity_rows(json);
        let today = Utc::now().date_naive();
        let tomorrow = today + chrono::Duration::days(1);
        let anchor = rows
            .iter()
            .filter_map(|r| NaiveDate::parse_from_str(&r.date, "%Y-%m-%d").ok())
            .filter(|d| *d < tomorrow)
            .max()
            .expect("should find an anchor");
        assert_eq!(anchor, NaiveDate::from_ymd_opt(2026, 6, 10).unwrap());
    }

    // ── FIX 1: Window filtering anchored on data ──────────────────────────────

    #[test]
    fn window_last_days_anchored_on_data_not_clock() {
        // anchor = 2026-06-12 (max date in fixture).
        // LastDays(3): [anchor-2, anchor] = [2026-06-10, 2026-06-12].
        // All 5 rows from the fixture fall in this range.
        let rows = parse_activity_rows(ACTIVITY_MULTIDAY_JSON);
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let window = ActivityWindow::LastDays(3);
        let filtered: Vec<_> = rows
            .iter()
            .filter(|r| {
                NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
                    .map(|d| window.includes(d, anchor))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(filtered.len(), 5, "LastDays(3) from anchor 2026-06-12 should include all 5 rows");
    }

    #[test]
    fn window_last_days_7_from_anchor_excludes_older_rows() {
        // anchor = 2026-06-12. LastDays(7): [2026-06-06, 2026-06-12].
        // All fixture dates (2026-06-10, 2026-06-11, 2026-06-12) are in range.
        let rows = parse_activity_rows(ACTIVITY_MULTIDAY_JSON);
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let window = ActivityWindow::LastDays(7);
        let filtered: Vec<_> = rows
            .iter()
            .filter(|r| {
                NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
                    .map(|d| window.includes(d, anchor))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(filtered.len(), 5, "LastDays(7) from 2026-06-12 should include all fixture rows");

        // Now test that a row older than the window is excluded.
        let old_row = ActivityRow {
            date: "2026-06-01".into(),
            model: "x/y".into(),
            prompt: 0,
            completion: 0,
            cost: 1.0,
        };
        let includes_old = window.includes(
            NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
            anchor,
        );
        assert!(!includes_old, "2026-06-01 should be outside LastDays(7) from anchor 2026-06-12");
        drop(old_row);
    }

    #[test]
    fn window_day_filters_exact_date() {
        let rows = parse_activity_rows(ACTIVITY_MULTIDAY_JSON);
        let target = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let window = ActivityWindow::Day(target);
        let filtered: Vec<ActivityRow> = rows
            .into_iter()
            .filter(|r| {
                NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
                    .map(|d| window.includes(d, target))
                    .unwrap_or(false)
            })
            .collect();
        // Only 2026-06-12 rows: 2 rows (one per model)
        assert_eq!(filtered.len(), 2, "Day filter should select exactly the 2 rows on 2026-06-12");
        assert!(filtered.iter().all(|r| r.date == "2026-06-12"));
        let aggregated = aggregate_rows(filtered);
        // claude-sonnet-4.6: $5.50, gpt-4o: $0.75
        assert_eq!(aggregated.len(), 2);
        let total: f64 = aggregated.iter().map(|(_, _, _, c)| c).sum();
        assert!((total - 6.25).abs() < 1e-9, "total for 2026-06-12 should be $6.25, got {total}");
    }

    #[test]
    fn window_days_1_selects_most_recent_only() {
        // With LastDays(1), cutoff = anchor - 0 = anchor. Only anchor date rows.
        let rows = parse_activity_rows(ACTIVITY_MULTIDAY_JSON);
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let window = ActivityWindow::LastDays(1);
        let filtered: Vec<_> = rows
            .iter()
            .filter(|r| {
                NaiveDate::parse_from_str(&r.date, "%Y-%m-%d")
                    .map(|d| window.includes(d, anchor))
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(filtered.len(), 2, "LastDays(1) with anchor 2026-06-12 should select 2 rows (that date only)");
        assert!(filtered.iter().all(|r| r.date == "2026-06-12"));
    }

    #[test]
    fn window_label_last_days() {
        assert_eq!(ActivityWindow::LastDays(30).label(), "last 30 days");
        assert_eq!(ActivityWindow::LastDays(7).label(), "last 7 days");
        assert_eq!(ActivityWindow::LastDays(1).label(), "last 1 days");
    }

    #[test]
    fn window_label_day() {
        let d = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        assert_eq!(ActivityWindow::Day(d).label(), "2026-06-12");
    }

    // ── FIX 2: compute_period_spend anchored on data ──────────────────────────

    #[test]
    fn compute_period_spend_latest_day_is_anchor_date() {
        // Fixture has dates 2026-06-10, 2026-06-11, 2026-06-12.
        // Anchor = 2026-06-12. latest_day = sum of 2026-06-12 rows.
        let rows = parse_activity_rows(ACTIVITY_MULTIDAY_JSON);
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let spend = compute_period_spend(&rows, anchor);
        assert_eq!(spend.anchor_date, anchor);
        // 2026-06-12: claude ($5.50) + gpt-4o ($0.75) = $6.25
        assert!((spend.latest_day - 6.25).abs() < 1e-9,
            "latest_day should be $6.25 (sum of 2026-06-12 rows), got {}", spend.latest_day);
    }

    #[test]
    fn compute_period_spend_7d_anchored_on_data() {
        // anchor = 2026-06-12, 7d window = [2026-06-06, 2026-06-12].
        // All fixture rows fall in window: 2+0.5+3+5.5+0.75 = 11.75
        let rows = parse_activity_rows(ACTIVITY_MULTIDAY_JSON);
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let spend = compute_period_spend(&rows, anchor);
        assert!((spend.last_7d - 11.75).abs() < 1e-9,
            "last_7d should be $11.75 (all fixture rows), got {}", spend.last_7d);
    }

    #[test]
    fn compute_period_spend_empty_rows() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let spend = compute_period_spend(&[], anchor);
        assert!((spend.latest_day).abs() < 1e-9);
        assert!((spend.last_7d).abs() < 1e-9);
        assert!((spend.last_30d).abs() < 1e-9);
    }

    // ── FIX 2: Label "Latest day" not "Last 24h" ─────────────────────────────

    #[test]
    fn render_text_latest_day_label_present_last_24h_absent() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let mut stmt = sample_statement();
        stmt.activity_available = true;
        stmt.period_spend = Some(PeriodSpend {
            latest_day: 6.25,
            anchor_date: anchor,
            last_7d: 14.5,
            last_30d: 137.14,
        });
        stmt.anchor_date = Some(anchor);
        let s = render_statement_text(&stmt, sample_when());
        assert!(s.contains("Latest day"), "should show 'Latest day' label (FIX 2)");
        assert!(s.contains("2026-06-12"), "should show anchor date in Latest day label");
        assert!(!s.contains("Last 24h"), "should NOT contain 'Last 24h' (FIX 2)");
        assert!(s.contains("Last 7d"), "should show Last 7d");
        assert!(s.contains("Last 30d"), "should show Last 30d");
    }

    // ── FIX 3: Unavailable activity ───────────────────────────────────────────

    #[test]
    fn render_text_unavailable_shows_mgmt_key_message_not_zero() {
        let mut stmt = sample_statement();
        stmt.activity_available = false;
        stmt.models = vec![];
        stmt.period_spend = None;
        let s = render_statement_text(&stmt, sample_when());
        // Must say unavailable
        assert!(s.contains("unavailable"), "should indicate activity unavailable (FIX 3)");
        // Must NOT show $0.00 period spend lines
        assert!(!s.contains("Latest day"), "should NOT show Latest day when unavailable (FIX 3)");
        assert!(!s.contains("Last 24h"), "should NOT show Last 24h when unavailable (FIX 3)");
        assert!(!s.contains("Last 7d"), "should NOT show Last 7d period spend when unavailable (FIX 3)");
        assert!(!s.contains("Last 30d"), "should NOT show Last 30d period spend when unavailable (FIX 3)");
        // CREDITS must still be present
        assert!(s.contains("CREDITS"), "CREDITS section must always be present (FIX 3)");
        assert!(s.contains("Total used"), "Total used must always be present (FIX 3)");
    }

    #[test]
    fn render_text_unavailable_period_spend_not_zero_dollars() {
        // Regression: when activity unavailable, must not show any "$0.00" period lines.
        let mut stmt = sample_statement();
        stmt.activity_available = false;
        stmt.models = vec![];
        stmt.period_spend = None;
        stmt.usage_daily = 0.0;
        stmt.usage_weekly = 0.0;
        stmt.usage_monthly = 0.0;
        let s = render_statement_text(&stmt, sample_when());
        // These specific $0.00 period lines must not appear
        assert!(!s.contains("Last 24h"), "no Last 24h $0 line");
        assert!(!s.contains("Last 7d"), "no Last 7d $0 line");
        assert!(!s.contains("Last 30d"), "no Last 30d $0 line");
        assert!(!s.contains("Key: today"), "no key fallback period lines (omit-by-default, FIX 3)");
    }

    // ── FIX 4: Exact micro-dollar summation ───────────────────────────────────

    #[test]
    fn microdollar_sum_exact_no_float_drift() {
        // Many small values (0.037629 each) accumulated via integer path must equal
        // the expected total exactly (to within 1 micro-dollar display precision).
        let unit_cost: f64 = 0.037629;
        let n: usize = 1000;
        let rows: Vec<ActivityRow> = (0..n)
            .map(|_| ActivityRow {
                date: "2026-06-12".into(),
                model: "a/b".into(),
                prompt: 10,
                completion: 5,
                cost: unit_cost,
            })
            .collect();
        // All same model, so aggregate into one.
        let result = aggregate_rows(rows);
        assert_eq!(result.len(), 1);
        let (_, _, _, total_cost) = &result[0];

        // Integer path: to_micros(0.037629) = 37629 each; × 1000 = 37_629_000 micros = $37.629000
        let expected_micros: i64 = to_micros(unit_cost) * (n as i64);
        let expected = from_micros(expected_micros);

        assert!(
            (total_cost - expected).abs() < 1e-9,
            "micro-dollar sum: got {total_cost:.9}, expected {expected:.9}"
        );

        // Also confirm the naive f64 path would differ (showing why FIX 4 matters).
        let naive: f64 = (0..n).map(|_| unit_cost).sum();
        // naive may differ from expected at the sub-cent level
        let _ = naive; // used only to document the comparison intent
    }

    #[test]
    fn microdollar_roundtrip_exact() {
        // to_micros / from_micros roundtrip for values with up to 6 decimal places.
        for &v in &[0.0, 0.000001, 0.037629, 1.0, 5.12, 137.141032] {
            let micros = to_micros(v);
            let back = from_micros(micros);
            assert!(
                (back - v).abs() < 5e-7,
                "roundtrip failed for {v}: got {back}"
            );
        }
    }

    // ── Existing parse_activity compatibility tests ───────────────────────────

    #[test]
    fn parse_activity_management_key_shape() {
        let rows = parse_activity(ACTIVITY_MGMT_JSON);
        assert_eq!(rows.len(), 2);
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
        assert_eq!(rows.len(), 2, "expected 2 aggregated models");

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
            activity_available: false,
            period_spend: None,
            window_label: "last 30 days".into(),
            anchor_date: None,
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
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let mut stmt = sample_statement();
        stmt.activity_available = true;
        stmt.anchor_date = Some(anchor);
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
    fn render_text_with_period_spend_fits_48_cols() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let mut stmt = sample_statement();
        stmt.activity_available = true;
        stmt.anchor_date = Some(anchor);
        stmt.period_spend = Some(PeriodSpend {
            latest_day: 6.25,
            anchor_date: anchor,
            last_7d: 14.5,
            last_30d: 137.14,
        });
        let s = render_statement_text(&stmt, sample_when());
        assert!(s.contains("Latest day"), "should show Latest day");
        assert!(!s.contains("Last 24h"), "should NOT show Last 24h (FIX 2)");
        assert!(s.contains("Last 7d"), "should show Last 7d");
        assert!(s.contains("Last 30d"), "should show Last 30d");
        for line in s.lines() {
            assert!(
                line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn render_text_fallback_key_spend_fits_48_cols() {
        // When activity unavailable, period spend is omitted (not key-scoped fallback).
        let mut stmt = sample_statement();
        stmt.activity_available = false;
        stmt.period_spend = None;
        stmt.usage_daily = 1.23;
        stmt.usage_weekly = 4.56;
        stmt.usage_monthly = 137.14;
        let s = render_statement_text(&stmt, sample_when());
        // Key-scoped lines should be omitted (omit-by-default, FIX 3)
        assert!(!s.contains("Key: today"), "key-scoped lines omitted when activity unavailable");
        assert!(s.contains("CREDITS"), "CREDITS section always present");
        for line in s.lines() {
            assert!(
                line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn render_text_date_window_header_fits_48_cols() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let mut stmt = sample_statement();
        stmt.window_label = "2026-06-12".into();
        stmt.activity_available = true;
        stmt.anchor_date = Some(anchor);
        stmt.models = vec![
            ("anthropic/claude-sonnet-4.6".into(), Some(35_000), Some(1_200), 5.50),
        ];
        let s = render_statement_text(&stmt, sample_when());
        assert!(s.contains("2026-06-12"), "should show date in header");
        for line in s.lines() {
            assert!(
                line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn render_text_activity_through_line_present_when_anchor_set() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let mut stmt = sample_statement();
        stmt.anchor_date = Some(anchor);
        let s = render_statement_text(&stmt, sample_when());
        assert!(s.contains("Activity through"), "should show 'Activity through' line (FIX 1)");
        assert!(s.contains("2026-06-12"), "should show anchor date in 'Activity through' line");
    }

    #[test]
    fn render_text_no_activity_through_when_unavailable() {
        let mut stmt = sample_statement();
        stmt.anchor_date = None;
        let s = render_statement_text(&stmt, sample_when());
        assert!(!s.contains("Activity through"), "no activity-through line when anchor absent");
    }

    #[test]
    fn render_text_usage_activity_note_present_in_breakdown() {
        let anchor = NaiveDate::from_ymd_opt(2026, 6, 12).unwrap();
        let mut stmt = sample_statement();
        stmt.activity_available = true;
        stmt.anchor_date = Some(anchor);
        stmt.models = vec![
            ("anthropic/claude-sonnet-4.6".into(), Some(10_000), Some(500), 2.50),
        ];
        let s = render_statement_text(&stmt, sample_when());
        assert!(s.contains("usage activity"), "should show usage activity note (FIX 2)");
        assert!(s.contains("CREDITS"), "CREDITS note should be referenced");
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

    // ── Render 48-col guard for unavailable path ──────────────────────────────

    #[test]
    fn render_unavailable_all_lines_fit_48_cols() {
        let stmt = sample_statement(); // activity_available=false, no models, no period_spend
        let s = render_statement_text(&stmt, sample_when());
        for line in s.lines() {
            assert!(
                line.chars().count() <= 48,
                "line too wide ({} chars): {line:?}",
                line.chars().count()
            );
        }
    }
}
