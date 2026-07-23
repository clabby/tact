use std::{
    env,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn main() {
    println!("cargo::rerun-if-changed=.git/HEAD");
    println!("cargo::rerun-if-changed=.git/index");
    println!("cargo::rerun-if-env-changed=SOURCE_DATE_EPOCH");
    println!("cargo::rerun-if-env-changed=TACT_RELEASE_BUILD");

    set("TACT_GIT_SHA", git(&["rev-parse", "--short=12", "HEAD"]));
    set(
        "TACT_GIT_BRANCH",
        git(&["branch", "--show-current"]).filter(|branch| !branch.is_empty()),
    );
    set(
        "TACT_GIT_COMMIT_TIMESTAMP",
        git(&["log", "-1", "--format=%cI"]),
    );
    set("TACT_GIT_DIRTY", dirty_state());
    set("TACT_BUILD_TIMESTAMP", Some(build_timestamp()));
    set("TACT_BUILD_TARGET", env::var("TARGET").ok());
    set("TACT_BUILD_PROFILE", env::var("PROFILE").ok());
    println!(
        "cargo::rustc-env=TACT_RELEASE_BUILD={}",
        env::var("TACT_RELEASE_BUILD").is_ok_and(|value| value == "1")
    );
    set(
        "TACT_RUSTC_VERSION",
        command_output(
            env::var("RUSTC").as_deref().unwrap_or("rustc"),
            &["--version"],
        ),
    );
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
    env::var("SOURCE_DATE_EPOCH").unwrap_or_else(|_| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock is before the Unix epoch")
            .as_secs()
            .to_string()
    })
}

fn set(name: &str, value: Option<String>) {
    println!(
        "cargo::rustc-env={name}={}",
        value.as_deref().unwrap_or("unknown")
    );
}
