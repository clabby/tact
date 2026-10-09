//! The machine token that authorizes browsers.
//!
//! One token per machine user lives at `$TACT_HOME/web/token` so that every Tact instance of that user
//! accepts the same credential and sibling instances can query each other. Whoever holds it can run
//! commands as the user, so the file is private and the in-memory copy is zeroized.

use crate::app::secret::SecretString;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};
use subtle::ConstantTimeEq;
use thiserror::Error;

const TOKEN_BYTES: usize = 32;

#[derive(Debug, Error)]
pub(crate) enum TokenError {
    #[error("failed to read the web token {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("failed to create the web token {path}: {source}")]
    Create {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("the system random number generator failed: {0}")]
    Random(getrandom::Error),
}

pub(crate) struct MachineToken(SecretString);

impl MachineToken {
    /// Loads the token in `directory`, creating it first if it is missing or malformed.
    ///
    /// Creation is atomic and create-if-absent, so instances starting together converge on one token.
    pub(crate) fn load_or_create(directory: &Path) -> Result<Self, TokenError> {
        let path = directory.join("token");
        match fs::read_to_string(&path) {
            Ok(contents) => {
                let contents = SecretString::new(contents);
                if let Some(token) = Self::parse(contents.expose_secret()) {
                    return Ok(token);
                }
                fs::remove_file(&path).map_err(|source| TokenError::Read {
                    path: path.clone(),
                    source,
                })?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(source) => return Err(TokenError::Read { path, source }),
        }
        Self::create(&path)
    }

    /// The token in `contents`, ignoring surrounding whitespace, if it is well formed.
    pub(super) fn parse(contents: &str) -> Option<Self> {
        let token = contents.trim();
        let valid = URL_SAFE_NO_PAD
            .decode(token)
            .is_ok_and(|bytes| bytes.len() == TOKEN_BYTES);
        valid.then(|| Self(SecretString::new(token.to_owned())))
    }

    /// The token in `directory`, if one exists and is well formed. Nothing is created.
    pub(super) fn read(directory: &Path) -> io::Result<Option<Self>> {
        match fs::read_to_string(directory.join("token")) {
            Ok(contents) => Ok(Self::parse(SecretString::new(contents).expose_secret())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn create(path: &Path) -> Result<Self, TokenError> {
        let create_error = |source| TokenError::Create {
            path: path.to_owned(),
            source,
        };
        let mut bytes = [0_u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(TokenError::Random)?;
        let token = SecretString::new(URL_SAFE_NO_PAD.encode(bytes));
        bytes.fill(0);

        let directory = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(directory).map_err(create_error)?;
        let temporary = path.with_extension(format!("tmp.{}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary).map_err(create_error)?;
        file.write_all(token.expose_secret().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(create_error)?;
        // A hard link fails when another instance created the token first; that one wins.
        let linked = fs::hard_link(&temporary, path);
        drop(fs::remove_file(&temporary));
        match linked {
            Ok(()) => Ok(Self(token)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let existing = SecretString::new(fs::read_to_string(path).map_err(create_error)?);
                Self::parse(existing.expose_secret()).ok_or_else(|| {
                    create_error(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "the existing token is malformed",
                    ))
                })
            }
            Err(error) => Err(create_error(error)),
        }
    }

    /// Compares in constant time with respect to the candidate's contents.
    pub(crate) fn matches(&self, candidate: &str) -> bool {
        self.0
            .expose_secret()
            .as_bytes()
            .ct_eq(candidate.as_bytes())
            .into()
    }

    pub(crate) fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl std::fmt::Debug for MachineToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MachineToken([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use super::MachineToken;
    use std::fs;

    #[test]
    fn token_is_created_private_and_reused() {
        let directory = tempfile::tempdir().unwrap();
        let first = MachineToken::load_or_create(directory.path()).unwrap();
        let second = MachineToken::load_or_create(directory.path()).unwrap();

        assert_eq!(first.expose(), second.expose());
        assert_eq!(first.expose().len(), 43);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(directory.path().join("token"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn malformed_token_file_is_replaced() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("token"), "short").unwrap();

        let token = MachineToken::load_or_create(directory.path()).unwrap();

        assert_eq!(token.expose().len(), 43);
    }

    #[test]
    fn comparison_accepts_only_the_exact_token() {
        let directory = tempfile::tempdir().unwrap();
        let token = MachineToken::load_or_create(directory.path()).unwrap();

        assert!(token.matches(token.expose()));
        assert!(!token.matches(""));
        assert!(!token.matches(&format!("{}x", token.expose())));
        assert!(!token.matches(&token.expose()[1..]));
        assert_eq!(format!("{token:?}"), "MachineToken([REDACTED])");
    }
}
