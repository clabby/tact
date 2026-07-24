const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/release.yaml");
const RELEASE_INSTRUCTIONS: &str = include_str!("../RELEASES.md");

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
}
