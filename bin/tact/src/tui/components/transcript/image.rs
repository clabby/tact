use image::ImageReader;
use ratatui::layout::Size;
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType},
    sliced::SlicedProtocol,
};
use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, OnceLock, Weak},
    time::SystemTime,
};
use url::Url;

pub(super) const MAX_IMAGE_HEIGHT: u16 = 24;

static PICKER: OnceLock<Picker> = OnceLock::new();

#[derive(Default)]
pub(super) struct Cache {
    entries: HashMap<CacheKey, CachedProtocol>,
    width: Option<u16>,
}

enum CachedProtocol {
    Failed,
    Loaded(Weak<SlicedProtocol>),
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct CacheKey {
    path: PathBuf,
    width: u16,
    len: Option<u64>,
    modified: Option<SystemTime>,
}

pub(crate) fn initialize() {
    let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    if picker.protocol_type() == ProtocolType::Halfblocks {
        let term = env::var("TERM").ok();
        let term_program = env::var("TERM_PROGRAM").ok();
        let inside_tmux = env::var_os("TMUX").is_some();
        let native_transport = !inside_tmux
            || picker_supports_tmux_passthrough(term.as_deref(), term_program.as_deref());
        let tmux_client = native_transport.then(tmux_client_termtype).flatten();
        if let Some(protocol) = protocol_hint(
            term_program.as_deref(),
            tmux_client.as_deref(),
            native_transport,
        ) {
            picker.set_protocol_type(protocol);
        }
    }
    drop(PICKER.set(picker));
}

fn protocol_hint(
    term_program: Option<&str>,
    tmux_client_termtype: Option<&str>,
    native_transport: bool,
) -> Option<ProtocolType> {
    if !native_transport {
        return None;
    }
    [term_program, tmux_client_termtype]
        .into_iter()
        .flatten()
        .filter_map(|terminal| terminal.split_ascii_whitespace().next())
        .any(|terminal| terminal.eq_ignore_ascii_case("ghostty"))
        .then_some(ProtocolType::Kitty)
}

fn picker_supports_tmux_passthrough(term: Option<&str>, term_program: Option<&str>) -> bool {
    term.is_some_and(|term| term.starts_with("tmux")) || term_program == Some("tmux")
}

fn tmux_client_termtype() -> Option<String> {
    env::var_os("TMUX")?;
    let output = Command::new("tmux")
        .args(["display-message", "-p", "#{client_termtype}"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

impl Cache {
    pub(super) fn load(
        &mut self,
        destination: &str,
        workspace: &Path,
        width: u16,
    ) -> Option<Arc<SlicedProtocol>> {
        if self.width != Some(width) {
            self.entries.clear();
            self.width = Some(width);
        }
        let path = local_path(destination, workspace)?;
        let metadata = fs::metadata(&path).ok();
        let key = CacheKey {
            path: path.clone(),
            width,
            len: metadata.as_ref().map(fs::Metadata::len),
            modified: metadata.and_then(|metadata| metadata.modified().ok()),
        };
        match self.entries.get(&key) {
            Some(CachedProtocol::Failed) => return None,
            Some(CachedProtocol::Loaded(protocol)) => {
                if let Some(protocol) = protocol.upgrade() {
                    return Some(protocol);
                }
            }
            None => {}
        }

        self.entries
            .retain(|cached, _| cached.path != path || cached.width != width);
        let protocol = load(&path, width);
        let cached = protocol
            .as_ref()
            .map_or(CachedProtocol::Failed, |protocol| {
                CachedProtocol::Loaded(Arc::downgrade(protocol))
            });
        self.entries.insert(key, cached);
        protocol
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
        self.width = None;
    }
}

fn load(path: &Path, width: u16) -> Option<Arc<SlicedProtocol>> {
    let image = ImageReader::open(path)
        .ok()?
        .with_guessed_format()
        .ok()?
        .decode()
        .ok()?;
    let picker = PICKER.get_or_init(Picker::halfblocks);
    let natural = Resize::natural_size(&image, picker.font_size());
    let available = Size::new(
        natural.width.min(width),
        natural.height.min(MAX_IMAGE_HEIGHT),
    );
    SlicedProtocol::new_with_resize(picker, image, available, Resize::Fit(None))
        .ok()
        .map(Arc::new)
}

fn local_path(destination: &str, workspace: &Path) -> Option<PathBuf> {
    let base = Url::from_directory_path(workspace).ok()?;
    let destination = base.join(destination).ok()?;
    if destination.scheme() != "file" {
        return None;
    }
    destination.to_file_path().ok()
}

#[cfg(test)]
mod tests {
    use super::{picker_supports_tmux_passthrough, protocol_hint};
    use ratatui_image::picker::ProtocolType;

    #[test]
    fn ghostty_uses_the_kitty_graphics_protocol() {
        assert_eq!(
            protocol_hint(Some("ghostty"), None, true),
            Some(ProtocolType::Kitty)
        );
    }

    #[test]
    fn ghostty_inside_tmux_uses_the_kitty_graphics_protocol() {
        assert_eq!(
            protocol_hint(Some("tmux"), Some("ghostty 1.3.1"), true),
            Some(ProtocolType::Kitty)
        );
    }

    #[test]
    fn ghostty_hint_requires_tmux_passthrough() {
        assert!(!picker_supports_tmux_passthrough(
            Some("screen-256color"),
            Some("ghostty")
        ));
        assert_eq!(
            protocol_hint(Some("ghostty"), Some("ghostty 1.3.1"), false),
            None
        );
    }
}
