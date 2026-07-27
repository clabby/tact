const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yaml");
const RELEASE_TEMPLATE: &str = include_str!("../.github/RELEASE_TEMPLATE.md");
const CHANGELOG_CONFIG: &str = include_str!("../cliff.toml");
const RELEASE_INSTRUCTIONS: &str = include_str!("../RELEASES.md");
const CARGO_MANIFEST: &str = include_str!("../Cargo.toml");

fn assert_contains(document: &str, expected: &str) {
    assert!(
        document.contains(expected),
        "expected document to contain `{expected}`"
    );
}

#[test]
fn release_instructions_tag_the_pushed_main_revision() {
    assert_contains(RELEASE_INSTRUCTIONS, "release_revision=main@origin");
    assert_contains(
        RELEASE_INSTRUCTIONS,
        "jj tag set \"v${version}\" -r \"$release_revision\"",
    );
    assert_contains(
        RELEASE_INSTRUCTIONS,
        "jj log -r \"$release_revision\" --no-graph -T 'commit_id'",
    );
}

#[test]
fn release_tag_must_be_on_main() {
    assert_contains(RELEASE_WORKFLOW, "fetch-depth: 0");
    assert_contains(
        RELEASE_WORKFLOW,
        "git merge-base --is-ancestor \"$GITHUB_SHA\" origin/main",
    );
}

#[test]
fn generated_changelog_uses_the_template_and_tagged_history() {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(RELEASE_WORKFLOW).expect("release workflow should be valid YAML");
    let steps = workflow["jobs"]["release"]["steps"]
        .as_sequence()
        .expect("release should contain steps");

    let checkout = steps
        .iter()
        .find(|step| step["name"] == "Checkout sources")
        .expect("release should check out its sources");
    assert_eq!(checkout["with"]["fetch-depth"], 0);

    let install = steps
        .iter()
        .find(|step| step["name"] == "Install git-cliff")
        .expect("release should install git-cliff");
    assert_eq!(install["with"]["tool"], "git-cliff@2.13.1");
    assert_eq!(install["with"]["fallback"], "none");

    let generate = steps
        .iter()
        .find(|step| step["name"] == "Generate release notes")
        .expect("release should generate release notes");
    let command = generate["run"]
        .as_str()
        .expect("release note generation should be a shell command");
    assert_contains(command, "cp .github/RELEASE_TEMPLATE.md release-notes.md");
    assert_contains(command, "git cliff --current --strip all");
    assert_eq!(generate["env"]["GITHUB_TOKEN"], "${{ github.token }}");

    let publish = steps
        .iter()
        .find(|step| step["name"] == "Publish release")
        .expect("release should publish its generated notes");
    assert_eq!(publish["with"]["body_path"], "release-notes.md");
    assert!(publish["with"]["generate_release_notes"].is_null());

    assert_contains(RELEASE_TEMPLATE, "https://tact.clab.by/install.sh");
    assert_contains(RELEASE_TEMPLATE, "tact update");
    assert_contains(RELEASE_INSTRUCTIONS, "`git-cliff`");
}

#[test]
fn changelog_includes_only_standard_conventional_commit_types() {
    let config: toml::Value =
        toml::from_str(CHANGELOG_CONFIG).expect("cliff.toml should be valid TOML");
    let git = &config["git"];

    assert_eq!(config["remote"]["github"]["owner"].as_str(), Some("clabby"));
    assert_eq!(config["remote"]["github"]["repo"].as_str(), Some("tact"));
    assert_eq!(git["conventional_commits"].as_bool(), Some(true));
    assert_eq!(git["filter_unconventional"].as_bool(), Some(true));
    assert_eq!(git["filter_commits"].as_bool(), Some(true));
    assert_eq!(git["tag_pattern"].as_str(), Some("v[0-9]*"));

    let parsers = git["commit_parsers"]
        .as_array()
        .expect("git-cliff should define changelog groups");
    let patterns: Vec<_> = parsers
        .iter()
        .map(|parser| {
            parser["message"]
                .as_str()
                .expect("each git-cliff parser should match a commit type")
        })
        .collect();
    assert_eq!(
        patterns,
        [
            "^feat",
            "^fix",
            "^perf",
            "^docs",
            "^refactor",
            "^style",
            "^test",
            "^build|^chore|^ci",
            "^revert",
        ]
    );
}

#[test]
fn publish_recovery_requires_the_exact_packaged_crate() {
    assert_contains(
        RELEASE_WORKFLOW,
        "run: cargo package --locked --allow-dirty",
    );

    for expected in [
        "target/package/tact-${version}.crate",
        "sha256sum \"$crate\"",
        "https://crates.io/api/v1/crates/tact/${version}",
        "--user-agent \"tact-release-workflow/${version} (https://github.com/clabby/tact)\"",
        ".version.checksum",
        "\"$published_checksum\" != \"$local_checksum\"",
        "for attempt in {1..5}",
        "sleep $((attempt * 15))",
    ] {
        assert_contains(RELEASE_WORKFLOW, expected);
    }

    let checksum = RELEASE_WORKFLOW
        .find("local_checksum=$(sha256sum \"$crate\"")
        .expect("publish recovery should checksum the packaged crate");
    let publish = RELEASE_WORKFLOW
        .find("cargo publish --locked --allow-dirty")
        .expect("the workflow should publish the crate");
    assert!(
        checksum < publish,
        "the crate checksum must be retained before cargo attempts the upload"
    );
}

#[test]
fn signed_release_assets_stay_outside_the_crate_package() {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(RELEASE_WORKFLOW).expect("release workflow should be valid YAML");
    let steps = workflow["jobs"]["publish_crate"]["steps"]
        .as_sequence()
        .expect("publish_crate should contain steps");
    let download = steps
        .iter()
        .find(|step| step["name"] == "Download signed release bundle")
        .expect("publish_crate should download the signed release bundle");

    assert_eq!(
        download["with"]["path"],
        "${{ runner.temp }}/signed-release"
    );

    let manifest: toml::Value =
        toml::from_str(CARGO_MANIFEST).expect("Cargo.toml should be valid TOML");
    let excluded = manifest["package"]["exclude"]
        .as_array()
        .expect("the package should define exclusions");
    assert!(excluded.iter().any(|path| path.as_str() == Some("dist/**")));
}

#[test]
fn container_build_uses_the_verified_local_binary() {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(RELEASE_WORKFLOW).expect("release workflow should be valid YAML");
    let steps = workflow["jobs"]["container_build"]["steps"]
        .as_sequence()
        .expect("container_build should contain steps");
    let bake = steps
        .iter()
        .find(|step| step["name"] == "Package and push image by digest")
        .expect("container_build should package the image with Docker Bake");

    assert_eq!(bake["with"]["source"], ".");
    assert_eq!(
        bake["env"]["RELEASE_BINARY_CONTEXT"],
        "target/image-context"
    );
}
