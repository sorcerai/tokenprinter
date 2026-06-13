use std::collections::HashMap;
use serde::Deserialize;
use crate::model::{UsageRecord, CacheTtl};

#[derive(Debug, Clone, Deserialize)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    #[serde(default)] pub cache_write_5m: f64,
    #[serde(default)] pub cache_write_1h: f64,
    #[serde(default)] pub cache_read: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelPrice {
    #[serde(flatten)] pub base: Rates,
    #[serde(default)] pub tier_over_200k: Option<Rates>,
}

#[derive(Debug, Clone)]
pub struct PriceTable(HashMap<String, ModelPrice>);

const PER_M: f64 = 1_000_000.0;

impl PriceTable {
    pub fn embedded() -> Self {
        let raw = include_str!("prices.json");
        Self(serde_json::from_str(raw).expect("embedded prices.json valid"))
    }

    pub fn load(path: &std::path::Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)?;
        Ok(Self(serde_json::from_str(&raw)?))
    }

    pub fn from_json(raw: &str) -> anyhow::Result<Self> {
        Ok(Self(serde_json::from_str(raw)?))
    }

    fn normalize_key(model: &str) -> &str {
        model.split('[').next().unwrap_or(model).trim()
    }

    fn rates_for<'a>(&'a self, model: &str, ctx: u64) -> Option<&'a Rates> {
        let mp = self.0.get(Self::normalize_key(model))?;
        if ctx > 200_000 {
            if let Some(t) = &mp.tier_over_200k { return Some(t); }
        }
        Some(&mp.base)
    }

    pub fn cost_for(&self, r: &UsageRecord) -> Option<f64> {
        if let Some(c) = r.cost { return Some(c); }
        let rates = self.rates_for(&r.model, r.context_size)?;
        let cw_rate = match r.cache_write_ttl {
            CacheTtl::FiveMin => rates.cache_write_5m,
            CacheTtl::OneHour => rates.cache_write_1h,
        };
        let cost = r.input as f64 / PER_M * rates.input
            + r.output as f64 / PER_M * rates.output
            + r.cache_write as f64 / PER_M * cw_rate
            + r.cache_read as f64 / PER_M * rates.cache_read;
        Some(cost)
    }

    pub fn cache_savings(&self, r: &UsageRecord) -> Option<f64> {
        let rates = self.rates_for(&r.model, r.context_size)?;
        Some(r.cache_read as f64 / PER_M * (rates.input - rates.cache_read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{UsageRecord, Agent, CacheTtl};

    fn rec(model: &str, input: u64, output: u64, cw: u64, cr: u64, ctx: u64) -> UsageRecord {
        let mut r = UsageRecord::zeroed(Agent::Claude, model);
        r.input = input; r.output = output; r.cache_write = cw; r.cache_read = cr;
        r.context_size = ctx; r.cache_write_ttl = CacheTtl::FiveMin; r
    }

    #[test]
    fn per_category_cost_base_tier() {
        let t = PriceTable::embedded();
        // opus-4-8: input=5.0 + output=25.0 + cache_write_5m=6.25 + cache_read=0.5 = 36.75
        let c = t.cost_for(&rec("claude-opus-4-8", 1_000_000, 1_000_000, 1_000_000, 1_000_000, 50_000)).unwrap();
        assert!((c - 36.75).abs() < 1e-6, "got {c}");
    }

    #[test]
    fn long_context_uses_over_200k_tier() {
        let raw = r#"{ "tiered-test": { "input": 1.0, "output": 1.0, "cache_write_5m": 1.0, "cache_write_1h": 1.0, "cache_read": 1.0,
            "tier_over_200k": { "input": 2.0, "output": 2.0, "cache_write_5m": 2.0, "cache_write_1h": 2.0, "cache_read": 2.0 } } }"#;
        let t = PriceTable::from_json(raw).unwrap();
        // 1M input at ctx 300k -> tier input 2.0
        assert!((t.cost_for(&rec("tiered-test", 1_000_000, 0, 0, 0, 300_000)).unwrap() - 2.0).abs() < 1e-6);
        // 1M input at ctx 50k -> base input 1.0
        assert!((t.cost_for(&rec("tiered-test", 1_000_000, 0, 0, 0, 50_000)).unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn one_m_suffix_normalizes_to_base_model() {
        let t = PriceTable::embedded();
        // opus-4-8: input=5.0
        let c = t.cost_for(&rec("claude-opus-4-8[1m]", 1_000_000, 0, 0, 0, 50_000)).unwrap();
        assert!((c - 5.0).abs() < 1e-6, "got {c}");
    }

    #[test]
    fn tool_reported_cost_wins() {
        let t = PriceTable::embedded();
        let mut r = rec("anything-unknown", 1, 1, 1, 1, 1);
        r.cost = Some(4.2);
        assert_eq!(t.cost_for(&r), Some(4.2));
    }

    #[test]
    fn unknown_model_returns_none() {
        let t = PriceTable::embedded();
        assert_eq!(t.cost_for(&rec("totally-unknown", 1, 1, 1, 1, 1)), None);
    }
}
