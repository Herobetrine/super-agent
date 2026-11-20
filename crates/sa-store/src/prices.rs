//! Versioned API list-price table and per-attempt monetary equivalent
//! (architecture §7.3, P0A).
//!
//! The table is a data file checked into the repository and embedded at build
//! time. Every entry names its vendor, exact model id (plus aliases the vendor
//! or the harness documents), currency, the date from which the table vouches
//! for the price, the official pricing page and the date it was read. Amounts
//! are an equivalent at the vendor's public API list price, never the actual
//! bill: subscription quota, negotiated discounts, regional uplifts, batch,
//! flex and fast tiers are outside it. No price is guessed: an attempt whose
//! model, tokens, tier or time cannot be matched exactly stays unpriced with a
//! reason code.
//!
//! Arithmetic is exact integer math. Rates are parsed into millionths of a
//! currency unit per million tokens, so `tokens × rate` is in 10⁻¹² units.

use crate::usage::TokenCounts;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Current table, see `docs/dev/usage-ledger.md`.
pub const PRICE_TABLE_JSON: &str = include_str!("../prices/2026-11-20.json");

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub schema_version: u32,
    pub table_version: String,
    pub basis: String,
    pub note: String,
    pub entries: Vec<PriceEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PriceEntry {
    /// Vendor whose public list price this is (not the route's gateway).
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    pub currency: String,
    /// `YYYY-MM-DD` (UTC). Attempts that started earlier are not priced.
    pub effective_from: String,
    pub source: String,
    pub retrieved: String,
    pub basis: String,
    pub rates: Rates,
    /// Request-level tier: a request whose prompt (all input, including cache
    /// reads and writes) exceeds the threshold pays `rates` of this tier for
    /// the whole request.
    #[serde(default)]
    pub long_context: Option<LongContext>,
    /// Time-of-day schedule; base `rates` are the peak rates.
    #[serde(default)]
    pub off_peak: Option<OffPeak>,
    #[serde(default)]
    pub notes: String,
}

/// Decimal strings per million tokens.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rates {
    /// Uncached input.
    pub input: String,
    pub cache_read: String,
    /// Keyed by cache lifetime (`5m`, `1h`, `30m`, `12h`) or `any` when the
    /// vendor charges every cache write the same. `null`: the vendor has no
    /// cache-write charge, so only attempts that wrote nothing are priced.
    pub cache_write: Option<BTreeMap<String, String>>,
    /// Includes reasoning/thinking output.
    pub output: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LongContext {
    pub above_input_tokens: u64,
    pub rates: Rates,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct OffPeak {
    /// Only `deepseek_utc_weekday_peak`: peak is 01:00–04:00 and
    /// 06:00–10:00 UTC Monday to Friday; Chinese public holidays are also
    /// off-peak, but the table has no holiday calendar, so a weekday peak
    /// window is never assumed to be a holiday.
    pub schedule: String,
    pub rates: Rates,
}

/// Evidence that decides tiers and cache-write variants, collected by the
/// usage projection next to the attempt's token counts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PriceEvidence {
    /// Anthropic cache writes split by lifetime, when every counted frame
    /// reported the split.
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    /// Upper bound of any single request's prompt, when tighter than the
    /// attempt's total input.
    pub request_input_bound: Option<u64>,
    /// The native frames reported a non-standard speed or a regional
    /// inference surcharge that the table does not model.
    pub nonstandard_mode: bool,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
}

/// Result of pricing one attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Priced {
    Amount {
        currency: String,
        /// 10⁻¹² currency units.
        units: u128,
        /// Matched table model id.
        model: String,
        provider: String,
        /// `standard`, `long_context` or `off_peak`.
        tier: &'static str,
    },
    Unpriced {
        reason: &'static str,
        detail: String,
    },
}

/// The embedded table, or `None` if it does not parse (a test guards this).
pub fn table() -> Option<&'static PriceTable> {
    static TABLE: OnceLock<Option<PriceTable>> = OnceLock::new();
    TABLE
        .get_or_init(|| parse_table(PRICE_TABLE_JSON).ok())
        .as_ref()
}

/// Parse and validate a table: rates are decimals, dates are dates, ids are
/// unique across models and aliases.
pub fn parse_table(text: &str) -> anyhow::Result<PriceTable> {
    let table: PriceTable = serde_json::from_str(text)?;
    anyhow::ensure!(table.schema_version == 1, "价格表格式版本不支持");
    let mut ids = std::collections::HashSet::new();
    for entry in &table.entries {
        anyhow::ensure!(day(&entry.effective_from).is_some(), "价格生效日期无效：{}", entry.model);
        anyhow::ensure!(day(&entry.retrieved).is_some(), "价格读取日期无效：{}", entry.model);
        anyhow::ensure!(entry.source.starts_with("https://"), "价格来源无效：{}", entry.model);
        anyhow::ensure!(!entry.currency.is_empty(), "价格币种缺失：{}", entry.model);
        for id in std::iter::once(&entry.model).chain(&entry.aliases) {
            anyhow::ensure!(ids.insert(id.clone()), "价格表型号重复：{id}");
        }
        let mut all = vec![&entry.rates];
        all.extend(entry.long_context.as_ref().map(|t| &t.rates));
        all.extend(entry.off_peak.as_ref().map(|t| &t.rates));
        for rates in all {
            parsed(rates).ok_or_else(|| anyhow::anyhow!("价格无效：{}", entry.model))?;
        }
        if let Some(off) = &entry.off_peak {
            anyhow::ensure!(off.schedule == "deepseek_utc_weekday_peak", "未知时段规则：{}", off.schedule);
        }
    }
    Ok(table)
}

impl PriceTable {
    /// Exact model id or alias; never a prefix or family match.
    pub fn lookup(&self, model: &str) -> Option<&PriceEntry> {
        self.entries
            .iter()
            .find(|e| e.model == model || e.aliases.iter().any(|a| a == model))
    }

    /// Price one attempt's reported tokens. Every billable category must be
    /// known; input includes cache reads and writes, so uncached input is
    /// `input − cache_read − cache_write`. Reasoning is a subset of output.
    pub fn price(&self, model: Option<&str>, tokens: &TokenCounts, evidence: &PriceEvidence) -> Priced {
        let unpriced = |reason: &'static str, detail: String| Priced::Unpriced { reason, detail };
        let Some(model) = model.filter(|m| !m.is_empty()) else {
            return unpriced("model_unknown", "无法确定这次执行使用的模型".into());
        };
        let Some(entry) = self.lookup(model) else {
            return unpriced("price_unknown", format!("价格表 {} 没有型号 {model} 的官方牌价", self.table_version));
        };
        let Some(at) = evidence.started_ms.or(evidence.ended_ms) else {
            return unpriced("time_unknown", "执行时间未知，无法判断价格是否适用".into());
        };
        if day(&entry.effective_from).is_none_or(|from| (at / 86_400_000) < from as u64) {
            return unpriced(
                "price_not_effective",
                format!("执行早于价格表对 {} 的生效日期 {}", entry.model, entry.effective_from),
            );
        }
        if evidence.nonstandard_mode {
            return unpriced("nonstandard_mode", "原生用量标明了快速模式或区域推理加价，价格表不含".into());
        }
        let (Some(input), Some(read), Some(write), Some(output)) = (
            tokens.input_tokens,
            tokens.cache_read_input_tokens,
            tokens.cache_creation_input_tokens,
            tokens.output_tokens,
        ) else {
            return unpriced("tokens_incomplete", "输入、缓存读取、缓存写入或输出有未报告的项".into());
        };
        let Some(uncached) = input.checked_sub(read).and_then(|rest| rest.checked_sub(write)) else {
            return unpriced("tokens_incomplete", "缓存计数大于全部输入，口径不一致".into());
        };
        let (rates, tier) = match (&entry.long_context, &entry.off_peak) {
            (Some(long), _) => {
                // A request is short-context only if its own prompt is within
                // the threshold. Any bound on the largest request decides it.
                let bound = evidence.request_input_bound.unwrap_or(input).min(input);
                if bound <= long.above_input_tokens {
                    (&entry.rates, "standard")
                } else {
                    return unpriced(
                        "tier_ambiguous",
                        format!("无法证明每个请求都不超过 {} 输入 Token 的长上下文阈值", long.above_input_tokens),
                    );
                }
            }
            (None, Some(off)) => match evidence.started_ms.zip(evidence.ended_ms) {
                Some((start, end)) if end >= start && deepseek_off_peak(start, end) => (&off.rates, "off_peak"),
                _ => {
                    return unpriced("tier_ambiguous", "执行时段可能落在高峰价时段（或时间不完整）".into());
                }
            },
            (None, None) => (&entry.rates, "standard"),
        };
        let Some(parsed) = parsed(rates) else {
            return unpriced("price_unknown", "价格表条目无效".into());
        };
        let write_units = match (&parsed.cache_write, write) {
            (_, 0) => Some(0u128),
            (None, _) => {
                return unpriced("price_unknown", format!("{} 没有缓存写入计费项，但报告了缓存写入", entry.model));
            }
            (Some(variants), write) => {
                if let Some(rate) = variants.get("any") {
                    Some(u128::from(write) * rate)
                } else if let (Some(m5), Some(h1), Some(r5), Some(r1)) = (
                    evidence.cache_write_5m,
                    evidence.cache_write_1h,
                    variants.get("5m"),
                    variants.get("1h"),
                ) && m5.checked_add(h1) == Some(write)
                {
                    Some(u128::from(m5) * r5 + u128::from(h1) * r1)
                } else {
                    None
                }
            }
        };
        let Some(write_units) = write_units else {
            return unpriced("cache_ttl_unknown", "缓存写入没有按有效期分项，无法选择写入价".into());
        };
        let units = u128::from(uncached) * parsed.input
            + u128::from(read) * parsed.cache_read
            + write_units
            + u128::from(output) * parsed.output;
        Priced::Amount {
            currency: entry.currency.clone(),
            units,
            model: entry.model.clone(),
            provider: entry.provider.clone(),
            tier,
        }
    }
}

struct Parsed {
    input: u128,
    cache_read: u128,
    cache_write: Option<BTreeMap<String, u128>>,
    output: u128,
}

fn parsed(rates: &Rates) -> Option<Parsed> {
    Some(Parsed {
        input: micros(&rates.input)?,
        cache_read: micros(&rates.cache_read)?,
        cache_write: match &rates.cache_write {
            None => None,
            Some(map) => {
                let mut out = BTreeMap::new();
                for (key, value) in map {
                    if !matches!(key.as_str(), "5m" | "1h" | "30m" | "12h" | "any") {
                        return None;
                    }
                    out.insert(key.clone(), micros(value)?);
                }
                Some(out)
            }
        },
        output: micros(&rates.output)?,
    })
}

/// Decimal string to millionths, rejecting signs, exponents and more than
/// six fractional digits.
fn micros(text: &str) -> Option<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || fraction.len() > 6
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let fraction: u128 = format!("{fraction:0<6}").parse().ok()?;
    whole.checked_mul(1_000_000)?.checked_add(fraction)
}

/// 10⁻¹² units as an exact decimal string without trailing zeros.
pub fn format_units(units: u128) -> String {
    const SCALE: u128 = 1_000_000_000_000;
    let fraction = format!("{:012}", units % SCALE);
    let fraction = fraction.trim_end_matches('0');
    if fraction.is_empty() {
        format!("{}", units / SCALE)
    } else {
        format!("{}.{fraction}", units / SCALE)
    }
}

/// Inverse of [`format_units`]; `None` for anything else.
pub fn parse_units(text: &str) -> Option<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if whole.is_empty()
        || fraction.len() > 12
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let fraction: u128 = format!("{fraction:0<12}").parse().ok()?;
    whole.checked_mul(1_000_000_000_000)?.checked_add(fraction)
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` date.
fn day(text: &str) -> Option<i64> {
    let mut parts = text.split('-');
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Whether `[start, end]` (Unix ms) avoids every weekday peak window
/// (01:00–04:00 and 06:00–10:00 UTC, Monday–Friday). Intervals longer than
/// 40 days are not examined.
fn deepseek_off_peak(start: u64, end: u64) -> bool {
    const DAY: u64 = 86_400_000;
    const HOUR: u64 = 3_600_000;
    let (first, last) = (start / DAY, end / DAY);
    if last - first > 40 {
        return false;
    }
    (first..=last).all(|day| {
        // 1970-01-01 was a Thursday: (day + 4) % 7 gives 0 = Sunday.
        let weekday = (day + 4) % 7;
        if weekday == 0 || weekday == 6 {
            return true;
        }
        let base = day * DAY;
        [(1, 4), (6, 10)]
            .iter()
            .all(|(from, to)| end < base + from * HOUR || start >= base + to * HOUR)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_and_date_parsing_is_exact() {
        assert_eq!(micros("4"), Some(4_000_000));
        assert_eq!(micros("0.0625"), Some(62_500));
        assert_eq!(micros("0.003"), Some(3_000));
        for bad in ["", "-1", "1e3", "0.0000001", ".5", "1.2.3"] {
            assert_eq!(micros(bad), None, "{bad}");
        }
        assert_eq!(format_units(1_500_000_000_000), "1.5");
        assert_eq!(format_units(4_000_000_000_000), "4");
        assert_eq!(format_units(1), "0.000000000001");
        assert_eq!(parse_units("1.5"), Some(1_500_000_000_000));
        assert_eq!(parse_units(&format_units(123_456_789)), Some(123_456_789));
        assert_eq!(day("1970-01-01"), Some(0));
        assert_eq!(day("2026-11-20"), Some(20_777));
        assert_eq!(day("2026-13-01"), None);
    }

    fn tokens(input: u64, read: u64, write: u64, output: u64) -> TokenCounts {
        TokenCounts {
            total_tokens: Some(input + output),
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_input_tokens: Some(read),
            cache_creation_input_tokens: Some(write),
            reasoning_output_tokens: None,
        }
    }

    fn at_table_date() -> PriceEvidence {
        PriceEvidence { started_ms: Some(20_777 * 86_400_000), ..Default::default() }
    }

    #[test]
    fn embedded_table_is_valid_and_traceable() {
        let table = table().expect("embedded price table parses");
        assert_eq!(table.table_version, "2026-11-20");
        assert_eq!(table.basis, "api_list_price");
        for entry in &table.entries {
            assert_eq!((entry.retrieved.as_str(), entry.basis.as_str()), ("2026-11-20", "api_list_price"));
            assert!(entry.source.starts_with("https://"), "{}", entry.model);
        }
        for model in ["claude-opus-5-5", "claude-fable-5-2", "gpt-6.2-sol", "gpt-6.1-sol", "deepseek-flash", "gemini-4-hydrogen"] {
            assert!(table.lookup(model).is_some(), "{model}");
        }
        // Exact ids only: no family, prefix or effort-suffix guessing.
        for model in ["claude-opus", "claude-opus-5-5-20260922", "gpt-6.2", "deepseek-v4.1-flash", "gemini-4-hydrogen-high"] {
            assert!(table.lookup(model).is_none(), "{model}");
        }
        assert_eq!(table.lookup("claude-haiku-4-5-20251001").map(|e| e.model.as_str()), Some("claude-haiku-4-5"));
        let duplicate = PRICE_TABLE_JSON.replace("\"claude-opus-4-7\"", "\"claude-opus-4-6\"");
        assert!(parse_table(&duplicate).is_err());
    }

    #[test]
    fn uncached_input_excludes_both_cache_buckets() {
        let table = table().unwrap();
        let evidence = PriceEvidence { cache_write_5m: Some(30), cache_write_1h: Some(0), ..at_table_date() };
        let priced = table.price(Some("claude-sonnet-5-5"), &tokens(1_000_000, 600_000, 30, 0), &evidence);
        // (1,000,000 − 600,000 − 30) × $2 + 600,000 × $0.10 + 30 × $2.50
        // (5-minute write), per million tokens.
        let Priced::Amount { units, currency, .. } = priced else { panic!("{priced:?}") };
        assert_eq!((format_units(units).as_str(), currency.as_str()), ("0.860015", "USD"));
        let inconsistent = table.price(Some("claude-sonnet-5-5"), &tokens(10, 8, 5, 0), &evidence);
        assert!(matches!(inconsistent, Priced::Unpriced { reason: "tokens_incomplete", .. }));
        let mut missing = tokens(10, 0, 0, 1);
        missing.cache_read_input_tokens = None;
        assert!(matches!(table.price(Some("claude-sonnet-5-5"), &missing, &evidence), Priced::Unpriced { reason: "tokens_incomplete", .. }));
        assert!(matches!(table.price(None, &missing, &evidence), Priced::Unpriced { reason: "model_unknown", .. }));
        // A vendor without a cache-write charge cannot price reported writes.
        assert!(matches!(
            table.price(Some("gemini-4-hydrogen"), &tokens(10, 0, 5, 1), &at_table_date()),
            Priced::Unpriced { reason: "price_unknown", .. }
        ));
        // OpenAI writes without a lifetime split are not guessed.
        assert!(matches!(
            table.price(Some("gpt-6.2-sol"), &tokens(10, 0, 5, 1), &at_table_date()),
            Priced::Unpriced { reason: "cache_ttl_unknown", .. }
        ));
    }

    #[test]
    fn request_tiers_need_a_bound_on_every_request() {
        let table = table().unwrap();
        let small = table.price(Some("claude-haiku-5-5"), &tokens(100_000, 0, 0, 1_000), &at_table_date());
        assert!(matches!(small, Priced::Amount { tier: "standard", .. }), "{small:?}");
        let large = table.price(Some("claude-haiku-5-5"), &tokens(100_001, 0, 0, 0), &at_table_date());
        assert!(matches!(large, Priced::Unpriced { reason: "tier_ambiguous", .. }));
        let bounded = PriceEvidence { request_input_bound: Some(90_000), ..at_table_date() };
        assert!(matches!(
            table.price(Some("claude-haiku-5-5"), &tokens(180_000, 0, 0, 0), &bounded),
            Priced::Amount { .. }
        ));
    }

    #[test]
    fn currencies_come_from_the_entry_and_are_never_converted() {
        let mut table = table().unwrap().clone();
        let mut entry = table.lookup("deepseek-v4-pro").unwrap().clone();
        entry.model = "fixture-cny".into();
        entry.aliases.clear();
        entry.currency = "CNY".into();
        entry.off_peak = None;
        entry.rates = Rates {
            input: "2".into(),
            cache_read: "0.5".into(),
            cache_write: Some(BTreeMap::from([("any".to_string(), "2".to_string())])),
            output: "8".into(),
        };
        table.entries.push(entry);
        let priced = table.price(Some("fixture-cny"), &tokens(1_000_000, 0, 0, 1_000_000), &at_table_date());
        let Priced::Amount { currency, units, .. } = priced else { panic!("{priced:?}") };
        assert_eq!((currency.as_str(), format_units(units).as_str()), ("CNY", "10"));
    }

    #[test]
    fn off_peak_windows_follow_utc_weekdays() {
        const HOUR: u64 = 3_600_000;
        // 2026-11-20 is a Friday; 2026-11-21 a Saturday.
        let friday = 20_777 * 86_400_000;
        let saturday = friday + 24 * HOUR;
        assert!(deepseek_off_peak(friday + 4 * HOUR, friday + 6 * HOUR - 1));
        assert!(deepseek_off_peak(friday + 10 * HOUR, friday + 23 * HOUR));
        assert!(!deepseek_off_peak(friday + 3 * HOUR, friday + 5 * HOUR));
        assert!(!deepseek_off_peak(friday + 9 * HOUR, friday + 11 * HOUR));
        assert!(deepseek_off_peak(saturday + 2 * HOUR, saturday + 9 * HOUR));
    }
}
