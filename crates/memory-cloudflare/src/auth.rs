//! Credential loading from one encrypted Worker secret.

use tact_memory::server::{Credential, protocol::RemoteRole};
use thiserror::Error;
use zeroize::Zeroizing;

pub(super) const CREDENTIALS_SECRET: &str = "TACT_MEMORY_CREDENTIALS";

/// A content-free failure to parse the deployment credential document.
#[derive(Debug, Error)]
pub(super) enum CredentialDocumentError {
    #[error("the credential document is empty")]
    Empty,
    #[error("credential line {line} must contain ROLE NAMESPACE TOKEN")]
    InvalidLine { line: usize },
    #[error("credential line {line} has an invalid role")]
    InvalidRole { line: usize },
    #[error("credential line {line} has an invalid namespace or bearer token")]
    InvalidCredential { line: usize },
}

/// Parses `ROLE NAMESPACE TOKEN` lines while keeping the source document zeroizing.
pub(super) fn parse_credentials(
    document: Zeroizing<String>,
) -> Result<Vec<Credential>, CredentialDocumentError> {
    let mut credentials = Vec::new();
    for (index, line) in document.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() {
            continue;
        }
        let mut fields = line.split_ascii_whitespace();
        let (Some(role), Some(namespace), Some(token), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(CredentialDocumentError::InvalidLine { line: line_number });
        };
        let role = match role {
            "reader" => RemoteRole::Reader,
            "writer" => RemoteRole::Writer,
            _ => return Err(CredentialDocumentError::InvalidRole { line: line_number }),
        };
        let credential = Credential::new(namespace.to_owned(), role, token.to_owned())
            .map_err(|_| CredentialDocumentError::InvalidCredential { line: line_number })?;
        credentials.push(credential);
    }
    if credentials.is_empty() {
        return Err(CredentialDocumentError::Empty);
    }
    Ok(credentials)
}

#[cfg(test)]
mod tests {
    use super::{CredentialDocumentError, parse_credentials};
    use zeroize::Zeroizing;

    #[test]
    fn parses_reader_and_writer_lines_without_echoing_input() {
        let credentials = parse_credentials(Zeroizing::new(
            "writer alice alice-token\nreader auditor audit-token".to_owned(),
        ))
        .unwrap();
        assert_eq!(credentials.len(), 2);

        let error = parse_credentials(Zeroizing::new("writer alice".to_owned())).unwrap_err();
        assert!(matches!(
            error,
            CredentialDocumentError::InvalidLine { line: 1 }
        ));
        assert!(!error.to_string().contains("alice"));
    }
}
