//! Per-model prices, in US dollars per million tokens, for the running cost of a run and the
//! run dialog's estimate. Update the table when prices change: it is hardcoded and goes stale.

use serde::Serialize;

/// What one request used, as the Messages API reports it. `input` is the uncached remainder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: i64,
    pub cache_write: i64,
    pub cache_read: i64,
    pub output: i64,
}

impl TokenUsage {
    /// The whole prompt: uncached, written to the cache and read from it.
    pub fn prompt(&self) -> i64 {
        self.input + self.cache_write + self.cache_read
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Price {
    input: f64,
    output: f64,
    /// 5-minute cache writes, 1.25x input.
    cache_write: f64,
    cache_read: f64,
}

/// The shortest prompt prefix the current Claude models cache. A shorter one is billed in full
/// on every request.
const MIN_CACHED_PREFIX: i64 = 512;

/// Claude Haiku 5.5 bills prompts over this many tokens at its long-prompt rates.
const HAIKU_LONG_PROMPT: i64 = 100_000;

fn price(model: &str, prompt_tokens: i64) -> Option<Price> {
    let price = |input: f64, output: f64, cache_read: f64| Price {
        input,
        output,
        cache_write: input * 1.25,
        cache_read,
    };
    Some(match model {
        "claude-sonnet-5-5" | "claude-sonnet-5" => price(2.0, 10.0, 0.20),
        "claude-opus-5-5" => price(4.0, 20.0, 0.20),
        "claude-opus-5" | "claude-opus-4-8" => price(5.0, 25.0, 0.50),
        "claude-haiku-5-5" if prompt_tokens > HAIKU_LONG_PROMPT => price(0.50, 2.50, 0.05),
        "claude-haiku-5-5" => price(0.10, 0.50, 0.01),
        // Jev bills input tokens only, every version at the same rate
        m if m.starts_with("jev-") => Price {
            input: 0.042,
            output: 0.0,
            cache_write: 0.0,
            cache_read: 0.0,
        },
        _ => return None,
    })
}

/// The cost of `usage` on `model`, or None for a model the table doesn't list.
pub fn cost_usd(model: &str, usage: &TokenUsage) -> Option<f64> {
    let p = price(model, usage.prompt())?;
    let per_token = |count: i64, rate: f64| count as f64 * rate / 1_000_000.0;
    Some(
        per_token(usage.input, p.input)
            + per_token(usage.cache_write, p.cache_write)
            + per_token(usage.cache_read, p.cache_read)
            + per_token(usage.output, p.output),
    )
}

/// Tokens in `text`, overestimated: about three bytes per token, which holds for English and
/// for the three-byte UTF-8 characters that tokenize one per token.
pub fn tokens_by_length(text: &str) -> usize {
    text.len().div_ceil(3)
}

/// What a run would use, shown before it starts (D22).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Estimate {
    /// Billed per token: about `usd`, or None for a model the table doesn't list.
    #[serde(rename_all = "camelCase")]
    Cost {
        usd: Option<f64>,
        model: Option<String>,
    },
    /// On the Claude plan: one request per asset, counted against the plan's limits.
    Requests { count: usize },
}

/// One asset's request, by length: the prefix every request of the run shares (the cached
/// system block), the rest of the prompt, and the expected output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequestTokens {
    pub shared: i64,
    pub own: i64,
    pub output: i64,
}

/// The cost of `requests` requests like `tokens` on `model`. A shared prefix long enough to
/// cache is written once and read by every later request; a shorter one is billed in full each
/// time. None for a model the table doesn't list.
pub fn estimate_usd(model: &str, tokens: RequestTokens, requests: usize) -> Option<f64> {
    let uncached = TokenUsage {
        input: tokens.shared + tokens.own,
        output: tokens.output,
        ..Default::default()
    };
    let (first, rest) = if tokens.shared >= MIN_CACHED_PREFIX {
        let first = TokenUsage {
            input: tokens.own,
            cache_write: tokens.shared,
            cache_read: 0,
            output: tokens.output,
        };
        let rest = TokenUsage {
            cache_write: 0,
            cache_read: tokens.shared,
            ..first
        };
        (first, rest)
    } else {
        (uncached, uncached)
    };
    let first_cost = cost_usd(model, &first)?;
    let rest_cost = cost_usd(model, &rest)?;
    Some(match requests {
        0 => 0.0,
        n => first_cost + (n - 1) as f64 * rest_cost,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-12
    }

    #[test]
    fn each_token_kind_is_priced_at_its_own_rate() {
        let usage = TokenUsage {
            input: 1_000_000,
            cache_write: 1_000_000,
            cache_read: 1_000_000,
            output: 1_000_000,
        };

        // Sonnet 5.5: $2 in, $2.50 cache write, $0.20 cache read, $10 out
        assert!(close(cost_usd("claude-sonnet-5-5", &usage).unwrap(), 14.70));
        // Opus 5.5: $4, $5, $0.20, $20
        assert!(close(cost_usd("claude-opus-5-5", &usage).unwrap(), 29.20));
    }

    #[test]
    fn a_typical_tagging_request_costs_a_fraction_of_a_cent() {
        let usage = TokenUsage {
            input: 600,
            cache_write: 0,
            cache_read: 3_000,
            output: 80,
        };

        let cost = cost_usd("claude-sonnet-5-5", &usage).unwrap();
        assert!(close(cost, 0.0012 + 0.0006 + 0.0008), "{cost}");
    }

    #[test]
    fn haiku_switches_to_its_long_prompt_rates_past_100k_tokens() {
        let short = TokenUsage {
            input: 100_000,
            output: 1_000,
            ..Default::default()
        };
        let long = TokenUsage {
            input: 100_001,
            output: 1_000,
            ..Default::default()
        };

        assert!(close(
            cost_usd("claude-haiku-5-5", &short).unwrap(),
            0.01 + 0.0005
        ));
        assert!(close(
            cost_usd("claude-haiku-5-5", &long).unwrap(),
            100_001.0 * 0.5 / 1e6 + 0.0025
        ));
    }

    #[test]
    fn jev_bills_input_tokens_alone() {
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            ..Default::default()
        };

        assert!(close(cost_usd("jev-1.14.0", &usage).unwrap(), 0.042));
        assert!(close(cost_usd("jev-latest", &usage).unwrap(), 0.042));
    }

    #[test]
    fn a_model_missing_from_the_table_has_no_price() {
        assert_eq!(cost_usd("my-custom-model", &TokenUsage::default()), None);
    }

    #[test]
    fn a_long_shared_prefix_is_written_once_then_read_from_the_cache() {
        let tokens = RequestTokens {
            shared: 10_000,
            own: 400,
            output: 200,
        };

        // Sonnet 5.5: the first request writes 10k at $2.50/M, the other 9 read it at $0.20/M;
        // each pays 400 in at $2/M and 200 out at $10/M
        let per_request = 400.0 * 2.0 / 1e6 + 200.0 * 10.0 / 1e6;
        let expected = 10.0 * per_request + 10_000.0 * 2.5 / 1e6 + 9.0 * 10_000.0 * 0.2 / 1e6;
        let cost = estimate_usd("claude-sonnet-5-5", tokens, 10).unwrap();
        assert!(close(cost, expected), "{cost} != {expected}");
    }

    #[test]
    fn a_shared_prefix_too_short_to_cache_is_billed_in_full_every_time() {
        let tokens = RequestTokens {
            shared: 511,
            own: 489,
            output: 100,
        };

        // 1,000 in at $2/M and 100 out at $10/M, three times
        let cost = estimate_usd("claude-sonnet-5-5", tokens, 3).unwrap();
        assert!(close(cost, 3.0 * (0.002 + 0.001)), "{cost}");
    }

    #[test]
    fn no_requests_cost_nothing_and_an_unlisted_model_has_no_estimate() {
        let tokens = RequestTokens {
            shared: 1_000,
            own: 100,
            output: 100,
        };

        assert_eq!(estimate_usd("claude-haiku-5-5", tokens, 0), Some(0.0));
        assert_eq!(estimate_usd("my-custom-model", tokens, 10), None);
    }

    #[test]
    fn tokens_by_length_counts_three_bytes_per_token_rounded_up() {
        assert_eq!(tokens_by_length(""), 0);
        assert_eq!(tokens_by_length("rock"), 2);
        assert_eq!(tokens_by_length("Nature > Rock"), 5);
    }
}
