//! What a call cost, in integers (M5a·4).
//!
//! [`crate::model::Usage`] carries the token counts a provider reported, deliberately without a price —
//! prices change, differ per model and per vendor, and a number computed inside a client would be a guess
//! baked into a database. This is where the guess is made instead, once, from a table that is configuration.
//!
//! ## Micro-cents, not decimals
//!
//! One enrichment call costs a fraction of a cent: ~2.25¢ on Opus 5 at §8.3's 3k-in/300-out shape, ~0.45¢ on
//! Haiku. Cents as integers would round most of a library's cost to nothing, and a decimal type would drag a
//! numeric dependency into the arithmetic on every job. So everything here is **micro-cents** — millionths of a
//! cent — which is the same unit `dam_db::quotas::charge` accumulates in, so a spend cap and a cost estimate
//! cannot disagree about scale.
//!
//! ## Four rates, because caching is the whole cost model
//!
//! §8.3 rests on a cached prefix costing about a tenth of a fresh one. A price list with a single input rate
//! could not express that, and the estimate for a caching workload would be off by most of the prompt. So a
//! [`Price`] carries all four rates the providers actually bill: fresh input, output, cache read, cache write.
//!
//! ## Matching a model name
//!
//! Exact first, then the longest configured prefix. A completion reports the model that answered, which may be
//! a dated build (`claude-opus-5-20260601`) of a name somebody priced (`claude-opus-5`); charging the fallback
//! rate for it would quietly misprice every call after a provider-side rollout.

use crate::model::Usage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Millionths of a cent. The unit everything in this module is denominated in.
pub const MICRO_CENTS_PER_CENT: i64 = 1_000_000;

/// Tokens per unit of the published rates. Vendors quote per million.
const TOKENS_PER_RATE_UNIT: i64 = 1_000_000;

/// The four rates a call is billed at, in micro-cents per million tokens.
///
/// So a dollar per million tokens is `100 * MICRO_CENTS_PER_CENT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    /// Fresh input tokens.
    pub input: i64,
    /// Output tokens, including whatever reasoning the provider billed as output.
    pub output: i64,
    /// Tokens served from a cached prefix. About a tenth of `input` for both families.
    pub cached_input: i64,
    /// Tokens written into a cache. Anthropic charges a premium on the write and nothing to keep it; the
    /// OpenAI-compatible family charges nothing at all, which is why this can be zero.
    pub cache_write: i64,
}

impl Price {
    /// A price from dollars per million tokens, which is how every vendor publishes them.
    ///
    /// Cache rates default to Anthropic's published multipliers — a tenth on a read, a quarter above input on a
    /// five-minute write — because a table without them would silently price a caching workload as if caching
    /// were free, which is the wrong direction: it would make the cheap path look cheaper than it is.
    pub fn per_mtok_dollars(input: f64, output: f64) -> Self {
        let micro = |dollars: f64| (dollars * 100.0 * MICRO_CENTS_PER_CENT as f64).round() as i64;
        Self {
            input: micro(input),
            output: micro(output),
            cached_input: micro(input * 0.1),
            cache_write: micro(input * 1.25),
        }
    }
}

/// A price list.
///
/// Configuration, not code: a vendor's announcement should not need a release. The defaults exist so a
/// deployment that has configured nothing still records something better than zero — a zero cost estimate makes
/// a spend cap decorative, which is exactly the failure G20 was written about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Prices {
    /// Keyed by model name or by a prefix of one.
    pub by_model: BTreeMap<String, Price>,
}

impl Default for Prices {
    /// Anthropic's and OpenAI's published rates as of 2026-06-24, per §8.3's table and the model list.
    ///
    /// Deliberately sparse. A name that is not here falls back on [`Prices::fallback`], and the fallback is
    /// expensive on purpose.
    fn default() -> Self {
        let by_model = BTreeMap::from([
            (
                "claude-opus-5".to_owned(),
                Price::per_mtok_dollars(5.0, 25.0),
            ),
            (
                "claude-opus-4".to_owned(),
                Price::per_mtok_dollars(5.0, 25.0),
            ),
            (
                "claude-sonnet-5".to_owned(),
                Price::per_mtok_dollars(3.0, 15.0),
            ),
            (
                "claude-sonnet-4".to_owned(),
                Price::per_mtok_dollars(3.0, 15.0),
            ),
            (
                "claude-haiku-4-5".to_owned(),
                Price::per_mtok_dollars(1.0, 5.0),
            ),
            (
                "claude-fable-5".to_owned(),
                Price::per_mtok_dollars(10.0, 50.0),
            ),
        ]);
        Self { by_model }
    }
}

impl Prices {
    /// What to charge for a model nobody priced.
    ///
    /// The most expensive rate in the default table, not the cheapest and not zero. An unpriced model is a
    /// configuration gap, and the safe direction for a spend cap is to overstate: an overstated estimate stops
    /// work early and somebody notices, where an understated one lets a cap be blown through silently. Recorded
    /// on the run either way, so the correction is arithmetic rather than archaeology.
    pub fn fallback() -> Price {
        Price::per_mtok_dollars(10.0, 50.0)
    }

    /// The price for a model name: exact match, then the longest configured prefix, then [`Self::fallback`].
    pub fn for_model(&self, model: &str) -> Price {
        if let Some(price) = self.by_model.get(model) {
            return *price;
        }
        self.by_model
            .iter()
            .filter(|(name, _)| model.starts_with(name.as_str()))
            .max_by_key(|(name, _)| name.len())
            .map(|(_, price)| *price)
            .unwrap_or_else(Self::fallback)
    }

    /// What a call cost, in micro-cents.
    pub fn estimate(&self, model: &str, usage: &Usage) -> i64 {
        cost(&self.for_model(model), usage)
    }

    /// The built-in table with a deployment's overrides applied on top.
    ///
    /// Merged rather than replaced: a deployment correcting one model's price after a vendor announcement
    /// should not silently unprice every other model into [`Prices::fallback`].
    pub fn with_overrides(
        overrides: &std::collections::BTreeMap<String, dam_core::config::ModelPrice>,
    ) -> Self {
        let mut prices = Self::default();
        for (model, rates) in overrides {
            prices.by_model.insert(
                model.clone(),
                Price::per_mtok_dollars(
                    rates.input_dollars_per_mtok,
                    rates.output_dollars_per_mtok,
                ),
            );
        }
        prices
    }
}

/// The arithmetic, separated so it is testable without a price list.
///
/// `i128` in the middle because a batch backfill's counts multiplied by a rate overflow `i64` long before the
/// result does, and a silently wrapped cost estimate is worse than no estimate.
pub fn cost(price: &Price, usage: &Usage) -> i64 {
    let rate = |tokens: u64, per_mtok: i64| -> i128 {
        i128::from(tokens) * i128::from(per_mtok) / i128::from(TOKENS_PER_RATE_UNIT)
    };
    let total = rate(usage.input_tokens, price.input)
        + rate(usage.output_tokens, price.output)
        + rate(usage.cached_input_tokens, price.cached_input)
        + rate(usage.cache_write_tokens, price.cache_write);
    i64::try_from(total).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn usage(input: u64, output: u64, cached: u64, written: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cached_input_tokens: cached,
            cache_write_tokens: written,
        }
    }

    #[test]
    fn the_shape_in_the_architecture_costs_what_the_architecture_says() {
        // §8.3's per-asset shape: ~3k in, ~300 out on Opus 5 at $5/$25 per Mtok. 1.5¢ + 0.75¢ = 2.25¢.
        let prices = Prices::default();
        let micro = prices.estimate("claude-opus-5", &usage(3_000, 300, 0, 0));
        assert_eq!(micro, 2_250_000, "2.25 cents in micro-cents");

        // And the whole library, which is the number §8.3's table is about: ~$23k naive.
        let library = i128::from(micro) * 1_000_000 / i128::from(MICRO_CENTS_PER_CENT) / 100;
        assert_eq!(library, 22_500, "dollars, within the table's rounding");
    }

    #[test]
    fn a_cached_prefix_is_the_discount_the_cost_model_rests_on() {
        let prices = Prices::default();
        let fresh = prices.estimate("claude-opus-5", &usage(3_000, 300, 0, 0));
        let cached = prices.estimate("claude-opus-5", &usage(300, 300, 2_700, 0));
        assert!(cached < fresh, "{cached} should be under {fresh}");
        // A tenth off the prefix, and the three parts spelled out so a wrong rate cannot hide in one total:
        // 300 fresh input at $5/Mtok is 0.15¢, 300 output at $25 is 0.75¢, and 2700 cached at $0.50 is 0.135¢.
        assert_eq!(cached, 150_000 + 750_000 + 135_000);
        // Under half the fresh price for the same work, which is the discount §8.3's costing rests on: most of
        // the prompt is the tenant's shared prefix, and it is the input side that collapses.
        assert!(cached * 2 < fresh, "{cached} is not under half of {fresh}");
    }

    #[test]
    fn a_dated_build_is_priced_as_the_model_it_is() {
        // A provider-side rollout returns a dated name. Falling back for it would misprice every call after.
        let prices = Prices::default();
        assert_eq!(
            prices.for_model("claude-opus-5-20260601"),
            prices.for_model("claude-opus-5")
        );
        // And the longest prefix wins, so haiku is not priced as opus by accident of ordering.
        assert_eq!(
            prices.for_model("claude-haiku-4-5-20251001"),
            Price::per_mtok_dollars(1.0, 5.0)
        );
    }

    #[test]
    fn an_unpriced_model_is_expensive_rather_than_free() {
        let prices = Prices::default();
        let unknown = prices.estimate("some-new-thing", &usage(3_000, 300, 0, 0));
        let opus = prices.estimate("claude-opus-5", &usage(3_000, 300, 0, 0));
        assert!(unknown > opus, "an unpriced model must not look cheap");
        assert!(
            unknown > 0,
            "and must never be free — a free call blows a cap"
        );
    }

    #[test]
    fn a_configured_price_beats_the_default() {
        let prices = Prices {
            by_model: BTreeMap::from([(
                "claude-opus-5".to_owned(),
                Price::per_mtok_dollars(1.0, 1.0),
            )]),
        };
        assert_eq!(
            prices.estimate("claude-opus-5", &usage(1_000_000, 0, 0, 0)),
            100_000_000
        );
    }

    #[test]
    fn an_override_replaces_one_price_and_keeps_the_rest() {
        let overrides = BTreeMap::from([(
            "claude-opus-5".to_owned(),
            dam_core::config::ModelPrice {
                input_dollars_per_mtok: 2.0,
                output_dollars_per_mtok: 8.0,
            },
        )]);
        let prices = Prices::with_overrides(&overrides);
        assert_eq!(
            prices.for_model("claude-opus-5"),
            Price::per_mtok_dollars(2.0, 8.0)
        );
        // The untouched entries survive. A replace-not-merge would drop haiku to the fallback rate and make
        // bulk classification look ten times dearer than it is.
        assert_eq!(
            prices.for_model("claude-haiku-4-5"),
            Price::per_mtok_dollars(1.0, 5.0)
        );
    }

    #[test]
    fn a_backfill_sized_count_does_not_wrap() {
        // Twenty billion input tokens — a few million assets at §8.3's per-asset shape — at the dearest rate on
        // the list. `tokens * rate` is 2 × 10^19, which does not fit in an i64 at all, so the multiplication has
        // to happen wider than the result. A wrapped estimate is worse than none: it is a small number that
        // looks like an answer.
        let huge = cost(
            &Price::per_mtok_dollars(10.0, 50.0),
            &usage(20_000_000_000, 0, 0, 0),
        );
        assert_eq!(huge, 20_000_000_000_000, "$200,000, in micro-cents");
    }

    #[test]
    fn the_longest_matching_prefix_wins() {
        // A deployment may price a whole family and then one model within it. The specific entry has to win, or
        // configuring a cheaper bulk model has no effect — and which one wins would otherwise depend on map
        // ordering, which is the kind of bug a passing suite keeps.
        let prices = Prices {
            by_model: BTreeMap::from([
                ("claude".to_owned(), Price::per_mtok_dollars(9.0, 9.0)),
                ("claude-haiku".to_owned(), Price::per_mtok_dollars(1.0, 5.0)),
            ]),
        };
        assert_eq!(
            prices.for_model("claude-haiku-4-5-20251001"),
            Price::per_mtok_dollars(1.0, 5.0)
        );
        assert_eq!(
            prices.for_model("claude-opus-5"),
            Price::per_mtok_dollars(9.0, 9.0),
            "and the family price still covers everything else in it"
        );
    }
}
