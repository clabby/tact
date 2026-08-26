//! Non-secret deployment configuration loaded from Worker bindings.

use crate::store::ScanBudget;
use tact_memory::MemoryLimits;
use thiserror::Error;
use worker::Env;

const SCAN_RECORDS_VARIABLE: &str = "TACT_MEMORY_SCAN_MAX_RECORDS";
const SCAN_CONTENT_BYTES_VARIABLE: &str = "TACT_MEMORY_SCAN_MAX_CONTENT_BYTES";
const SCAN_RESULTS_VARIABLE: &str = "TACT_MEMORY_SCAN_MAX_RESULTS";

/// Failure to load a valid shared-scan resource budget.
#[derive(Debug, Error)]
pub(super) enum ConfigError {
    #[error("missing Worker variable {name}")]
    Missing { name: &'static str },
    #[error("Worker variable {name} must be a positive integer")]
    Invalid { name: &'static str },
    #[error("Worker variable {name} must not exceed {maximum}")]
    AboveMaximum { name: &'static str, maximum: usize },
}

/// Loads the deployment's Worker-side BM25 scan budget.
pub(super) fn scan_budget(environment: &Env) -> Result<ScanBudget, ConfigError> {
    Ok(ScanBudget {
        records: positive_usize(environment, SCAN_RECORDS_VARIABLE)?,
        content_bytes: positive_usize(environment, SCAN_CONTENT_BYTES_VARIABLE)?,
        results: at_most(
            positive_usize(environment, SCAN_RESULTS_VARIABLE)?,
            SCAN_RESULTS_VARIABLE,
            MemoryLimits::PRODUCTION.scan_results,
        )?,
    })
}

fn positive_usize(environment: &Env, name: &'static str) -> Result<usize, ConfigError> {
    let value = environment
        .var(name)
        .map_err(|_| ConfigError::Missing { name })?
        .to_string();
    parse_positive_usize(&value, name)
}

fn parse_positive_usize(value: &str, name: &'static str) -> Result<usize, ConfigError> {
    value
        .parse()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(ConfigError::Invalid { name })
}

fn at_most(value: usize, name: &'static str, maximum: usize) -> Result<usize, ConfigError> {
    if value > maximum {
        return Err(ConfigError::AboveMaximum { name, maximum });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{ConfigError, SCAN_RESULTS_VARIABLE, at_most, parse_positive_usize};
    use tact_memory::MemoryLimits;

    #[test]
    fn positive_integer_variables_reject_zero_and_malformed_values() {
        assert!(parse_positive_usize("1", "TEST").is_ok());
        for value in ["0", "-1", "invalid"] {
            assert!(matches!(
                parse_positive_usize(value, "TEST"),
                Err(ConfigError::Invalid { name: "TEST" })
            ));
        }
    }

    #[test]
    fn scan_result_limit_cannot_exceed_the_protocol_bound() {
        let maximum = MemoryLimits::PRODUCTION.scan_results;
        assert_eq!(at_most(1, SCAN_RESULTS_VARIABLE, maximum).unwrap(), 1);
        assert_eq!(
            at_most(maximum, SCAN_RESULTS_VARIABLE, maximum).unwrap(),
            maximum
        );
        assert!(matches!(
            at_most(maximum + 1, SCAN_RESULTS_VARIABLE, maximum),
            Err(ConfigError::AboveMaximum {
                name: SCAN_RESULTS_VARIABLE,
                maximum: rejected_maximum,
            }) if rejected_maximum == maximum
        ));
    }
}
