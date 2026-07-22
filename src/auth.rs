//! Authentication selection and shared ChatGPT credential management.

use crate::{
    config::{AuthConfig, AuthMode},
    error::{AuthError, AuthResult, SecretError},
    secret::SecretString,
};
use nanocodex::{
    ChatGptAuthStatus, ChatGptLogin, OpenAiAuth, chatgpt_auth_status, load_chatgpt_auth,
    logout_chatgpt,
};
use std::{path::Path, result::Result as StdResult};

const OPENAI_API_KEY: &str = "OPENAI_API_KEY";

enum SelectedAuth {
    ChatGpt,
    ApiKey(SecretString),
}

impl AuthConfig {
    pub(crate) async fn login(&self) -> AuthResult<()> {
        let login = ChatGptLogin::start(self.file()).await?;

        eprintln!(
            "Open this URL to sign in with ChatGPT:\n\n{}\n",
            login.authorization_url()
        );
        if let Err(error) = crate::browser::open(login.authorization_url()) {
            eprintln!(
                "Could not open a browser automatically ({error}). Open the URL above manually."
            );
        }

        let account = login.complete().await?;
        eprintln!("{}", self.login_success(&account));
        Ok(())
    }

    pub(crate) fn load(&self) -> AuthResult<OpenAiAuth> {
        let selected = self.select_auth(|| SecretString::from_environment(OPENAI_API_KEY))?;

        selected.into_openai_auth(self.file())
    }

    pub(crate) fn status(&self) -> AuthResult<()> {
        match self.select_auth(|| SecretString::from_environment(OPENAI_API_KEY))? {
            SelectedAuth::ChatGpt => self.print_chatgpt_status()?,
            SelectedAuth::ApiKey(_api_key) => {
                println!("Authentication: OpenAI API key");
                println!("Source: {OPENAI_API_KEY}");
            }
        }

        Ok(())
    }

    pub(crate) fn logout(&self) -> AuthResult<()> {
        if logout_chatgpt(self.file())? {
            eprintln!(
                "Removed shared ChatGPT credentials from {}. Tact and Codex are logged out.",
                self.file().display()
            );
            return Ok(());
        }

        eprintln!(
            "No ChatGPT credentials were stored at {}.",
            self.file().display()
        );
        Ok(())
    }

    fn select_auth<F>(&self, read_api_key: F) -> AuthResult<SelectedAuth>
    where
        F: FnOnce() -> StdResult<Option<SecretString>, SecretError>,
    {
        match self.mode() {
            AuthMode::ChatGpt => Ok(SelectedAuth::ChatGpt),
            AuthMode::ApiKey => read_api_key()?
                .map(SelectedAuth::ApiKey)
                .ok_or(AuthError::ApiKeyUnavailable),
            AuthMode::Auto => {
                if self
                    .file()
                    .try_exists()
                    .map_err(|source| AuthError::InspectCredentialFile {
                        path: self.file().to_path_buf(),
                        source,
                    })?
                {
                    return Ok(SelectedAuth::ChatGpt);
                }

                read_api_key()?.map(SelectedAuth::ApiKey).ok_or_else(|| {
                    AuthError::CredentialsUnavailable {
                        path: self.file().to_path_buf(),
                    }
                })
            }
        }
    }

    fn print_chatgpt_status(&self) -> AuthResult<()> {
        let account = chatgpt_auth_status(self.file())?;
        println!("Authentication: ChatGPT");
        if let Some(email) = account.email {
            println!("Email: {email}");
        }
        if let Some(plan) = account.plan {
            println!("Plan: {plan}");
        }
        println!("Account: {}", account.account_id);
        println!("FedRAMP: {}", account.fedramp);
        println!("Credentials: {}", self.file().display());
        Ok(())
    }

    fn login_success(&self, account: &ChatGptAuthStatus) -> String {
        let identity = account
            .email
            .as_deref()
            .map_or(String::new(), |email| format!(" as {email}"));
        format!(
            "Tact and Codex are logged in{identity} (account {}). Credentials saved to {}.",
            account.account_id,
            self.file().display()
        )
    }
}

impl SelectedAuth {
    fn into_openai_auth(self, auth_file: &Path) -> AuthResult<OpenAiAuth> {
        match self {
            Self::ChatGpt => load_chatgpt_auth(auth_file).map_err(Into::into),
            Self::ApiKey(api_key) => {
                // Nanocodex owns the retained key after this boundary. The application-owned
                // buffer is still zeroized when `api_key` is dropped.
                Ok(OpenAiAuth::api_key(api_key.expose_secret()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::SelectedAuth;
    use crate::{
        config::{AuthConfig, AuthMode},
        error::AuthError,
        secret::SecretString,
    };
    use nanocodex::OpenAiAuthMode;
    use std::{cell::Cell, fs};
    use tempfile::tempdir;

    #[test]
    fn auto_prefers_an_existing_chatgpt_file_without_reading_the_api_key() {
        let directory = tempdir().unwrap();
        let auth_file = directory.path().join("auth.json");
        fs::write(&auth_file, "invalid but present").unwrap();
        let api_key_read = Cell::new(false);

        let config = AuthConfig::new(AuthMode::Auto, auth_file);
        let selected = config
            .select_auth(|| {
                api_key_read.set(true);
                Ok(Some(SecretString::new("api-key".into())))
            })
            .unwrap();

        assert!(matches!(selected, SelectedAuth::ChatGpt));
        assert!(!api_key_read.get());
    }

    #[test]
    fn auto_falls_back_to_an_api_key_when_chatgpt_is_absent() {
        let directory = tempdir().unwrap();
        let config = AuthConfig::new(AuthMode::Auto, directory.path().join("auth.json"));
        let selected = config
            .select_auth(|| Ok(Some(SecretString::new("api-key".into()))))
            .unwrap();

        assert!(matches!(selected, SelectedAuth::ApiKey(_)));
    }

    #[test]
    fn forced_chatgpt_does_not_read_the_api_key() {
        let api_key_read = Cell::new(false);
        let config = AuthConfig::new(AuthMode::ChatGpt, "missing.json".into());
        let selected = config
            .select_auth(|| {
                api_key_read.set(true);
                Ok(Some(SecretString::new("api-key".into())))
            })
            .unwrap();

        assert!(matches!(selected, SelectedAuth::ChatGpt));
        assert!(!api_key_read.get());
    }

    #[test]
    fn forced_api_key_reports_a_missing_environment_value() {
        let config = AuthConfig::new(AuthMode::ApiKey, "unused.json".into());
        let result = config.select_auth(|| Ok(None));

        assert!(matches!(result, Err(AuthError::ApiKeyUnavailable)));
    }

    #[test]
    fn selected_api_key_constructs_nanocodex_authorization() {
        let selected = SelectedAuth::ApiKey(SecretString::new("api-key".into()));
        let auth = selected.into_openai_auth("unused.json".as_ref()).unwrap();

        assert_eq!(auth.mode(), OpenAiAuthMode::ApiKey);
    }

    #[test]
    fn logout_is_idempotent() {
        let directory = tempdir().unwrap();
        let auth_file = directory.path().join("auth.json");
        fs::write(&auth_file, "credentials").unwrap();
        let config = AuthConfig::new(AuthMode::ChatGpt, auth_file.clone());

        config.logout().unwrap();
        assert!(!auth_file.exists());
        config.logout().unwrap();
    }
}
