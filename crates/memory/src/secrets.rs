//! Conservative checks that keep likely credentials out of persistent memory.

use zeroize::Zeroizing;

const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "akia",
];

const ASSIGNMENT_NAMES: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "private_key",
    "private-key",
    "api_key",
    "api-key",
    "apikey",
];

pub(super) fn contains_likely_secret(content: &str) -> bool {
    let lowercase = Zeroizing::new(content.to_ascii_lowercase());

    contains_private_key(&lowercase)
        || contains_authorization(&lowercase)
        || contains_credential_url(content)
        || contains_prefixed_secret(&lowercase)
        || contains_jwt(content)
        || contains_secret_assignment(&lowercase)
}

fn contains_private_key(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim();
        line.starts_with("-----begin ") && line.ends_with("private key-----")
    })
}

fn contains_authorization(content: &str) -> bool {
    content.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("authorization:")
            || line.starts_with("authorization=")
            || has_bearer_token(line)
    })
}

fn has_bearer_token(content: &str) -> bool {
    content.match_indices("bearer ").any(|(index, _)| {
        let token = Zeroizing::new(
            content[index + "bearer ".len()..]
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric()
                        || matches!(character, '-' | '_' | '.' | '~' | '+' | '/')
                })
                .take(128)
                .collect::<String>(),
        );
        token.len() >= 12
            && token
                .chars()
                .any(|character| !character.is_ascii_alphabetic())
    })
}

fn contains_credential_url(content: &str) -> bool {
    content.split_whitespace().any(|word| {
        let Some((_, remainder)) = word.split_once("://") else {
            return false;
        };
        let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
        let Some((credentials, _)) = authority.rsplit_once('@') else {
            return false;
        };
        let Some((username, password)) = credentials.split_once(':') else {
            return false;
        };
        !username.is_empty() && !password.is_empty()
    })
}

fn contains_prefixed_secret(content: &str) -> bool {
    SECRET_PREFIXES.iter().any(|prefix| {
        content.match_indices(prefix).any(|(index, _)| {
            if index > 0 {
                let previous = content[..index].chars().next_back().unwrap();
                if previous.is_ascii_alphanumeric() || previous == '_' {
                    return false;
                }
            }
            let candidate = &content[index + prefix.len()..];
            candidate
                .chars()
                .take_while(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '_' | '-')
                })
                .take(16)
                .count()
                >= 12
        })
    })
}

fn contains_jwt(content: &str) -> bool {
    content.split_whitespace().any(|word| {
        let candidate = word.trim_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !matches!(character, '-' | '_' | '.')
        });
        let mut segments = candidate.split('.');
        let (Some(header), Some(payload), Some(signature), None) = (
            segments.next(),
            segments.next(),
            segments.next(),
            segments.next(),
        ) else {
            return false;
        };

        header.starts_with("eyJ")
            && header.len() >= 8
            && payload.len() >= 8
            && signature.len() >= 8
            && [header, payload, signature]
                .iter()
                .all(|segment| segment.chars().all(is_base64_url_character))
    })
}

fn is_base64_url_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn contains_secret_assignment(content: &str) -> bool {
    ASSIGNMENT_NAMES.iter().any(|name| {
        content.match_indices(name).any(|(index, _)| {
            let remainder = &content[index + name.len()..];
            let remainder = remainder.trim_start();
            let Some(value) = remainder
                .strip_prefix('=')
                .or_else(|| remainder.strip_prefix(':'))
            else {
                return false;
            };
            !value.trim_start().is_empty()
        })
    })
}

#[cfg(test)]
mod tests {
    use super::contains_likely_secret;

    #[test]
    fn rejects_supported_secret_shapes() {
        let secrets = [
            "sk-1234567890123456",
            "GITHUB_PAT_1234567890123456",
            "Authorization: Basic abc",
            "Bearer abcdefghijk1234",
            "https://user:hunter2@example.com/repository",
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.abcdefghijklmnop",
            "password = hunter2",
            "private_key: material",
            "DATABASE_PASSWORD=hunter2",
            "CLIENT_SECRET=hunter2",
            "MY_TOKEN=hunter2",
            "clientSecret=hunter2",
            "databasePassword=hunter2",
            "apiToken=hunter2",
            "-----BEGIN OPENSSH PRIVATE KEY-----",
        ];

        for secret in secrets {
            assert!(
                contains_likely_secret(secret),
                "did not reject secret shape"
            );
        }
    }

    #[test]
    fn allows_security_discussion_without_secret_material() {
        let safe = [
            "Use Bearer authentication for this endpoint.",
            "Rotate the password regularly.",
            "The token parser handles punctuation.",
            "Connect to https://example.com/repository.",
            "An API key should never be logged.",
            "The task-123456789012 identifier belongs to the scheduler.",
        ];

        for content in safe {
            assert!(!contains_likely_secret(content), "rejected safe prose");
        }
    }
}
