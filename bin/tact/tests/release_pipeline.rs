const RELEASE_WORKFLOW: &str = include_str!("../../../.github/workflows/release.yaml");
const CI_WORKFLOW: &str = include_str!("../../../.github/workflows/ci.yaml");
const RELEASE_TEMPLATE: &str = include_str!("../../../.github/RELEASE_TEMPLATE.md");
const CHANGELOG_CONFIG: &str = include_str!("../../../cliff.toml");
const RELEASE_INSTRUCTIONS: &str = include_str!("../../../RELEASES.md");
const PACKAGE_MANIFEST: &str = include_str!("../Cargo.toml");
const REVIEW_BUILD: &str = include_str!("../../../web/review/build.ts");
const REVIEW_ASSETS: &str = include_str!("../src/review/assets.rs");
const JUSTFILE: &str = include_str!("../../../justfile");

fn assert_contains(document: &str, expected: &str) {
    assert!(
        document.contains(expected),
        "expected document to contain `{expected}`"
    );
}

fn workflow(document: &str) -> serde_yaml::Value {
    serde_yaml::from_str(document).expect("workflow should be valid YAML")
}

fn number_after(document: &str, marker: &str) -> u32 {
    let value = document
        .split_once(marker)
        .unwrap_or_else(|| panic!("expected document to contain `{marker}`"))
        .1;
    let digits: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits
        .parse()
        .unwrap_or_else(|_| panic!("expected `{marker}` to be followed by a number"))
}

#[test]
fn just_recipes_forward_command_arguments() {
    for command in [
        "cargo build {{args}}",
        "cargo +nightly fmt --all -- {{args}}",
        "cargo +stable clippy --all-targets {{args}} -- -D warnings",
        "cargo nextest run {{args}}",
        "cargo bench {{args}}",
    ] {
        assert_contains(JUSTFILE, command);
    }
}

#[test]
fn harbor_context_contains_every_workspace_member() {
    assert_contains(
        JUSTFILE,
        "for source_tree in bin/tact/src crates/memory/src crates/subagents/src examples/memory-server/src; do",
    );
    assert_contains(
        JUSTFILE,
        "cp crates/subagents/Cargo.toml crates/subagents/README.md \"$build_context/crates/subagents/\"",
    );
    assert_contains(
        JUSTFILE,
        "cp -R crates/subagents/src \"$build_context/crates/subagents/src\"",
    );
    assert_contains(
        JUSTFILE,
        "cp examples/memory-server/Cargo.toml \"$build_context/examples/memory-server/\"",
    );
    assert_contains(
        JUSTFILE,
        "cp -R examples/memory-server/src \"$build_context/examples/memory-server/src\"",
    );
    assert!(
        !JUSTFILE
            .contains("cp -R examples/memory-server \"$build_context/examples/memory-server\"")
    );
}

#[test]
fn crate_package_omits_repository_only_assets() {
    let package: toml::Value = toml::from_str(PACKAGE_MANIFEST).unwrap();
    let excluded = package["package"]["exclude"].as_array().unwrap();
    let excluded = excluded
        .iter()
        .map(|entry| entry.as_str().unwrap())
        .collect::<Vec<_>>();

    assert!(excluded.contains(&"tests/release_pipeline.rs"));
}

#[test]
fn ci_tests_builds_and_typechecks_review_assets_with_locked_dependencies() {
    let workflow = workflow(CI_WORKFLOW);
    let steps = workflow["jobs"]["review-web"]["steps"]
        .as_sequence()
        .expect("review-web should contain steps");

    for command in [
        "bun install --frozen-lockfile",
        "bun test",
        "bun run build",
        "bun run typecheck",
    ] {
        assert!(
            steps.iter().any(|step| step["run"]
                .as_str()
                .is_some_and(|run| run.starts_with(command))),
            "review-web should run `{command}`"
        );
    }
}

#[test]
fn review_bundle_api_matches_the_rust_asset_validator() {
    let rust_api = number_after(REVIEW_ASSETS, "const REVIEW_API_VERSION: u32 = ");
    let bundle_min = number_after(REVIEW_BUILD, "review_api: { min: ");
    let bundle_max = number_after(REVIEW_BUILD, "max: ");

    assert!(
        (bundle_min..=bundle_max).contains(&rust_api),
        "review bundle API {bundle_min}..={bundle_max} excludes Rust API {rust_api}"
    );
}

#[test]
fn release_packages_and_signs_the_review_bundle() {
    let workflow = workflow(RELEASE_WORKFLOW);
    let review_steps = workflow["jobs"]["review_assets"]["steps"]
        .as_sequence()
        .expect("review_assets should contain steps");
    let package = review_steps
        .iter()
        .find(|step| step["name"] == "Package review assets")
        .expect("review assets should be packaged")["run"]
        .as_str()
        .expect("review packaging should be a shell command");

    assert_contains(package, "archive=\"tact-review-${GITHUB_REF_NAME}.tar.gz\"");
    assert_contains(package, "cp -R dist/. review/");
    assert_contains(package, "tar -czf \"$archive\" review");
    assert_contains(package, "shasum -a 256 \"$archive\"");

    let sign_needs = workflow["jobs"]["sign"]["needs"]
        .as_sequence()
        .expect("sign should depend on all asset builds");
    assert!(sign_needs.iter().any(|need| need == "review_assets"));

    assert_contains(RELEASE_WORKFLOW, "for archive in dist/*.tar.gz");
    assert_contains(RELEASE_WORKFLOW, "test -s \"${archive}.sig\"");
    assert_contains(RELEASE_WORKFLOW, "dist/*.tar.gz");
    assert_contains(RELEASE_WORKFLOW, "dist/*.sha256");
    assert_contains(RELEASE_WORKFLOW, "dist/*.sig");
}

#[test]
fn release_instructions_tag_the_pushed_main_revision() {
    let fetch = RELEASE_INSTRUCTIONS
        .find("jj git fetch")
        .expect("release instructions should refresh remote bookmarks");
    let pin = RELEASE_INSTRUCTIONS
        .find("release_revision=$(jj log -r main@origin")
        .expect("release instructions should pin the main commit");
    assert!(fetch < pin, "main must be fetched before it is pinned");
    assert_contains(
        RELEASE_INSTRUCTIONS,
        "jj file show -r \"$release_revision\" Cargo.toml",
    );
    assert_contains(
        RELEASE_INSTRUCTIONS,
        "jj tag set \"v${version}\" -r \"$release_revision\"",
    );
    assert_contains(RELEASE_INSTRUCTIONS, "release_commit=\"$release_revision\"");
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

    let body = config["changelog"]["body"]
        .as_str()
        .expect("git-cliff should define a changelog body");
    assert_contains(body, "commit.id | truncate(length=7, end=\"\")");
    assert_contains(
        body,
        "github.com/{{ remote.github.owner }}/{{ remote.github.repo }}/commit/{{ commit.id }}",
    );
    assert_contains(
        body,
        "github.com/{{ remote.github.owner }}/{{ remote.github.repo }}/pull/{{ commit.remote.pr_number }}",
    );

    let pull_request = body
        .find("{% if commit.remote.pr_number %}")
        .expect("the changelog should prefer pull request links");
    let fallback = body[pull_request..]
        .find("{% else %}")
        .map(|index| pull_request + index)
        .expect("the changelog should fall back when no pull request exists");
    let commit = body
        .find(
            "github.com/{{ remote.github.owner }}/{{ remote.github.repo }}/commit/{{ commit.id }}",
        )
        .expect("the changelog should link fallback commits");
    assert!(pull_request < fallback && fallback < commit);

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
    assert_contains(RELEASE_WORKFLOW, "for attempt in {1..10}");
    assert_contains(
        RELEASE_WORKFLOW,
        "if cargo package --locked --allow-dirty -p tact; then",
    );

    for expected in [
        "target/package/${package}-${version}.crate",
        "sha256sum \"$crate\"",
        "https://crates.io/api/v1/crates/${package}/${version}",
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
        .find("cargo publish --locked --no-verify -p \"$package\" \"$@\"")
        .expect("the workflow should publish the crate");
    assert!(
        checksum < publish,
        "the crate checksum must be retained before cargo attempts the upload"
    );
}

#[test]
fn library_crates_are_published_before_tact() {
    let workflow = workflow(RELEASE_WORKFLOW);
    assert_eq!(workflow["jobs"]["publish_crates"]["needs"], "sign");

    let publish = workflow["jobs"]["publish_crates"]["steps"]
        .as_sequence()
        .expect("publish_crates should contain steps")
        .iter()
        .find(|step| step["name"] == "Publish crates in dependency order")
        .expect("crates should be published together")["run"]
        .as_str()
        .expect("crate publication should be a shell command");

    for expected in [
        "publish_package()",
        "cargo package --locked -p tact-memory",
        "publish_package tact-memory",
        "cargo package --locked -p tact-subagents",
        "publish_package tact-subagents",
        "cp \"${RUNNER_TEMP}/signed-release/bin/tact/Cargo.toml\" bin/tact/Cargo.toml",
        "cargo package --locked --allow-dirty -p tact",
        "publish_package tact --allow-dirty",
    ] {
        assert_contains(publish, expected);
    }

    let memory = publish.find("publish_package tact-memory").unwrap();
    let subagents = publish.find("publish_package tact-subagents").unwrap();
    let signed_manifest = publish.find("cp \"${RUNNER_TEMP}").unwrap();
    let tact = publish.find("publish_package tact --allow-dirty").unwrap();
    assert!(memory < signed_manifest && subagents < signed_manifest && signed_manifest < tact);
}

#[test]
fn signed_release_assets_stay_outside_the_crate_package() {
    let workflow: serde_yaml::Value =
        serde_yaml::from_str(RELEASE_WORKFLOW).expect("release workflow should be valid YAML");
    let steps = workflow["jobs"]["publish_crates"]["steps"]
        .as_sequence()
        .expect("publish_crates should contain steps");
    let download = steps
        .iter()
        .find(|step| step["name"] == "Download signed release bundle")
        .expect("publish_crates should download the signed release bundle");

    assert_eq!(
        download["with"]["path"],
        "${{ runner.temp }}/signed-release"
    );

    let manifest: toml::Value =
        toml::from_str(PACKAGE_MANIFEST).expect("Cargo.toml should be valid TOML");
    let excluded = manifest["package"]["exclude"]
        .as_array()
        .expect("the package should define exclusions");
    assert!(
        excluded
            .iter()
            .any(|path| path.as_str() == Some("tests/release_pipeline.rs"))
    );
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
