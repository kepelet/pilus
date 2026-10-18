//! Estimating what a model's tokens would cost at list price.
//!
//! Pi records its own cost on every turn, which is better than any table, so
//! this is only for the turns that have none (Claude's). It is a table of
//! per-million-token list prices, taken from <https://pi.dev/models> (Pi's own
//! catalogue), so it goes stale: a model
//! not listed is *unpriced*, never guessed at, and the page says how many of its
//! tokens went uncounted. Cache writes are priced at the 5-minute rate (1.25×
//! input) because a transcript does not say which lifetime was used; a session
//! that wrote 1-hour entries (2×) costs a little more than shown.

use crate::stats::Tokens;

/// A model's list price, in dollars per million tokens.
///
/// A million tokens at a dollar each is one micro-dollar per token, so the
/// price is also the cost in micro-dollars of a single token.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

const fn price(input: f64, output: f64, cache_read: f64) -> Price {
    Price {
        input,
        output,
        cache_read,
        cache_write: input * 1.25,
    }
}

/// The price of `model`, or `None` for one this table does not know.
#[must_use]
pub fn lookup(model: &str) -> Option<Price> {
    Some(match canonical(model) {
        // Fable and Mythos 5.1 read the cache at a fortieth of input; the
        // earlier 5 reads at a tenth.
        "claude-fable-5-1" | "claude-mythos-5-1" => price(10.0, 50.0, 0.25),
        "claude-fable-5" | "claude-mythos-5" => price(10.0, 50.0, 1.0),
        "claude-opus-5-5" => price(4.0, 20.0, 0.20),
        "claude-opus-5" | "claude-opus-4-8" | "claude-opus-4-7" | "claude-opus-4-6"
        | "claude-opus-4-5" => price(5.0, 25.0, 0.50),
        "claude-sonnet-5" => price(2.0, 10.0, 0.20),
        "claude-sonnet-5-5" => price(2.0, 10.0, 0.10),
        "claude-sonnet-4-6" | "claude-sonnet-4-5" => price(3.0, 15.0, 0.30),
        "claude-haiku-5-5" => price(0.10, 0.50, 0.01),
        "claude-haiku-4-5" => price(1.0, 5.0, 0.10),
        _ => return None,
    })
}

/// `model` without what a transcript appends to the id: a snapshot date
/// (`-20251001`) or a context-window tag (`[1m]`).
fn canonical(model: &str) -> &str {
    let model = model.split('[').next().unwrap_or(model);
    match model.rsplit_once('-') {
        Some((base, tail)) if tail.len() == 8 && tail.bytes().all(|byte| byte.is_ascii_digit()) => {
            base
        }
        _ => model,
    }
}

/// What `tokens` of `model` cost, in millionths of a dollar, or `None` when the
/// model has no known price.
#[must_use]
pub fn estimate(model: &str, tokens: &Tokens) -> Option<u64> {
    let price = lookup(model)?;
    // Counts are far below f64's exact range and the answer is an estimate.
    #[allow(clippy::cast_precision_loss)]
    let micros = tokens.input as f64 * price.input
        + tokens.output as f64 * price.output
        + tokens.cache_read as f64 * price.cache_read
        + tokens.cache_write as f64 * price.cache_write;
    Some((micros + 0.5).floor() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_models_have_their_list_prices() {
        let sonnet = lookup("claude-sonnet-5-5").expect("sonnet");
        assert_eq!((sonnet.input, sonnet.output), (2.0, 10.0));
        assert_eq!(sonnet.cache_write, 2.5);
        assert_eq!(lookup("claude-opus-5-5").expect("opus").cache_read, 0.20);
        assert_eq!(lookup("claude-fable-5-1").expect("fable").cache_read, 0.25);
        assert_eq!(lookup("claude-fable-5").expect("fable 5").cache_read, 1.0);
        assert_eq!(lookup("claude-haiku-5-5").expect("haiku").output, 0.5);
        assert_eq!(
            lookup("claude-opus-4-5").expect("opus 4.5").cache_write,
            6.25
        );
        assert_eq!(
            lookup("claude-sonnet-4-5").expect("sonnet 4.5").cache_write,
            3.75
        );
    }

    #[test]
    fn dates_and_context_tags_do_not_hide_a_model() {
        assert_eq!(
            lookup("claude-haiku-4-5-20251001"),
            lookup("claude-haiku-4-5")
        );
        assert_eq!(lookup("claude-opus-4-6[1m]"), lookup("claude-opus-4-6"));
        assert_eq!(
            lookup("claude-opus-5-5-20260401"),
            lookup("claude-opus-5-5")
        );
    }

    #[test]
    fn an_unknown_model_is_unpriced_not_guessed() {
        assert_eq!(lookup("deepseek-v4.1-flash"), None);
        assert_eq!(lookup("claude-opus-5-9"), None);
        assert_eq!(estimate("nope", &Tokens::default()), None);
    }

    #[test]
    fn a_million_tokens_of_each_kind_costs_the_list_price() {
        let tokens = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
            turns: 0,
        };
        // 2 + 10 + 0.10 + 2.50 dollars.
        assert_eq!(estimate("claude-sonnet-5-5", &tokens), Some(14_600_000));
        assert_eq!(estimate("claude-sonnet-5-5", &Tokens::default()), Some(0));
    }
}
