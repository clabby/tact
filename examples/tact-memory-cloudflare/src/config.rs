//! Non-secret deployment configuration loaded from Worker bindings.

use crate::store::ScanBudget;
use thiserror::Error;
use worker::Env;

const SCAN_RECORDS_VARIABLE: &str = "TACT_MEMORY_SCAN_MAX_RECORDS";
const SCAN_CONTENT_BYTES_VARIABLE: &str = "TACT_MEMORY_SCAN_MAX_CONTENT_BYTES";

/// Failure to load a positive shared-scan resource budget.
#[derive(Debug, Error)]
pub(super) enum ConfigError {
    #[error("missing Worker variable {name}")]
    Missing { name: &'static str },
    #[error("Worker variable {name} must be a positive integer")]
    Invalid { name: &'static str },
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
    value
        .parse()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(ConfigError::Invalid { name })
}
