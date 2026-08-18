//! Non-secret deployment configuration loaded from Worker bindings.

use crate::store::ScanBudget;
use tact_memory::MemoryLimits;
use thiserror::Error;
use worker::Env;

const MAX_RECORDS_VARIABLE: &str = "TACT_MEMORY_MAX_RECORDS";
const SCAN_RECORDS_VARIABLE: &str = "TACT_MEMORY_SCAN_MAX_RECORDS";
const SCAN_CONTENT_BYTES_VARIABLE: &str = "TACT_MEMORY_SCAN_MAX_CONTENT_BYTES";

/// Failure to load a positive deployment limit.
#[derive(Debug, Error)]
pub(super) enum ConfigError {
    #[error("missing Worker variable {name}")]
    Missing { name: &'static str },
    #[error("Worker variable {name} must be a positive integer")]
    Invalid { name: &'static str },
}

/// Loads the per-namespace capacity enforced by the D1 store.
pub(super) fn memory_limits(environment: &Env) -> Result<MemoryLimits, ConfigError> {
    MemoryLimits::PRODUCTION
        .try_with_record_capacity(positive_usize(environment, MAX_RECORDS_VARIABLE)?)
        .ok_or(ConfigError::Invalid {
            name: MAX_RECORDS_VARIABLE,
        })
}

/// Loads the deployment's maximum Worker-side BM25 corpus.
pub(super) fn scan_budget(environment: &Env) -> Result<ScanBudget, ConfigError> {
    Ok(ScanBudget {
        records: positive_usize(environment, SCAN_RECORDS_VARIABLE)?,
        content_bytes: positive_usize(environment, SCAN_CONTENT_BYTES_VARIABLE)?,
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

#[cfg(test)]
mod tests {
    use super::{ConfigError, MAX_RECORDS_VARIABLE, parse_positive_usize};
    use tact_memory::MemoryLimits;

    #[test]
    fn deployment_limits_must_be_positive_integers() {
        assert_eq!(
            parse_positive_usize("1024", MAX_RECORDS_VARIABLE).unwrap(),
            1_024
        );
        for value in ["", "0", "-1", "many"] {
            assert!(matches!(
                parse_positive_usize(value, MAX_RECORDS_VARIABLE),
                Err(ConfigError::Invalid {
                    name: MAX_RECORDS_VARIABLE
                })
            ));
        }
        assert!(
            MemoryLimits::PRODUCTION
                .try_with_record_capacity(usize::MAX)
                .is_none()
        );
    }
}
