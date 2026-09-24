//! Budgets and usage accounting.
//!
//! Budgets are hard limits enforced by the orchestrator, not suggestions to the
//! model: when a run or attempt crosses one, the engine stops dispatching work
//! for it and records why.

use serde::{Deserialize, Serialize};

/// Token and cost usage reported by an agent (or summed over many).
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, ts_rs::TS)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Cost in US dollars, when the agent reports it.
    pub cost_usd: f64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

impl std::ops::Add for Usage {
    type Output = Usage;

    fn add(self, rhs: Usage) -> Usage {
        Usage {
            input_tokens: self.input_tokens + rhs.input_tokens,
            output_tokens: self.output_tokens + rhs.output_tokens,
            cache_read_tokens: self.cache_read_tokens + rhs.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens + rhs.cache_write_tokens,
            cost_usd: self.cost_usd + rhs.cost_usd,
        }
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, rhs: Usage) {
        *self = *self + rhs;
    }
}

impl std::iter::Sum for Usage {
    fn sum<I: Iterator<Item = Usage>>(iter: I) -> Usage {
        iter.fold(Usage::default(), |acc, u| acc + u)
    }
}

/// Limits for a run or a single attempt. `None` means unlimited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS)]
#[serde(default)]
pub struct Budget {
    pub max_cost_usd: Option<f64>,
    pub max_tokens: Option<u64>,
    pub max_wall_clock_secs: Option<u64>,
    /// How many attempts a single task may use before it fails for good.
    pub max_attempts_per_task: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_cost_usd: None,
            max_tokens: None,
            max_wall_clock_secs: None,
            max_attempts_per_task: 3,
        }
    }
}

/// Which limit was crossed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ts_rs::TS, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BudgetExceeded {
    #[error("cost ${spent:.2} exceeded the ${limit:.2} budget")]
    Cost { limit: f64, spent: f64 },
    #[error("{spent} tokens exceeded the {limit}-token budget")]
    Tokens { limit: u64, spent: u64 },
    #[error("{elapsed_secs}s exceeded the {limit_secs}s time budget")]
    WallClock { limit_secs: u64, elapsed_secs: u64 },
}

impl Budget {
    /// Returns the first limit that `usage` (after `elapsed_secs`) has crossed.
    pub fn check(&self, usage: &Usage, elapsed_secs: u64) -> Option<BudgetExceeded> {
        if let Some(limit) = self.max_cost_usd
            && usage.cost_usd > limit
        {
            return Some(BudgetExceeded::Cost {
                limit,
                spent: usage.cost_usd,
            });
        }
        if let Some(limit) = self.max_tokens
            && usage.total_tokens() > limit
        {
            return Some(BudgetExceeded::Tokens {
                limit,
                spent: usage.total_tokens(),
            });
        }
        if let Some(limit_secs) = self.max_wall_clock_secs
            && elapsed_secs > limit_secs
        {
            return Some(BudgetExceeded::WallClock {
                limit_secs,
                elapsed_secs,
            });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_sums() {
        let a = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: 0.25,
            ..Usage::default()
        };
        let total: Usage = [a, a].into_iter().sum();
        assert_eq!(total.total_tokens(), 30);
        assert!((total.cost_usd - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn unlimited_budget_never_trips() {
        let usage = Usage {
            input_tokens: u64::MAX / 4,
            cost_usd: 1e9,
            ..Usage::default()
        };
        assert_eq!(Budget::default().check(&usage, u64::MAX), None);
    }

    #[test]
    fn reports_the_first_crossed_limit() {
        let budget = Budget {
            max_cost_usd: Some(1.0),
            max_tokens: Some(100),
            max_wall_clock_secs: Some(60),
            ..Budget::default()
        };
        let usage = Usage {
            input_tokens: 150,
            cost_usd: 0.5,
            ..Usage::default()
        };
        assert_eq!(
            budget.check(&usage, 10),
            Some(BudgetExceeded::Tokens {
                limit: 100,
                spent: 150
            })
        );
        assert_eq!(
            budget.check(&Usage::default(), 61),
            Some(BudgetExceeded::WallClock {
                limit_secs: 60,
                elapsed_secs: 61
            })
        );
    }
}
