//! Non-secret deployment configuration loaded from Worker bindings.

use crate::store::ScanBudget;
use tact_memory::MemoryLimits;
use thiserror::Error;
use worker::Env;

const MAX_RECORDS_VARIABLE: &str = "TACT_MEMORY_MAX_RECORDS";
const MAX_RECORD_BYTES_VARIABLE: &str = "TACT_MEMORY_MAX_RECORD_BYTES";
const MAX_TOTAL_BYTES_VARIABLE: &str = "TACT_MEMORY_MAX_TOTAL_BYTES";
const MAX_REQUEST_BYTES_VARIABLE: &str = "TACT_MEMORY_MAX_REQUEST_BYTES";
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
    Ok(MemoryLimits {
        records: positive_usize(environment, MAX_RECORDS_VARIABLE)?,
        content_bytes: positive_usize(environment, MAX_RECORD_BYTES_VARIABLE)?,
        total_content_bytes: positive_usize(environment, MAX_TOTAL_BYTES_VARIABLE)?,
        ..MemoryLimits::PRODUCTION
    })
}

/// Loads the deployment's maximum encoded JSON request body.
pub(super) fn max_request_bytes(environment: &Env) -> Result<usize, ConfigError> {
    positive_usize(environment, MAX_REQUEST_BYTES_VARIABLE)
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
    use super::{
        ConfigError, MAX_RECORD_BYTES_VARIABLE, MAX_RECORDS_VARIABLE, MAX_REQUEST_BYTES_VARIABLE,
        MAX_TOTAL_BYTES_VARIABLE, SCAN_CONTENT_BYTES_VARIABLE, SCAN_RECORDS_VARIABLE,
        parse_positive_usize,
    };

    #[test]
    fn deployment_limits_must_be_positive_integers() {
        for name in [
            MAX_RECORDS_VARIABLE,
            MAX_RECORD_BYTES_VARIABLE,
            MAX_TOTAL_BYTES_VARIABLE,
            MAX_REQUEST_BYTES_VARIABLE,
            SCAN_RECORDS_VARIABLE,
            SCAN_CONTENT_BYTES_VARIABLE,
        ] {
            assert_eq!(parse_positive_usize("1024", name).unwrap(), 1_024);
            for value in ["", "0", "-1", "many", "18446744073709551616"] {
                assert!(matches!(
                    parse_positive_usize(value, name),
                    Err(ConfigError::Invalid { name: invalid_name }) if invalid_name == name
                ));
            }
        }
    }
}
