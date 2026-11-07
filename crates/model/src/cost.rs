//! Run cost (design §4.6): the cost a run reported, else its tokens × a local price list, else "no price".
//! Integer picodollars throughout; rounded to cents only for display.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::run::{JsonMap, Run, RunType};

pub const PICO_PER_USD: i64 = 1_000_000_000_000;
const PICO_PER_CENT: i64 = PICO_PER_USD / 100;
const EMBEDDED: &str = include_str!("prices.toml");

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cost {
    Reported(i64),
    Computed(i64),
    /// A model call whose cost is unknown: counted, never taken as 0.
    NoPrice,
}

/// Picodollars per token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Price {
    pub input_pico: i64,
    pub output_pico: i64,
}

#[derive(Clone, Debug)]
struct Entry {
    price: Price,
    aliases: Vec<String>,
    user: bool,
}

#[derive(Clone, Debug)]
pub struct Prices {
    models: BTreeMap<String, Entry>,
    /// Exact names and aliases → model name; exact names win, then the user's aliases.
    index: HashMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PriceError {
    #[error("{file}: {source}")]
    Read { file: String, source: std::io::Error },
    #[error("{file} line {line}: {msg}")]
    Parse { file: String, line: usize, msg: String },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    version: toml::Spanned<i64>,
    #[serde(default)]
    models: BTreeMap<String, ModelFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelFile {
    input: toml::Spanned<f64>,
    output: toml::Spanned<f64>,
    #[serde(default)]
    aliases: Vec<String>,
}

impl Prices {
    pub fn embedded() -> Prices {
        let mut p = Prices {
            models: BTreeMap::new(),
            index: HashMap::new(),
        };
        p.merge_text("prices.toml (embedded)", EMBEDDED, false)
            .expect("the embedded price list parses");
        p
    }

    /// Replaces entries by model name with those in `path` (same shape as the embedded list).
    pub fn merge_user(&mut self, path: &Path) -> Result<(), PriceError> {
        let file = path.display().to_string();
        let text = std::fs::read_to_string(path).map_err(|source| PriceError::Read {
            file: file.clone(),
            source,
        })?;
        self.merge_text(&file, &text, true)
    }

    fn merge_text(&mut self, file: &str, text: &str, user: bool) -> Result<(), PriceError> {
        let at = |span: Option<std::ops::Range<usize>>, msg: String| PriceError::Parse {
            file: file.to_string(),
            line: span.map_or(1, |s| text[..s.start.min(text.len())].matches('\n').count() + 1),
            msg,
        };
        let parsed: File = toml::from_str(text).map_err(|e| at(e.span(), e.message().to_string()))?;
        if *parsed.version.get_ref() != 1 {
            return Err(at(Some(parsed.version.span()), "version must be 1".into()));
        }
        let mut entries = Vec::with_capacity(parsed.models.len());
        for (name, m) in parsed.models {
            let per_token = |v: &toml::Spanned<f64>, what: &str| {
                let usd = *v.get_ref();
                if usd.is_finite() && usd >= 0.0 {
                    Ok((usd * 1e6).round() as i64)
                } else {
                    Err(at(
                        Some(v.span()),
                        format!("{name}: {what} must be a price ≥ 0 in USD per 1M tokens"),
                    ))
                }
            };
            let price = Price {
                input_pico: per_token(&m.input, "input")?,
                output_pico: per_token(&m.output, "output")?,
            };
            entries.push((
                name,
                Entry {
                    price,
                    aliases: m.aliases,
                    user,
                },
            ));
        }
        self.models.extend(entries);
        self.reindex();
        Ok(())
    }

    fn reindex(&mut self) {
        self.index.clear();
        for user in [false, true] {
            for (name, e) in self.models.iter().filter(|(_, e)| e.user == user) {
                for a in &e.aliases {
                    self.index.insert(a.clone(), name.clone());
                }
            }
        }
        for name in self.models.keys() {
            self.index.insert(name.clone(), name.clone());
        }
    }

    /// Exact model name, then aliases; no prefix matching.
    pub fn get(&self, model: &str) -> Option<Price> {
        self.index.get(model).map(|name| self.models[name].price)
    }
}

/// The cost of a model call; `None` for every other run, so usage a chain repeats from its children (as the SDK
/// may send it) is never counted twice. Token counts too large to price are "no price".
pub fn of(run: &Run, prices: &Prices) -> Option<Cost> {
    if run.run_type != RunType::Llm {
        return None;
    }
    if let Some(c) = run.reported_cost_pico {
        return Some(Cost::Reported(c));
    }
    let price = run.model.as_deref().and_then(|m| prices.get(m));
    let computed = |p: Price, i: Option<u64>, o: Option<u64>| {
        let part = |n: Option<u64>, per: i64| i64::try_from(n.unwrap_or(0)).ok()?.checked_mul(per);
        part(i, p.input_pico)?.checked_add(part(o, p.output_pico)?)
    };
    Some(match (price, run.input_tokens, run.output_tokens) {
        (Some(p), i, o) if i.is_some() || o.is_some() => computed(p, i, o).map_or(Cost::NoPrice, Cost::Computed),
        _ => Cost::NoPrice,
    })
}

/// The cost a run reported, in picodollars: usage `total_cost`, or `input_cost + output_cost`, or the run's own
/// `total_cost` (a top-level LangSmith field, kept in `extra.other`).
pub fn reported_pico(usage: Option<&JsonMap>, extra: &JsonMap) -> Option<i64> {
    let usd = |v: Option<&Value>| {
        v.and_then(Value::as_f64)
            .filter(|x| x.is_finite() && *x >= 0.0)
            .map(|x| (x * PICO_PER_USD as f64).round() as i64)
    };
    let field = |k: &str| usd(usage.and_then(|u| u.get(k)));
    let parts = match (field("input_cost"), field("output_cost")) {
        (None, None) => None,
        (i, o) => Some(i.unwrap_or(0) + o.unwrap_or(0)),
    };
    field("total_cost")
        .or(parts)
        .or_else(|| usd(extra.get("other").and_then(|o| o.get("total_cost"))))
}

/// Picodollars → whole cents, half-up.
pub fn cents_half_up(pico: i64) -> i64 {
    (pico + PICO_PER_CENT / 2).div_euclid(PICO_PER_CENT)
}

/// `$4.12`.
pub fn display(pico: i64) -> String {
    let c = cents_half_up(pico);
    format!("${}.{:02}", c / 100, c % 100)
}
