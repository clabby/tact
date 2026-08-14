//! Startup credentials and retained authentication principals.
//!
//! Raw bearer tokens exist only while credentials are assembled. Server construction hashes each
//! token, consumes the credential, and retains only the digest, namespace, and role.

use super::protocol::{self, RemoteRole};
use sha2::{Digest, Sha256};
use std::{fmt, mem};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

const MAX_BEARER_TOKEN_BYTES: usize = 4096;

struct BearerToken(Zeroizing<String>);

impl BearerToken {
    fn new(token: String) -> Result<Self, CredentialError> {
        let token = Zeroizing::new(token);
        if token.is_empty()
            || token.len() > MAX_BEARER_TOKEN_BYTES
            || !token.bytes().all(is_bearer_token_byte)
        {
            return Err(CredentialError::InvalidBearerToken);
        }
        Ok(Self(token))
    }

    fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("BearerToken([REDACTED])")
    }
}

impl Zeroize for BearerToken {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}

impl Drop for BearerToken {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// A startup credential consumed by [`MemoryServer::new`](super::MemoryServer::new).
///
/// The server hashes the token and drops this value before serving requests. This type
/// deliberately implements neither `Clone`, `Display`, nor serialization.
pub struct Credential {
    namespace: String,
    role: RemoteRole,
    token: BearerToken,
}

impl Credential {
    /// Validates a namespace and bearer token for the given remote role.
    pub fn new(
        namespace: String,
        role: RemoteRole,
        bearer_token: String,
    ) -> Result<Self, CredentialError> {
        let token = BearerToken::new(bearer_token)?;
        if !protocol::is_valid_namespace(&namespace) {
            return Err(CredentialError::InvalidNamespace);
        }
        Ok(Self {
            namespace,
            role,
            token,
        })
    }

    pub(crate) fn into_hashed_principal(mut self) -> ([u8; 32], Principal) {
        let token_hash = hash_token(self.token.expose());
        let principal = Principal {
            namespace: mem::take(&mut self.namespace),
            role: self.role,
        };
        (token_hash, principal)
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credential")
            .field("namespace", &self.namespace)
            .field("role", &self.role)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Zeroize for Credential {
    fn zeroize(&mut self) {
        self.token.zeroize();
    }
}

impl Drop for Credential {
    fn drop(&mut self) {
        self.zeroize();
    }
}

/// Failure to construct a startup credential.
#[derive(Debug, Error)]
pub enum CredentialError {
    /// The namespace does not satisfy the protocol grammar.
    #[error("memory namespace is invalid")]
    InvalidNamespace,
    /// The bearer token is empty, too large, or contains an unsupported byte.
    #[error("bearer token is invalid")]
    InvalidBearerToken,
}

#[derive(Clone)]
pub(crate) struct Principal {
    pub(crate) namespace: String,
    pub(crate) role: RemoteRole,
}

pub(crate) fn is_bearer_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
}

pub(crate) fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
