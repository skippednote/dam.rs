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
    ///
    /// The OpenAI half was missing until a real key was pointed at a real library: this comment said "and
    /// OpenAI's" while the table held six Claude models and nothing else, so every OpenAI call was charged the
    /// $10/$50 fallback. `gpt-4o-mini` at $0.15/$0.60 was therefore overstated about sixty-sevenfold, and a
    /// tenant with a hard cap would have hit it after seven photographs instead of four hundred. The fallback
    /// was doing exactly what it says — overstating, so the cap trips early rather than late — but a rate
    /// that wrong is not a safety margin, it is a broken feature.
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
            // OpenAI. Prefix-matched like the rest, so a dated build — `gpt-4o-mini-2024-07-18`, which is what
            // the API actually reports back — is priced by its family rather than falling through.
            ("gpt-4o-mini".to_owned(), Price::per_mtok_dollars(0.15, 0.6)),
            ("gpt-4o".to_owned(), Price::per_mtok_dollars(2.5, 10.0)),
            ("gpt-4.1-mini".to_owned(), Price::per_mtok_dollars(0.4, 1.6)),
            ("gpt-4.1".to_owned(), Price::per_mtok_dollars(2.0, 8.0)),
            ("o4-mini".to_owned(), Price::per_mtok_dollars(1.1, 4.4)),
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

    /// What a call cost when it went through the Batch API, in micro-cents.
    ///
    /// Half, which is the provider's published batch discount and the reason §8.3 sends all library backfill
    /// that way. Halved here rather than by the caller so a batched run's `est_cost_cents` is the number that
    /// will appear on the invoice — a backfill recorded at list price would make every spend cap and every
    /// forecast wrong by a factor of two, in the direction that stops work early.
    pub fn estimate_batched(&self, model: &str, usage: &Usage) -> i64 {
        // Integer halving, rounding up: a fraction of a micro-cent is not worth losing, and rounding down would
        // make a cheap call free.
        let full = self.estimate(model, usage);
        full / 2 + full % 2
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
mod openai_prices {
    use super::*;

    #[test]
    fn an_openai_model_is_priced_by_its_family_rather_than_the_fallback() {
        let prices = Prices::default();
        // The name the API reports back, which is dated. Prefix matching is what makes that harmless.
        let mini = prices.for_model("gpt-4o-mini-2024-07-18");
        assert_eq!(mini, Price::per_mtok_dollars(0.15, 0.6));
        // And not the fallback, which is what it was getting: sixty-seven times the real rate, enough to make
        // a hard cap trip after seven photographs.
        assert_ne!(mini, Prices::fallback());

        // `gpt-4o-mini` must win over `gpt-4o` — the longest prefix, or every mini call is priced as a 4o one.
        assert_eq!(
            prices.for_model("gpt-4o-mini"),
            Price::per_mtok_dollars(0.15, 0.6)
        );
        assert_eq!(
            prices.for_model("gpt-4o"),
            Price::per_mtok_dollars(2.5, 10.0)
        );
        assert_eq!(
            prices.for_model("gpt-4.1-mini-2026-01-01"),
            Price::per_mtok_dollars(0.4, 1.6)
        );
    }

    #[test]
    fn a_model_nobody_priced_still_costs_the_expensive_fallback() {
        // The behaviour that was right all along, kept: an unpriced model is a configuration gap, and
        // overstating stops work early where understating blows a cap silently.
        assert_eq!(
            Prices::default().for_model("some-new-vendor-model"),
            Prices::fallback()
        );
    }

    #[test]
    fn a_described_photograph_costs_about_half_a_penny() {
        // The numbers from the real run that found this: 37,160 input tokens and 76 output for one iPhone
        // photograph through `gpt-4o-mini`. Image inputs on mini are charged at roughly thirty-three times the
        // tokens of 4o at a twenty-fifth of the price, which is why the token count looks alarming and the
        // bill should not.
        let prices = Prices::default();
        let micro_cents = prices.estimate(
            "gpt-4o-mini-2024-07-18",
            &Usage {
                input_tokens: 37_160,
                output_tokens: 76,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
            },
        );
        let cents = micro_cents as f64 / 1_000_000.0;
        assert!(
            (0.4..0.7).contains(&cents),
            "a described photograph should cost about half a penny, not {cents:.2}"
        );
    }
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
    fn a_batched_call_is_half_price_and_never_free() {
        let prices = Prices::default();
        let asset = usage(3_000, 300, 0, 0);
        let full = prices.estimate("claude-opus-5", &asset);
        assert_eq!(prices.estimate_batched("claude-opus-5", &asset), full / 2);
        // §8.3's table, batched: the naive $23k row becomes the $6–8k one once caching is in play too.
        assert_eq!(prices.estimate_batched("claude-opus-5", &asset), 1_125_000);
        // And an odd number rounds up rather than losing the last micro-cent.
        let tiny = usage(1, 0, 0, 0);
        let full = prices.estimate("claude-opus-5", &tiny);
        assert_eq!(full, 500);
        assert_eq!(prices.estimate_batched("claude-opus-5", &tiny), 250);
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
