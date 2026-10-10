//! Provider authentication selection and shared ChatGPT credential management.

use crate::app::{
    config::{AuthConfig, AuthMode, ClaudeConfig, DecisionsConfig},
    error::{AuthError, AuthResult, SecretError},
    secret::SecretString,
};
use nanocodex::oai::auth::{
    ChatGptAuthStatus, ChatGptLogin, OpenAiAuth, load_chatgpt_auth, logout_chatgpt,
    resolve_chatgpt_auth_status,
};
use std::{path::Path, result::Result as StdResult, sync::Arc};

const OPENAI_API_KEY: &str = "OPENAI_API_KEY";

pub(crate) fn validate_claude_api_key(key: &SecretString) -> AuthResult<()> {
    let value = key.expose_secret();
    if !(value.starts_with("sk-ant-api") || value.starts_with("sk-ant-usr-"))
        || value.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(AuthError::InvalidClaudeApiKey);
    }
    Ok(())
}

enum SelectedAuth {
    ChatGpt,
    ApiKey(OpenAiApiKey),
}

#[derive(Debug)]
pub(crate) enum OpenAiApiKey {
    Environment(SecretString),
    Config(Arc<SecretString>),
    DecisionsConfig(Arc<SecretString>),
}

impl OpenAiApiKey {
    pub(crate) fn key(&self) -> &SecretString {
        match self {
            Self::Environment(key) => key,
            Self::Config(key) | Self::DecisionsConfig(key) => key,
        }
    }

    const fn source(&self) -> &'static str {
        match self {
            Self::Environment(_) => OPENAI_API_KEY,
            Self::Config(_) => "openai.api_key",
            Self::DecisionsConfig(_) => "openai.decisions_api_key",
        }
    }
}

#[derive(Debug)]
pub(crate) enum ClaudeApiKey {
    Environment(SecretString),
    Config(Arc<SecretString>),
}

impl ClaudeConfig {
    pub(crate) fn resolve_api_key(
        &self,
        read_environment: impl FnOnce() -> StdResult<Option<SecretString>, SecretError>,
    ) -> AuthResult<Option<ClaudeApiKey>> {
        let selected = match read_environment()? {
            Some(key) => Some(ClaudeApiKey::Environment(key)),
            None => self
                .api_key()
                .map(|key| ClaudeApiKey::Config(Arc::clone(key))),
        };
        if let Some(key) = &selected {
            validate_claude_api_key(key.key())?;
        }
        Ok(selected)
    }
}

impl ClaudeApiKey {
    pub(crate) fn key(&self) -> &SecretString {
        match self {
            Self::Environment(key) => key,
            Self::Config(key) => key,
        }
    }

    pub(crate) const fn source(&self) -> &'static str {
        match self {
            Self::Environment(_) => "ANTHROPIC_API_KEY",
            Self::Config(_) => "claude.api_key",
        }
    }
}

impl DecisionsConfig {
    /// Selects the OpenAI Platform key for Decisions API requests, or `None` when decisions are
    /// disabled.
    pub(crate) fn resolve_api_key(&self, auth: &AuthConfig) -> AuthResult<Option<OpenAiApiKey>> {
        self.select_api_key(auth, || SecretString::from_environment(OPENAI_API_KEY))
    }

    /// An OpenAI API key is used whenever one is available, whatever the authentication mode.
    /// A ChatGPT subscription cannot authorize Platform requests, so setups without an API key
    /// must provide `openai.decisions_api_key`.
    fn select_api_key<F>(
        &self,
        auth: &AuthConfig,
        read_api_key: F,
    ) -> AuthResult<Option<OpenAiApiKey>>
    where
        F: FnOnce() -> StdResult<Option<SecretString>, SecretError>,
    {
        if !self.enabled() {
            return Ok(None);
        }
        let selected = match auth.configured_api_key(read_api_key)? {
            Some(key) => key,
            None => self
                .api_key()
                .map(|key| OpenAiApiKey::DecisionsConfig(Arc::clone(key)))
                .ok_or(AuthError::DecisionsApiKeyUnavailable)?,
        };
        Ok(Some(selected))
    }
}

impl AuthConfig {
    pub(crate) async fn login(&self, open_automatically: bool) -> AuthResult<()> {
        let login = ChatGptLogin::start(self.file()).await?;

        eprintln!(
            "Open this URL to sign in with ChatGPT:\n\n{}\n",
            login.authorization_url()
        );
        if open_automatically
            && let Err(error) = crate::app::browser::open(login.authorization_url()).await
        {
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

    pub(crate) async fn status(&self) -> AuthResult<()> {
        match self.select_auth(|| SecretString::from_environment(OPENAI_API_KEY))? {
            SelectedAuth::ChatGpt => self.print_chatgpt_status().await?,
            SelectedAuth::ApiKey(api_key) => {
                println!("Authentication: OpenAI API key");
                println!("Source: {}", api_key.source());
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
            AuthMode::ChatGpt => return Ok(SelectedAuth::ChatGpt),
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
            }
            AuthMode::ApiKey => {}
        }

        self.configured_api_key(read_api_key)?
            .map(SelectedAuth::ApiKey)
            .ok_or_else(|| match self.mode() {
                AuthMode::ApiKey => AuthError::ApiKeyUnavailable,
                _ => AuthError::CredentialsUnavailable {
                    path: self.file().to_path_buf(),
                },
            })
    }

    /// The OpenAI API key from the environment, then the configuration file.
    fn configured_api_key<F>(&self, read_api_key: F) -> AuthResult<Option<OpenAiApiKey>>
    where
        F: FnOnce() -> StdResult<Option<SecretString>, SecretError>,
    {
        Ok(match read_api_key()? {
            Some(key) => Some(OpenAiApiKey::Environment(key)),
            None => self
                .api_key()
                .map(|key| OpenAiApiKey::Config(Arc::clone(key))),
        })
    }

    async fn print_chatgpt_status(&self) -> AuthResult<()> {
        let account = resolve_chatgpt_auth_status(self.file()).await?;
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
                // Nanocodex retains a non-zeroizing copy. The application-owned secret is
                // zeroized when its last application owner is dropped.
                Ok(OpenAiAuth::api_key(api_key.key().expose_secret()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn claude_api_key_validation_rejects_other_credentials_without_disclosing_them() {
        for value in [
            "sk-ant-oat01-access-sentinel",
            "sk-ant-ort01-refresh-sentinel",
            "Bearer sk-ant-oat01-access-sentinel",
            "session-sentinel",
            "",
            " sk-ant-api03-sentinel",
            "sk-ant-api03-sentinel\n",
            " sk-ant-usr-sentinel",
            "sk-ant-usr-sentinel\n",
            "sk-ant-usr",
        ] {
            let error = super::validate_claude_api_key(&SecretString::new(value.into()))
                .expect_err("only API keys are accepted");
            assert!(matches!(error, AuthError::InvalidClaudeApiKey));
            assert!(!error.to_string().contains("sentinel"));
        }
        for value in ["sk-ant-api03-sentinel", "sk-ant-usr-sentinel"] {
            assert!(super::validate_claude_api_key(&SecretString::new(value.into())).is_ok());
        }
    }

    use super::{OpenAiApiKey, SelectedAuth};
    use crate::app::{
        config::{AuthConfig, AuthMode, ClaudeConfig, DecisionsConfig},
        error::{AuthError, SecretError},
        secret::SecretString,
    };
    use nanocodex::oai::auth::OpenAiAuthMode;
    use std::{cell::Cell, fs, sync::Arc};
    use tempfile::tempdir;

    #[test]
    fn claude_key_selection_prefers_environment_then_config() {
        for (configured, environment, expected_source) in [
            (None, None, None),
            (
                Some("sk-ant-api03-config-sentinel"),
                None,
                Some("claude.api_key"),
            ),
            (
                None,
                Some("sk-ant-usr-env-sentinel"),
                Some("ANTHROPIC_API_KEY"),
            ),
            (
                Some("sk-ant-api03-config-sentinel"),
                Some("sk-ant-usr-env-sentinel"),
                Some("ANTHROPIC_API_KEY"),
            ),
            (
                Some("invalid-config-sentinel"),
                Some("sk-ant-usr-env-sentinel"),
                Some("ANTHROPIC_API_KEY"),
            ),
            (Some(" \t"), None, None),
        ] {
            let text = configured.map_or(String::new(), |key| format!("api_key = {key:?}"));
            let config: ClaudeConfig = toml::from_str(&text).unwrap();
            let selected = config
                .resolve_api_key(|| Ok(environment.map(|key| SecretString::new(key.into()))))
                .unwrap();
            assert_eq!(selected.as_ref().map(|key| key.source()), expected_source);
            if let Some(key) = selected {
                assert_eq!(
                    key.key().expose_secret(),
                    environment.or(configured).unwrap()
                );
                assert!(!format!("{key:?}").contains("sentinel"));
            }
        }
    }

    #[test]
    fn claude_key_selection_rejects_invalid_selected_credentials_without_fallback() {
        for (configured, environment) in [
            (
                "sk-ant-usr-config-sentinel",
                Some("sk-ant-oat01-env-sentinel"),
            ),
            ("sk-ant-oat01-config-sentinel", None),
            ("sk-ant-api03-config-sentinel\n", None),
        ] {
            let config: ClaudeConfig =
                toml::from_str(&format!("api_key = {configured:?}")).unwrap();
            let error = config
                .resolve_api_key(|| Ok(environment.map(|key| SecretString::new(key.into()))))
                .unwrap_err();
            assert!(matches!(error, AuthError::InvalidClaudeApiKey));
            assert!(!format!("{error:?} {error}").contains("sentinel"));
        }
        let config: ClaudeConfig =
            toml::from_str("api_key = 'sk-ant-usr-config-sentinel'").unwrap();
        let error = config
            .resolve_api_key(|| {
                Err(SecretError {
                    name: "ANTHROPIC_API_KEY",
                })
            })
            .unwrap_err();
        assert!(matches!(error, AuthError::Secret(_)));
        assert!(!format!("{error:?} {error}").contains("sentinel"));
    }

    #[test]
    fn openai_key_selection_prefers_environment_then_config_without_changing_auth_mode() {
        let directory = tempdir().unwrap();
        let auth_file = directory.path().join("auth.json");
        fs::write(&auth_file, "present login").unwrap();
        for mode in [AuthMode::ApiKey, AuthMode::Auto] {
            if mode == AuthMode::Auto {
                fs::remove_file(&auth_file).unwrap();
            }
            for (configured, environment, expected_source) in [
                (Some("config-sentinel"), None, "openai.api_key"),
                (
                    Some("config-sentinel"),
                    Some("env-sentinel"),
                    "OPENAI_API_KEY",
                ),
                (None, Some("env-sentinel"), "OPENAI_API_KEY"),
            ] {
                let config = AuthConfig::new(
                    mode,
                    auth_file.clone(),
                    configured.map(|key| Arc::new(SecretString::new(key.into()))),
                );
                let selected = config
                    .select_auth(|| Ok(environment.map(|key| SecretString::new(key.into()))))
                    .unwrap();
                let SelectedAuth::ApiKey(key) = selected else {
                    panic!("expected API key")
                };
                assert_eq!(key.source(), expected_source);
                assert_eq!(
                    key.key().expose_secret(),
                    environment.or(configured).unwrap()
                );
                assert!(!format!("{key:?}").contains("sentinel"));
                let auth = SelectedAuth::ApiKey(key)
                    .into_openai_auth(&auth_file)
                    .unwrap();
                assert_eq!(auth.mode(), OpenAiAuthMode::ApiKey);
            }
        }
    }

    #[test]
    fn openai_environment_error_does_not_fall_back_to_config() {
        let config = AuthConfig::new(
            AuthMode::ApiKey,
            "unused.json".into(),
            Some(Arc::new(SecretString::new("config-sentinel".into()))),
        );
        let result = config.select_auth(|| {
            Err(SecretError {
                name: "OPENAI_API_KEY",
            })
        });
        assert!(matches!(result, Err(AuthError::Secret(_))));
    }

    #[test]
    fn decisions_prefer_an_openai_api_key_then_the_decisions_key() {
        let secret = |key: &str| Arc::new(SecretString::new(key.into()));
        for mode in [AuthMode::Auto, AuthMode::ChatGpt, AuthMode::ApiKey] {
            for (openai, environment, decisions, expected) in [
                (
                    None,
                    None,
                    Some("decisions-sentinel"),
                    Some("openai.decisions_api_key"),
                ),
                (
                    Some("config-sentinel"),
                    None,
                    Some("decisions-sentinel"),
                    Some("openai.api_key"),
                ),
                (
                    None,
                    Some("env-sentinel"),
                    Some("decisions-sentinel"),
                    Some("OPENAI_API_KEY"),
                ),
                (None, None, None, None),
            ] {
                let auth = AuthConfig::new(mode, "unused.json".into(), openai.map(secret));
                let config = DecisionsConfig::new(true, decisions.map(secret));
                let result = config.select_api_key(&auth, || {
                    Ok(environment.map(|key| SecretString::new(key.into())))
                });
                let Some(expected) = expected else {
                    let error = result.unwrap_err();
                    assert!(matches!(error, AuthError::DecisionsApiKeyUnavailable));
                    continue;
                };
                let key = result.unwrap().expect("decisions are enabled");
                assert_eq!(key.source(), expected);
                assert_eq!(
                    key.key().expose_secret(),
                    environment.or(openai).or(decisions).unwrap()
                );
                assert!(!format!("{key:?}").contains("sentinel"));
            }
        }
    }

    #[test]
    fn disabled_decisions_read_no_credentials() {
        let api_key_read = Cell::new(false);
        let auth = AuthConfig::new(AuthMode::ChatGpt, "unused.json".into(), None);
        let selected = DecisionsConfig::new(false, None)
            .select_api_key(&auth, || {
                api_key_read.set(true);
                Ok(None)
            })
            .unwrap();

        assert!(selected.is_none());
        assert!(!api_key_read.get());
    }

    #[test]
    fn auto_prefers_an_existing_chatgpt_file_without_reading_the_api_key() {
        let directory = tempdir().unwrap();
        let auth_file = directory.path().join("auth.json");
        fs::write(&auth_file, "invalid but present").unwrap();
        let api_key_read = Cell::new(false);

        let config = AuthConfig::new(
            AuthMode::Auto,
            auth_file,
            Some(Arc::new(SecretString::new("config-sentinel".into()))),
        );
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
        let config = AuthConfig::new(AuthMode::Auto, directory.path().join("auth.json"), None);
        let selected = config
            .select_auth(|| Ok(Some(SecretString::new("api-key".into()))))
            .unwrap();

        assert!(matches!(selected, SelectedAuth::ApiKey(_)));
    }

    #[test]
    fn forced_chatgpt_does_not_read_the_api_key() {
        let api_key_read = Cell::new(false);
        let config = AuthConfig::new(
            AuthMode::ChatGpt,
            "missing.json".into(),
            Some(Arc::new(SecretString::new("config-sentinel".into()))),
        );
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
    fn forced_api_key_reports_missing_credentials() {
        let config = AuthConfig::new(AuthMode::ApiKey, "unused.json".into(), None);
        let result = config.select_auth(|| Ok(None));

        assert!(matches!(result, Err(AuthError::ApiKeyUnavailable)));
    }

    #[test]
    fn selected_api_key_constructs_nanocodex_authorization() {
        let selected = SelectedAuth::ApiKey(OpenAiApiKey::Environment(SecretString::new(
            "api-key".into(),
        )));
        let auth = selected.into_openai_auth("unused.json".as_ref()).unwrap();

        assert_eq!(auth.mode(), OpenAiAuthMode::ApiKey);
    }

    #[test]
    fn logout_is_idempotent() {
        let directory = tempdir().unwrap();
        let auth_file = directory.path().join("auth.json");
        fs::write(&auth_file, "credentials").unwrap();
        let config = AuthConfig::new(AuthMode::ChatGpt, auth_file.clone(), None);

        config.logout().unwrap();
        assert!(!auth_file.exists());
        config.logout().unwrap();
    }
}
