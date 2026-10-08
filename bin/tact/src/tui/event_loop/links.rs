//! Links the terminal opens: transcript links and the web interface's sign-in link.

use crate::web::bridge::WebStatus;
use std::path::{Path, PathBuf};
use tokio::sync::watch;

/// Whether a transcript link opens in the browser rather than in the editor.
pub(super) fn is_web_link(destination: &str) -> bool {
    destination.starts_with("https://") || destination.starts_with("http://")
}

/// The file a local transcript link names, resolved against `workspace`. A trailing line
/// reference such as `:42` or `#L42` is dropped.
pub(super) fn local_link_path(destination: &str, workspace: &Path) -> PathBuf {
    let destination = destination.strip_prefix("file://").unwrap_or(destination);
    let destination = destination
        .rsplit_once("#L")
        .filter(|(_, line)| line.parse::<u32>().is_ok())
        .map_or(destination, |(path, _)| path);
    let destination = destination
        .rsplit_once(':')
        .filter(|(_, line)| line.parse::<u32>().is_ok())
        .map_or(destination, |(path, _)| path);
    let path = Path::new(destination);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        workspace.join(path)
    }
}

/// The login link of the web interface. It embeds the credential, so it is only opened or
/// copied on the user's request and never displayed or logged.
pub(super) fn web_link(
    status: &watch::Receiver<WebStatus>,
    enabled: bool,
) -> Result<String, String> {
    if !enabled {
        return Err("The web interface is disabled; set web.enabled = true.".to_owned());
    }
    match &*status.borrow() {
        WebStatus::Ready { url, .. } => Ok(url.clone()),
        WebStatus::Starting => Err("The web interface is still starting.".to_owned()),
        WebStatus::Unavailable { reason } => {
            Err(format!("The web interface is unavailable: {reason}"))
        }
    }
}

/// A sign-in link that a phone can use.
///
/// With `web.tailscale`, the server is published to the tailnet now, and Tailscale is checked
/// again on every call, so a client that was switched on after an earlier refusal is picked up.
pub(super) async fn phone_link(
    link: String,
    status: &watch::Receiver<WebStatus>,
) -> Result<String, String> {
    let tailnet = match &*status.borrow() {
        WebStatus::Ready { tailnet, .. } => tailnet.clone(),
        _ => None,
    };
    let Some(tailnet) = tailnet else {
        return reachable_link(link);
    };
    let origin = tailnet
        .origin()
        .await
        .map_err(|error| format!("Cannot share over Tailscale: {error}"))?;
    let (_, credential) = link
        .split_once('#')
        .ok_or_else(|| "The web link has no sign-in credential.".to_owned())?;
    Ok(format!("{origin}/#{credential}"))
}

/// Refuses a link whose address is this computer's own (the default when neither
/// `web.tailscale` nor `web.public_url` is set) rather than encoding it into a code that cannot
/// work.
fn reachable_link(link: String) -> Result<String, String> {
    use url::Host;
    let reachable = url::Url::parse(&link)
        .ok()
        .and_then(|url| {
            url.host().map(|host| match host {
                Host::Domain(domain) => domain != "localhost" && !domain.ends_with(".localhost"),
                Host::Ipv4(address) => !address.is_loopback() && !address.is_unspecified(),
                Host::Ipv6(address) => !address.is_loopback() && !address.is_unspecified(),
            })
        })
        .unwrap_or(false);
    if reachable {
        Ok(link)
    } else {
        Err("The web link points at this computer, so a phone cannot use it. Set web.tailscale = true, or web.public_url to your tunnel's address.".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::{is_web_link, local_link_path, reachable_link};
    use std::path::Path;

    #[test]
    fn a_phone_cannot_use_a_link_to_this_computer() {
        for local in [
            "http://127.0.0.1:7878/#k=t",
            "http://localhost:7878/#k=t",
            "http://[::1]:7878/#k=t",
            "http://0.0.0.0:7878/#k=t",
        ] {
            assert!(reachable_link(local.to_owned()).is_err(), "{local}");
        }
        for reachable in [
            "https://laptop.tail1234.ts.net/#k=t",
            "http://100.64.0.7:7878/#k=t",
        ] {
            assert_eq!(
                reachable_link(reachable.to_owned()).as_deref(),
                Ok(reachable)
            );
        }
    }

    #[test]
    fn local_links_resolve_against_the_workspace_and_ignore_line_suffixes() {
        let workspace = Path::new("/work/project");

        assert_eq!(
            local_link_path("src/main.rs:42", workspace),
            workspace.join("src/main.rs")
        );
        assert_eq!(
            local_link_path("file:///tmp/example.rs#L7", workspace),
            Path::new("/tmp/example.rs")
        );
        assert_eq!(
            local_link_path("file.txt", workspace),
            workspace.join("file.txt")
        );
    }

    #[test]
    fn only_http_links_open_in_the_browser() {
        assert!(is_web_link("https://example.com"));
        assert!(is_web_link("http://example.com"));
        assert!(!is_web_link("file:///tmp/example.rs"));
        assert!(!is_web_link("src/main.rs"));
    }
}
