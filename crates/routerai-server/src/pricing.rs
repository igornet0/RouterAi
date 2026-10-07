//! Model prices from an explicit, operator-maintained price sheet.
//!
//! The server never invents prices: every rate comes from `pricing.toml` (or the
//! file named by `ROUTERAI_PRICING_FILE`) together with where it was taken from
//! and when it was last checked. A model without an entry is refused before any
//! request is sent (`PricingUnavailable`) — never treated as free.
//!
//! ```toml
//! [[price]]
//! provider = "openai"
//! model = "gpt-4o-mini"
//! input_per_million = "0.15"           # USD, written as strings (exact decimals)
//! output_per_million = "0.60"
//! cached_input_per_million = "0.075"   # optional
//! source = "https://openai.com/api/pricing"
//! as_of = "2026-10-01"                 # when the rate was last verified
//! ```
//!
//! Optional rates: `cached_input_per_million`, `cache_write_per_million`,
//! `reasoning_per_million` (see `universal_ai::ModelPricing::rate` for how a
//! missing one is handled). A self-hosted model is free only when its sheet says
//! so (`"0"` rates).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, TimeZone, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use universal_ai::{AiClient, ModelId, ModelPricing, ProviderId};

/// Rates older than this are reported (not rejected) at startup.
const STALE_AFTER_DAYS: i64 = 90;

/// Default sheet location inside the data directory.
pub fn default_pricing_path(data_dir: &Path) -> PathBuf {
    data_dir.join("pricing.toml")
}

/// Parsed and validated price sheet.
#[derive(Debug, Clone, Default, Serialize)]
pub struct PriceSheet {
    pub prices: Vec<PriceEntry>,
}

/// One model's rates with their provenance.
#[derive(Debug, Clone, Serialize)]
pub struct PriceEntry {
    pub provider: String,
    pub model: String,
    pub input_per_million: Decimal,
    pub output_per_million: Decimal,
    pub cached_input_per_million: Option<Decimal>,
    pub cache_write_per_million: Option<Decimal>,
    pub reasoning_per_million: Option<Decimal>,
    /// Where the rates were taken from (URL, contract, invoice, …).
    pub source: String,
    /// When the rates were last verified.
    pub as_of: NaiveDate,
}

impl PriceEntry {
    fn to_model_pricing(&self) -> ModelPricing {
        ModelPricing {
            provider: ProviderId::new(self.provider.clone()),
            model: ModelId::new(self.model.clone()),
            input_per_million: Some(self.input_per_million),
            output_per_million: Some(self.output_per_million),
            cached_input_per_million: self.cached_input_per_million,
            cache_write_per_million: self.cache_write_per_million,
            reasoning_per_million: self.reasoning_per_million,
            effective_from: Utc.from_utc_datetime(&self.as_of.and_hms_opt(0, 0, 0).unwrap()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSheet {
    #[serde(default)]
    price: Vec<RawEntry>,
}

/// File format: every money value is a string (no float rounding) and unknown
/// keys are errors, so a typo cannot silently drop a rate.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    provider: String,
    model: String,
    input_per_million: String,
    output_per_million: String,
    cached_input_per_million: Option<String>,
    cache_write_per_million: Option<String>,
    reasoning_per_million: Option<String>,
    source: String,
    as_of: String,
}

fn rate(entry: usize, field: &str, value: &str) -> Result<Decimal, String> {
    let d: Decimal = value
        .trim()
        .parse()
        .map_err(|e| format!("price[{entry}].{field}: invalid decimal {value:?}: {e}"))?;
    if d.is_sign_negative() {
        return Err(format!("price[{entry}].{field}: negative rate {d}"));
    }
    Ok(d)
}

/// Parse and validate a price sheet. `today` bounds `as_of` (no future dates).
pub fn parse_price_sheet(text: &str, today: NaiveDate) -> Result<PriceSheet, String> {
    let raw: RawSheet = toml::from_str(text).map_err(|e| format!("invalid price sheet: {e}"))?;
    let mut seen = HashSet::new();
    let mut prices = Vec::with_capacity(raw.price.len());
    for (i, e) in raw.price.into_iter().enumerate() {
        let provider = e.provider.trim().to_string();
        let model = e.model.trim().to_string();
        if provider.is_empty() || model.is_empty() {
            return Err(format!("price[{i}]: provider and model are required"));
        }
        if e.source.trim().is_empty() {
            return Err(format!(
                "price[{i}] ({provider}/{model}): source is required (where the rate comes from)"
            ));
        }
        let as_of = NaiveDate::parse_from_str(e.as_of.trim(), "%Y-%m-%d").map_err(|err| {
            format!("price[{i}] ({provider}/{model}): as_of must be YYYY-MM-DD: {err}")
        })?;
        if as_of > today {
            return Err(format!(
                "price[{i}] ({provider}/{model}): as_of {as_of} is in the future"
            ));
        }
        if !seen.insert((provider.clone(), model.clone())) {
            return Err(format!(
                "price[{i}]: duplicate entry for {provider}/{model}"
            ));
        }
        let opt =
            |field: &str, v: &Option<String>| v.as_deref().map(|v| rate(i, field, v)).transpose();
        prices.push(PriceEntry {
            input_per_million: rate(i, "input_per_million", &e.input_per_million)?,
            output_per_million: rate(i, "output_per_million", &e.output_per_million)?,
            cached_input_per_million: opt("cached_input_per_million", &e.cached_input_per_million)?,
            cache_write_per_million: opt("cache_write_per_million", &e.cache_write_per_million)?,
            reasoning_per_million: opt("reasoning_per_million", &e.reasoning_per_million)?,
            source: e.source.trim().to_string(),
            as_of,
            provider,
            model,
        });
    }
    Ok(PriceSheet { prices })
}

/// Load the sheet at `path`. A missing file is an empty sheet (every model
/// request will be refused with `PricingUnavailable`); an unreadable or invalid
/// file is an error — the server must not start with half-understood prices.
pub async fn load_price_sheet(path: &Path) -> Result<PriceSheet, String> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::warn!(
                path = %path.display(),
                "no price sheet: every model request will be refused (PricingUnavailable) \
                 until prices are configured"
            );
            return Ok(PriceSheet::default());
        }
        Err(e) => return Err(format!("cannot read price sheet {}: {e}", path.display())),
    };
    let sheet = parse_price_sheet(&text, Utc::now().date_naive())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let today = Utc::now().date_naive();
    for p in &sheet.prices {
        let age = (today - p.as_of).num_days();
        if age > STALE_AFTER_DAYS {
            tracing::warn!(
                provider = %p.provider,
                model = %p.model,
                as_of = %p.as_of,
                age_days = age,
                source = %p.source,
                "price entry has not been verified for a long time"
            );
        }
    }
    tracing::info!(path = %path.display(), entries = sheet.prices.len(), "price sheet loaded");
    Ok(sheet)
}

/// Register every entry of `sheet` with the client's pricing registry.
pub fn apply_price_sheet(ai: &AiClient, sheet: &PriceSheet) -> Result<(), String> {
    for p in &sheet.prices {
        ai.pricing()
            .try_upsert(p.to_model_pricing())
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 7).unwrap()
    }

    const VALID: &str = r#"
[[price]]
provider = "openai"
model = "gpt-4o-mini"
input_per_million = "0.15"
output_per_million = "0.60"
cached_input_per_million = "0.075"
source = "https://openai.com/api/pricing"
as_of = "2026-10-01"

[[price]]
provider = "openai-compatible"
model = "local-llama"
input_per_million = "0"
output_per_million = "0"
source = "self-hosted"
as_of = "2026-10-07"
"#;

    #[test]
    fn valid_sheet_keeps_exact_rates_and_provenance() {
        let sheet = parse_price_sheet(VALID, today()).unwrap();
        assert_eq!(sheet.prices.len(), 2);
        let p = &sheet.prices[0];
        assert_eq!(p.input_per_million, "0.15".parse::<Decimal>().unwrap());
        assert_eq!(p.cached_input_per_million, Some("0.075".parse().unwrap()));
        assert_eq!(p.reasoning_per_million, None);
        assert_eq!(p.source, "https://openai.com/api/pricing");
        assert_eq!(p.as_of, NaiveDate::from_ymd_opt(2026, 10, 1).unwrap());
        // Free only because the sheet says so.
        assert!(sheet.prices[1].output_per_million.is_zero());
    }

    fn rejects(text: &str, needle: &str) {
        let err = parse_price_sheet(text, today()).unwrap_err();
        assert!(err.contains(needle), "expected {needle:?} in {err:?}");
    }

    fn entry(extra: &str) -> String {
        format!(
            "[[price]]\nprovider = \"openai\"\nmodel = \"m\"\n\
             input_per_million = \"1\"\noutput_per_million = \"2\"\n{extra}"
        )
    }

    #[test]
    fn invalid_sheets_are_rejected() {
        let ok_tail = "source = \"s\"\nas_of = \"2026-10-01\"\n";
        rejects(&entry("as_of = \"2026-10-01\"\n"), "source");
        rejects(
            &entry("source = \"  \"\nas_of = \"2026-10-01\"\n"),
            "source is required",
        );
        rejects(&entry("source = \"s\"\n"), "as_of");
        rejects(&entry("source = \"s\"\nas_of = \"2026-12-01\"\n"), "future");
        rejects(
            &entry("source = \"s\"\nas_of = \"01.10.2026\"\n"),
            "YYYY-MM-DD",
        );
        // A typo must not silently drop a rate.
        rejects(
            &entry(&format!("{ok_tail}cached_input_per_milion = \"1\"\n")),
            "unknown field",
        );
        // Money as floats would be rounded: strings only.
        rejects(
            "[[price]]\nprovider = \"openai\"\nmodel = \"m\"\ninput_per_million = 0.15\n\
             output_per_million = \"2\"\nsource = \"s\"\nas_of = \"2026-10-01\"\n",
            "invalid price sheet",
        );
        rejects(
            &entry(&format!("{ok_tail}reasoning_per_million = \"-1\"\n")),
            "negative",
        );
        rejects(
            &entry(&format!("{ok_tail}reasoning_per_million = \"abc\"\n")),
            "invalid decimal",
        );
        let one = entry(ok_tail);
        rejects(&format!("{one}\n{one}"), "duplicate");
        // Input and output rates are mandatory.
        rejects(
            "[[price]]\nprovider = \"openai\"\nmodel = \"m\"\noutput_per_million = \"2\"\n\
             source = \"s\"\nas_of = \"2026-10-01\"\n",
            "input_per_million",
        );
    }

    #[tokio::test]
    async fn missing_file_is_an_empty_sheet() {
        let dir = std::env::temp_dir().join(format!("routerai-pricing-{}", uuid::Uuid::new_v4()));
        let sheet = load_price_sheet(&dir.join("pricing.toml")).await.unwrap();
        assert!(sheet.prices.is_empty());
    }

    #[test]
    fn applied_rates_reach_the_registry() {
        let ai = AiClient::builder().allow_empty_providers().build().unwrap();
        let sheet = parse_price_sheet(VALID, today()).unwrap();
        apply_price_sheet(&ai, &sheet).unwrap();
        let p = ai
            .pricing()
            .get_price_sync(&ProviderId::openai(), &ModelId::new("gpt-4o-mini"))
            .unwrap();
        assert_eq!(p.output_per_million, Some("0.60".parse().unwrap()));
        assert!(ai
            .pricing()
            .get_price_sync(&ProviderId::deepseek(), &ModelId::new("deepseek-chat"))
            .is_none());
    }

    /// The bundled demo prices must never reach the server: no server source file
    /// may call the demo loaders.
    #[test]
    fn server_never_loads_demo_prices() {
        let forbidden = [
            concat!("with_example", "_prices"),
            concat!("load_example", "_prices"),
        ];
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut checked = 0;
        for entry in std::fs::read_dir(&src).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for needle in forbidden {
                    assert!(!text.contains(needle), "{} calls {needle}", path.display());
                }
                checked += 1;
            }
        }
        assert!(checked >= 4, "scanned the server sources");
    }
}
