//! Per-model prices, in US dollars per million tokens, for the running cost of a run. Update
//! the table when prices change: it is hardcoded and goes stale.

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
}
