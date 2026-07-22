//! Application-owned secret storage with redaction and zeroization.

use crate::error::SecretError;
use std::{
    env::{self, VarError},
    fmt,
};
use zeroize::{Zeroize, ZeroizeOnDrop};

/// An application-owned secret that is redacted and zeroized on drop.
#[derive(Zeroize, ZeroizeOnDrop)]
pub(crate) struct SecretString(String);

impl SecretString {
    pub(crate) fn from_environment(name: &'static str) -> Result<Option<Self>, SecretError> {
        match env::var(name) {
            Ok(value) => {
                let secret = Self::new(value);
                if secret.expose_secret().trim().is_empty() {
                    return Ok(None);
                }

                Ok(Some(secret))
            }
            Err(VarError::NotPresent) => Ok(None),
            Err(VarError::NotUnicode(_)) => Err(SecretError { name }),
        }
    }

    pub(crate) fn new(value: String) -> Self {
        Self(value)
    }

    pub(crate) fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("SecretString")
            .field(&"[REDACTED]")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::SecretString;
    use zeroize::Zeroize;

    #[test]
    fn debug_output_is_redacted() {
        let secret = SecretString::new("secret-sentinel".into());

        let output = format!("{secret:?}");
        assert!(!output.contains(secret.expose_secret()));
        assert_eq!(output, "SecretString(\"[REDACTED]\")");
    }

    #[test]
    fn explicit_zeroization_clears_the_value() {
        let mut secret = SecretString::new("secret-sentinel".into());

        secret.zeroize();

        assert!(secret.expose_secret().is_empty());
    }
}
