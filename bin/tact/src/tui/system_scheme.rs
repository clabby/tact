//! Detection of the operating system's light or dark color scheme.

use crate::app::theme::ColorScheme;
use tokio::{sync::mpsc, time::Duration};
use tokio_util::sync::CancellationToken;

pub(crate) fn detect_system_scheme() -> Option<ColorScheme> {
    match dark_light::detect().ok()? {
        dark_light::Mode::Light => Some(ColorScheme::Light),
        dark_light::Mode::Dark => Some(ColorScheme::Dark),
        dark_light::Mode::Unspecified => None,
    }
}

const SYSTEM_SCHEME_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) fn watch_system_scheme(
    updates: mpsc::UnboundedSender<ColorScheme>,
    shutdown: CancellationToken,
) {
    tokio::spawn(async move {
        let mut last = None;
        let mut interval = tokio::time::interval(SYSTEM_SCHEME_POLL_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {
                    let detected = tokio::task::spawn_blocking(detect_system_scheme)
                        .await
                        .ok()
                        .flatten();
                    if let Some(scheme) = detected
                        && last != Some(scheme)
                    {
                        last = Some(scheme);
                        if updates.send(scheme).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::SYSTEM_SCHEME_POLL_INTERVAL;

    #[test]
    fn system_theme_polling_is_perceptually_immediate() {
        assert!(SYSTEM_SCHEME_POLL_INTERVAL <= std::time::Duration::from_millis(100));
    }
}
