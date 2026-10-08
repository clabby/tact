use chrono::{DateTime, Utc};
use std::{
    env,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn main() {
    println!("cargo::rerun-if-changed=../../.git/HEAD");
    println!("cargo::rerun-if-changed=../../.git/index");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo::rerun-if-env-changed=TACT_RELEASE_BUILD");
    println!("cargo::rerun-if-env-changed=TACT_PACKAGE_MANAGER");

    set(
        "TACT_GIT_SHA",
        env_override("TACT_GIT_SHA").or_else(|| git(&["rev-parse", "--short=12", "HEAD"])),
    );
    set(
        "TACT_GIT_BRANCH",
        env_override("TACT_GIT_BRANCH")
            .or_else(|| git(&["branch", "--show-current"]))
            .filter(|branch| !branch.is_empty()),
    );
    set(
        "TACT_GIT_COMMIT_TIMESTAMP",
        env_override("TACT_GIT_COMMIT_TIMESTAMP").or_else(|| git(&["log", "-1", "--format=%cI"])),
    );
    set(
        "TACT_GIT_DIRTY",
        env_override("TACT_GIT_DIRTY").or_else(dirty_state),
    );
    set("TACT_BUILD_TIMESTAMP", Some(build_timestamp()));
    set("TACT_BUILD_TARGET", env::var("TARGET").ok());
    set("TACT_BUILD_PROFILE", env::var("PROFILE").ok());
    println!(
        "cargo::rustc-env=TACT_RELEASE_CHANNEL={}",
        release_channel()
    );
    println!(
        "cargo::rustc-env=TACT_PACKAGE_MANAGER={}",
        env::var("TACT_PACKAGE_MANAGER").unwrap_or_default()
    );
    set(
        "TACT_RUSTC_VERSION",
        command_output(
            env::var("RUSTC").as_deref().unwrap_or("rustc"),
            &["--version"],
        ),
    );
}

/// What `TACT_RELEASE_BUILD` declares this build to be: `1` is an official release,
/// `pre-release` is a build of a commit on `main`, and anything else is a development build.
fn release_channel() -> &'static str {
    match env::var("TACT_RELEASE_BUILD").as_deref() {
        Ok("1") => "release",
        Ok("pre-release") => {
            let revision =
                env_override("TACT_GIT_SHA").or_else(|| git(&["rev-parse", "--short=12", "HEAD"]));
            assert!(
                revision.is_some_and(|revision| revision.len() == 12
                    && revision
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))),
                "a pre-release build needs a twelve-digit commit hash in TACT_GIT_SHA or a git checkout"
            );
            "pre-release"
        }
        _ => "development",
    }
}

/// Reads `name` from the build environment, letting builds without a git
/// checkout, such as distribution packaging, provide the repository metadata.
fn env_override(name: &str) -> Option<String> {
    println!("cargo::rerun-if-env-changed={name}");
    env::var(name).ok().filter(|value| !value.is_empty())
}

fn git(arguments: &[&str]) -> Option<String> {
    command_output("git", arguments)
}

fn command_output(program: &str, arguments: &[&str]) -> Option<String> {
    let output = Command::new(program).args(arguments).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn dirty_state() -> Option<String> {
    let output = git(&["status", "--porcelain", "--untracked-files=no"])?;
    Some(if output.is_empty() { "clean" } else { "dirty" }.to_owned())
}

fn build_timestamp() -> String {
    let unix_timestamp = env::var("SOURCE_DATE_EPOCH")
        .map(|value| {
            value
                .parse()
                .expect("SOURCE_DATE_EPOCH must be a Unix timestamp")
        })
        .unwrap_or_else(|_| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock is before the Unix epoch")
                .as_secs()
                .try_into()
                .expect("build timestamp exceeds the supported range")
        });

    DateTime::<Utc>::from_timestamp(unix_timestamp, 0)
        .expect("build timestamp exceeds the supported range")
        .to_rfc3339()
}

fn set(name: &str, value: Option<String>) {
    println!(
        "cargo::rustc-env={name}={}",
        value.as_deref().unwrap_or("unknown")
    );
}
