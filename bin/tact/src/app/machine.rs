//! `tact machine`: link this computer's web interface to others running `tact serve`.

use crate::{
    app::{
        config::Config,
        error::{CliError, MachineError, Result},
        secret::SecretString,
    },
    web::machines::{Registry, peer_client},
};
use clap::Subcommand;
use crossterm::terminal;
use std::{
    io::{self, IsTerminal, Read},
    path::Path,
};
use zeroize::Zeroizing;

/// The longest token input read. Tokens are 43 characters.
const MAX_TOKEN_INPUT: u64 = 1024;

#[derive(Debug, Subcommand)]
pub(crate) enum MachineCommand {
    /// Link a machine running `tact serve`. Its web token is read from standard input, for
    /// example `ssh devbox tact web token | tact machine add devbox https://devbox.example.ts.net`,
    /// or prompted for without echo.
    Add {
        /// Name shown in the web interface: lowercase letters, digits, and hyphens.
        name: String,
        /// The machine's https address, with no path.
        url: String,
        /// Replace an existing machine of the same name.
        #[arg(long)]
        replace: bool,
    },
    /// Unlink a machine.
    Remove { name: String },
    /// List linked machines and their addresses.
    List,
}

impl MachineCommand {
    pub(crate) async fn run(self, config: &Config) -> Result<()> {
        let registry = Registry::new(&config.path().parent().unwrap_or(Path::new(".")).join("web"));
        match self {
            Self::Add { name, url, replace } => add(&registry, &name, &url, replace).await,
            Self::Remove { name } => {
                registry.remove(&name).map_err(CliError::Machine)?;
                println!("Removed machine `{name}`.");
                Ok(())
            }
            Self::List => {
                print!("{}", listing(&registry));
                Ok(())
            }
        }
    }
}

/// One `name<TAB>address` line per machine. Tokens are never shown.
fn listing(registry: &Registry) -> String {
    registry
        .all()
        .iter()
        .map(|machine| format!("{}\t{}\n", machine.name, machine.origin))
        .collect()
}

async fn add(registry: &Registry, name: &str, url: &str, replace: bool) -> Result<()> {
    let linked = async {
        let token = read_token(name)?;
        let machine = registry.machine(name, url, &token)?;
        let client = peer_client().map_err(MachineError::Client)?;
        registry.link(&client, &machine, replace).await
    }
    .await
    .map_err(CliError::Machine)?;
    println!("Linked machine `{name}`.");
    if !linked.compatible() {
        eprintln!(
            "warning: `{name}` runs a different Tact web protocol (version {}); upgrade it, or this \
             Tact, before using it from the web interface",
            linked.protocol_version
        );
    }
    Ok(())
}

/// Reads the token from standard input, or prompts for it without echo on a terminal.
fn read_token(name: &str) -> std::result::Result<SecretString, MachineError> {
    let stdin = io::stdin();
    let mut input = Zeroizing::new(Vec::with_capacity(MAX_TOKEN_INPUT as usize));
    if stdin.is_terminal() {
        eprint!("Web token of `{name}` (run `tact web token` there): ");
        terminal::enable_raw_mode().map_err(MachineError::ReadToken)?;
        let typed = read_line(&mut stdin.lock(), &mut input);
        let restored = terminal::disable_raw_mode();
        eprintln!();
        typed.and(restored).map_err(MachineError::ReadToken)?;
    } else {
        stdin
            .lock()
            .take(MAX_TOKEN_INPUT)
            .read_to_end(&mut input)
            .map_err(MachineError::ReadToken)?;
    }
    let text = std::str::from_utf8(&input).map_err(|_| MachineError::InvalidToken)?;
    Ok(SecretString::new(text.trim().to_owned()))
}

/// Reads one line typed in raw mode, where the terminal neither echoes nor edits it.
fn read_line(input: &mut impl Read, line: &mut Vec<u8>) -> io::Result<()> {
    let mut byte = [0_u8];
    loop {
        if input.read(&mut byte)? == 0 {
            return Ok(());
        }
        match byte[0] {
            b'\r' | b'\n' => return Ok(()),
            // Ctrl-C and Ctrl-D do not raise signals in raw mode.
            0x03 | 0x04 => return Err(io::Error::from(io::ErrorKind::Interrupted)),
            0x08 | 0x7f => {
                line.pop();
            }
            typed if line.len() < line.capacity() => line.push(typed),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{listing, read_line};
    use crate::web::machines::Registry;
    use std::fs;

    #[test]
    fn listing_shows_names_and_addresses_but_never_tokens() {
        let home = tempfile::tempdir().unwrap();
        let machines = home.path().join("web/machines");
        fs::create_dir_all(&machines).unwrap();
        let token = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        for name in ["laptop", "devbox"] {
            fs::write(
                machines.join(format!("{name}.toml")),
                format!("url = \"https://{name}.example.net\"\ntoken = \"{token}\"\n"),
            )
            .unwrap();
        }

        let listed = listing(&Registry::new(&home.path().join("web")));

        assert_eq!(
            listed,
            "devbox\thttps://devbox.example.net\nlaptop\thttps://laptop.example.net\n"
        );
    }

    #[test]
    fn typed_tokens_end_at_enter_honor_backspace_and_abort_on_control_c() {
        let mut line = Vec::with_capacity(8);
        read_line(&mut &b"abx\x7fc\rignored"[..], &mut line).unwrap();
        assert_eq!(line, b"abc");

        let mut line = Vec::with_capacity(8);
        let interrupted = read_line(&mut &b"ab\x03c\r"[..], &mut line).unwrap_err();
        assert_eq!(interrupted.kind(), std::io::ErrorKind::Interrupted);
    }
}
