//! Native clipboard access at the terminal boundary.

use arboard::Clipboard;
use base64::{Engine, engine::general_purpose::STANDARD};
use png::{BitDepth, ColorType, Encoder, EncodingError};

#[cfg(target_os = "macos")]
use std::{
    io::{self, Write},
    process::{Command, ExitStatus, Stdio},
};

#[cfg(target_os = "macos")]
use thiserror::Error;

#[cfg(not(target_os = "macos"))]
pub(crate) fn copy_text(text: &str) -> Result<(), arboard::Error> {
    Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text))
}

#[cfg(target_os = "macos")]
pub(crate) fn copy_text(text: &str) -> Result<(), CopyTextError> {
    match copy_with_pbcopy(text, "/usr/bin/pbcopy") {
        Ok(()) => Ok(()),
        Err(pbcopy) => Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text))
            .map_err(|arboard| CopyTextError { pbcopy, arboard }),
    }
}

#[cfg(target_os = "macos")]
#[derive(Debug, Error)]
#[error("pbcopy failed: {pbcopy}; native pasteboard fallback failed: {arboard}")]
pub(crate) struct CopyTextError {
    pbcopy: PbcopyError,
    arboard: arboard::Error,
}

#[cfg(target_os = "macos")]
#[derive(Debug, Error)]
enum PbcopyError {
    #[error("could not launch {program}: {source}")]
    Launch {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("could not write to {program}: {source}")]
    Write {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("could not wait for {program}: {source}")]
    Wait {
        program: String,
        #[source]
        source: io::Error,
    },
    #[error("{program} exited with {status}")]
    Exit { program: String, status: ExitStatus },
}

#[cfg(target_os = "macos")]
fn copy_with_pbcopy(text: &str, program: &str) -> Result<(), PbcopyError> {
    let mut child = Command::new(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| PbcopyError::Launch {
            program: program.to_owned(),
            source,
        })?;
    let write_result = child
        .stdin
        .take()
        .expect("piped pbcopy stdin must be available")
        .write_all(text.as_bytes())
        .map_err(|source| PbcopyError::Write {
            program: program.to_owned(),
            source,
        });
    let status = child.wait().map_err(|source| PbcopyError::Wait {
        program: program.to_owned(),
        source,
    })?;
    write_result?;
    if status.success() {
        Ok(())
    } else {
        Err(PbcopyError::Exit {
            program: program.to_owned(),
            status,
        })
    }
}

pub(crate) fn image_data_url() -> Option<String> {
    let mut clipboard = Clipboard::new().ok()?;
    let image = clipboard.get_image().ok()?;
    encode_png(image.width, image.height, image.bytes.as_ref())
        .ok()
        .map(|png| format!("data:image/png;base64,{}", STANDARD.encode(png)))
}

fn encode_png(width: usize, height: usize, pixels: &[u8]) -> Result<Vec<u8>, EncodingError> {
    let width = u32::try_from(width).map_err(|_| EncodingError::LimitsExceeded)?;
    let height = u32::try_from(height).map_err(|_| EncodingError::LimitsExceeded)?;
    let mut png = Vec::new();
    let mut encoder = Encoder::new(&mut png, width, height);
    encoder.set_color(ColorType::Rgba);
    encoder.set_depth(BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(pixels)?;
    writer.finish()?;
    Ok(png)
}

#[cfg(test)]
mod tests {
    use super::encode_png;

    #[cfg(target_os = "macos")]
    use super::copy_with_pbcopy;

    #[test]
    fn clipboard_pixels_are_encoded_as_png() {
        let encoded = encode_png(1, 1, &[255, 0, 0, 255]).unwrap();

        assert_eq!(&encoded[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn pbcopy_launch_failure_identifies_the_program() {
        let program = "/tact-test/missing-pbcopy";
        let error = copy_with_pbcopy("copy me", program).unwrap_err();

        assert!(matches!(
            error,
            super::PbcopyError::Launch {
                program: failed_program,
                ..
            } if failed_program == program
        ));
    }
}
