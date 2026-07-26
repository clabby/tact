const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yaml");
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
fn publish_recovery_requires_the_exact_packaged_crate() {
    for expected in [
        "target/package/tact-${version}.crate",
        "sha256sum \"$crate\"",
        "https://crates.io/api/v1/crates/tact/${version}",
        "--user-agent \"tact-release-workflow/${version} (https://github.com/clabby/tact)\"",
        ".version.checksum",
        "\"$published_checksum\" != \"$local_checksum\"",
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
