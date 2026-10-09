//! The commands that run this machine as a peer of another Tact, as processes.

use std::{fs, path::Path, process::Command};
use tempfile::TempDir;

/// A Tact home whose configuration serves the web interface on `bind`.
fn home(bind: &str) -> TempDir {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    fs::write(
        &path,
        format!(
            "[auth]\nmode = 'api-key'\n[openai]\napi_key = 'sk-serve-fixture'\n\
             [agent]\ntransport = 'https'\napi_base_url = 'http://127.0.0.1:9/v1'\n\
             websocket_url = 'ws://127.0.0.1:9/responses'\nweb_search = false\n\
             image_generation = false\n[skills]\nenabled = false\n[memory]\nenabled = false\n\
             [subagents]\nenabled = false\n[web]\nenabled = true\nbind = '{bind}'\nport = 0\n"
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    home
}

/// `tact` with `args`, isolated in `home`.
fn tact(home: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_tact"));
    command
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("TACT_HOME", home)
        .env("CODEX_HOME", home.join("codex"))
        .env("TACT_AUTH_FILE", home.join("unused-auth.json"))
        .env("TACT_WORKSPACE", home)
        .current_dir(home);
    command
}

#[test]
fn serve_exits_with_an_error_when_it_cannot_listen() {
    // TEST-NET-1 is never assigned to a local interface, so binding to it fails.
    let home = home("192.0.2.1");

    let output = tact(home.path(), &["serve"]).output().unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("192.0.2.1"), "{stderr}");
    let token = fs::read_to_string(home.path().join("web/token")).unwrap();
    assert!(!stderr.contains(token.trim()));
}

#[test]
fn web_token_prints_the_machine_token_and_nothing_else() {
    let home = home("127.0.0.1");

    let first = tact(home.path(), &["web", "token"]).output().unwrap();
    let second = tact(home.path(), &["web", "token"]).output().unwrap();

    assert!(first.status.success());
    let token = fs::read_to_string(home.path().join("web/token")).unwrap();
    assert_eq!(
        String::from_utf8(first.stdout.clone()).unwrap(),
        format!("{token}\n")
    );
    assert!(first.stderr.is_empty());
    assert_eq!(second.stdout, first.stdout);
}
