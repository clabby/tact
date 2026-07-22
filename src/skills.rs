//! Bounded discovery and model-facing metadata for local filesystem skills.

use crate::config::SkillsConfig;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    fs::{self, File},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
};
use thiserror::Error;

const MAX_DIRECTORIES: usize = 2_048;
const MAX_ENTRIES_PER_DIRECTORY: usize = 4_096;
const MAX_SKILL_FILES: usize = 4_096;
#[cfg(not(test))]
const MAX_VISITED_ENTRIES: usize = 20_000;
#[cfg(test)]
const MAX_VISITED_ENTRIES: usize = 256;
const MAX_DIAGNOSTICS: usize = 128;
const MAX_DEPTH: usize = 8;
const MAX_FRONTMATTER_BYTES: usize = 16 * 1_024;
const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1_024;
const MAX_RENDERED_BYTES: usize = 8 * 1_024;

pub(crate) const CATALOG_START_MARKER: &str = "<!-- tact:skills-catalog:start -->";
pub(crate) const CATALOG_END_MARKER: &str = "<!-- tact:skills-catalog:end -->";

const CATALOG_PREAMBLE: &str = "## Available local skills\n";
const CATALOG_RULES: &str = "\
Use a skill when the user names it or the task clearly matches its description. Before acting, \
read the selected `SKILL.md` completely. Resolve referenced resources relative to the directory \
containing that `SKILL.md`.\n\n";

#[derive(Debug, Error)]
pub(crate) enum SkillDiagnostic {
    #[error("could not inspect skill path {path}: {source}")]
    Inspect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("skill traversal below {root} exceeded the depth limit of {limit}")]
    DepthLimit { root: PathBuf, limit: usize },
    #[error("skill traversal reached the directory limit of {limit}")]
    DirectoryLimit { limit: usize },
    #[error("skill directory {path} exceeds the entry limit of {limit}")]
    EntryLimit { path: PathBuf, limit: usize },
    #[error("skill traversal reached the skill file limit of {limit}")]
    SkillLimit { limit: usize },
    #[error("skill traversal reached the visited entry limit of {limit}")]
    EntryBudget { limit: usize },
    #[error("skill root {path} is not a directory")]
    RootNotDirectory { path: PathBuf },
    #[error("skill path {path} resolves outside root {root} to {target}")]
    OutsideRoot {
        root: PathBuf,
        path: PathBuf,
        target: PathBuf,
    },
    #[error("skill path {path} resolves to a non-regular file {target}")]
    NonRegularFile { path: PathBuf, target: PathBuf },
    #[error("invalid skill metadata in {path}: {message}")]
    Metadata { path: PathBuf, message: String },
    #[error("duplicate skill name `{name}` in {ignored}; keeping {kept}")]
    Duplicate {
        name: String,
        kept: PathBuf,
        ignored: PathBuf,
    },
    #[error("skill metadata budget omitted {omitted} catalog entries")]
    MetadataBudget { omitted: usize },
    #[error("skill diagnostics exceeded the retained limit of {limit}")]
    DiagnosticsTruncated { limit: usize },
}

struct DiagnosticCollector {
    diagnostics: Vec<SkillDiagnostic>,
    truncated: bool,
}

#[derive(Debug)]
pub(crate) struct SkillCatalog {
    #[cfg(test)]
    skills: Vec<SkillMetadata>,
    #[cfg(test)]
    diagnostics: Vec<SkillDiagnostic>,
    rendered: Option<String>,
}

#[derive(Debug)]
struct SkillMetadata {
    name: String,
    description: String,
    path: PathBuf,
}

#[derive(Deserialize)]
struct Frontmatter {
    name: Option<String>,
    description: Option<String>,
}

impl SkillCatalog {
    pub(crate) fn load(config: &SkillsConfig) -> Self {
        if !config.enabled() {
            return Self {
                #[cfg(test)]
                skills: Vec::new(),
                #[cfg(test)]
                diagnostics: Vec::new(),
                rendered: None,
            };
        }

        let mut diagnostics = DiagnosticCollector::new();
        let paths = discover(config.roots(), &mut diagnostics);
        let mut by_name: BTreeMap<String, SkillMetadata> = BTreeMap::new();

        for path in paths {
            let skill = match parse_metadata(&path) {
                Ok(skill) => skill,
                Err(diagnostic) => {
                    diagnostics.push(diagnostic);
                    continue;
                }
            };
            if let Some(existing) = by_name.get(&skill.name) {
                diagnostics.push(SkillDiagnostic::Duplicate {
                    name: skill.name,
                    kept: existing.path.clone(),
                    ignored: skill.path,
                });
            } else {
                by_name.insert(skill.name.clone(), skill);
            }
        }

        let skills: Vec<_> = by_name.into_values().collect();
        let (rendered, omitted) = render(&skills);
        if omitted > 0 {
            diagnostics.push(SkillDiagnostic::MetadataBudget { omitted });
        }
        let diagnostics = diagnostics.finish();
        #[cfg(not(test))]
        drop(diagnostics);

        Self {
            rendered: (!skills.is_empty()).then_some(rendered),
            #[cfg(test)]
            skills,
            #[cfg(test)]
            diagnostics,
        }
    }

    /// Instructions safe to add to model context; skill bodies are never included here.
    pub(crate) fn rendered_instructions(&self) -> Option<&str> {
        self.rendered.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn diagnostics(&self) -> &[SkillDiagnostic] {
        &self.diagnostics
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.skills.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
}

impl DiagnosticCollector {
    fn new() -> Self {
        Self {
            diagnostics: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, diagnostic: SkillDiagnostic) {
        if self.diagnostics.len() < MAX_DIAGNOSTICS - 1 {
            self.diagnostics.push(diagnostic);
        } else {
            self.truncated = true;
        }
    }

    fn finish(mut self) -> Vec<SkillDiagnostic> {
        if self.truncated {
            self.diagnostics
                .push(SkillDiagnostic::DiagnosticsTruncated {
                    limit: MAX_DIAGNOSTICS,
                });
        }
        self.diagnostics
    }
}

pub(crate) fn contains_catalog(instructions: &str) -> bool {
    instructions
        .split_once(CATALOG_START_MARKER)
        .is_some_and(|(_, rest)| rest.contains(CATALOG_END_MARKER))
}

fn discover(roots: &[PathBuf], diagnostics: &mut DiagnosticCollector) -> BTreeSet<PathBuf> {
    let mut skill_paths = BTreeSet::new();
    let mut visited_directories = BTreeSet::new();
    let mut directory_count = 0;
    let mut visited_entries = 0;

    for root in roots {
        let canonical_root = match fs::canonicalize(root) {
            Ok(path) => path,
            Err(source) if source.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                diagnostics.push(SkillDiagnostic::Inspect {
                    path: root.clone(),
                    source,
                });
                continue;
            }
        };
        match fs::metadata(&canonical_root) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                diagnostics.push(SkillDiagnostic::RootNotDirectory {
                    path: canonical_root,
                });
                continue;
            }
            Err(source) => {
                diagnostics.push(SkillDiagnostic::Inspect {
                    path: canonical_root,
                    source,
                });
                continue;
            }
        }

        let mut queue = VecDeque::from([(canonical_root.clone(), 0)]);
        let mut reported_depth_limit = false;

        while let Some((directory, depth)) = queue.pop_front() {
            let canonical = match fs::canonicalize(&directory) {
                Ok(path) => path,
                Err(source) => {
                    diagnostics.push(SkillDiagnostic::Inspect {
                        path: directory,
                        source,
                    });
                    continue;
                }
            };
            if !canonical.starts_with(&canonical_root) {
                diagnostics.push(SkillDiagnostic::OutsideRoot {
                    root: canonical_root.clone(),
                    path: directory,
                    target: canonical,
                });
                continue;
            }
            if !visited_directories.insert(canonical.clone()) {
                continue;
            }
            directory_count += 1;
            if directory_count > MAX_DIRECTORIES {
                diagnostics.push(SkillDiagnostic::DirectoryLimit {
                    limit: MAX_DIRECTORIES,
                });
                return skill_paths;
            }

            let entries = match sorted_entries(
                &canonical,
                MAX_VISITED_ENTRIES.saturating_sub(visited_entries),
            ) {
                Ok(entries) => entries,
                Err(DirectoryEntriesError::Read(source)) => {
                    diagnostics.push(SkillDiagnostic::Inspect {
                        path: canonical,
                        source,
                    });
                    continue;
                }
                Err(DirectoryEntriesError::Limit { inspected }) => {
                    visited_entries += inspected;
                    diagnostics.push(SkillDiagnostic::EntryLimit {
                        path: canonical,
                        limit: MAX_ENTRIES_PER_DIRECTORY,
                    });
                    continue;
                }
                Err(DirectoryEntriesError::GlobalLimit) => {
                    diagnostics.push(SkillDiagnostic::EntryBudget {
                        limit: MAX_VISITED_ENTRIES,
                    });
                    return skill_paths;
                }
            };
            visited_entries += entries.len();
            for entry in entries {
                let path = entry.path();
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(source) => {
                        diagnostics.push(SkillDiagnostic::Inspect { path, source });
                        continue;
                    }
                };
                if file_type.is_symlink() {
                    let target = match fs::canonicalize(&path) {
                        Ok(target) => target,
                        Err(source) => {
                            diagnostics.push(SkillDiagnostic::Inspect { path, source });
                            continue;
                        }
                    };
                    if !target.starts_with(&canonical_root) {
                        diagnostics.push(SkillDiagnostic::OutsideRoot {
                            root: canonical_root.clone(),
                            path,
                            target,
                        });
                        continue;
                    }
                    let metadata = match fs::metadata(&target) {
                        Ok(metadata) => metadata,
                        Err(source) => {
                            diagnostics.push(SkillDiagnostic::Inspect { path, source });
                            continue;
                        }
                    };
                    if metadata.is_dir() {
                        enqueue_directory(
                            &mut queue,
                            &mut reported_depth_limit,
                            diagnostics,
                            root,
                            target,
                            depth,
                        );
                    } else if entry.file_name() == "SKILL.md"
                        && add_skill_path(&mut skill_paths, diagnostics, path, target, &metadata)
                    {
                        return skill_paths;
                    }
                } else if file_type.is_dir() {
                    enqueue_directory(
                        &mut queue,
                        &mut reported_depth_limit,
                        diagnostics,
                        root,
                        path,
                        depth,
                    );
                } else if entry.file_name() == "SKILL.md" {
                    let target = match fs::canonicalize(&path) {
                        Ok(target) => target,
                        Err(source) => {
                            diagnostics.push(SkillDiagnostic::Inspect { path, source });
                            continue;
                        }
                    };
                    let metadata = match fs::metadata(&target) {
                        Ok(metadata) => metadata,
                        Err(source) => {
                            diagnostics.push(SkillDiagnostic::Inspect { path, source });
                            continue;
                        }
                    };
                    if add_skill_path(&mut skill_paths, diagnostics, path, target, &metadata) {
                        return skill_paths;
                    }
                }
            }
        }
    }

    skill_paths
}

fn enqueue_directory(
    queue: &mut VecDeque<(PathBuf, usize)>,
    reported_depth_limit: &mut bool,
    diagnostics: &mut DiagnosticCollector,
    root: &Path,
    path: PathBuf,
    depth: usize,
) {
    if depth < MAX_DEPTH {
        queue.push_back((path, depth + 1));
    } else if !*reported_depth_limit {
        diagnostics.push(SkillDiagnostic::DepthLimit {
            root: root.to_path_buf(),
            limit: MAX_DEPTH,
        });
        *reported_depth_limit = true;
    }
}

fn add_skill_path(
    skill_paths: &mut BTreeSet<PathBuf>,
    diagnostics: &mut DiagnosticCollector,
    path: PathBuf,
    target: PathBuf,
    metadata: &fs::Metadata,
) -> bool {
    if !metadata.is_file() {
        diagnostics.push(SkillDiagnostic::NonRegularFile { path, target });
        return false;
    }
    if !skill_paths.contains(&target) && skill_paths.len() == MAX_SKILL_FILES {
        diagnostics.push(SkillDiagnostic::SkillLimit {
            limit: MAX_SKILL_FILES,
        });
        return true;
    }
    skill_paths.insert(target);
    false
}

enum DirectoryEntriesError {
    Read(io::Error),
    Limit { inspected: usize },
    GlobalLimit,
}

fn sorted_entries(
    directory: &Path,
    remaining_entries: usize,
) -> Result<Vec<fs::DirEntry>, DirectoryEntriesError> {
    let limit = MAX_ENTRIES_PER_DIRECTORY.min(remaining_entries);
    let mut entries = fs::read_dir(directory)
        .map_err(DirectoryEntriesError::Read)?
        .take(limit + 1)
        .collect::<io::Result<Vec<_>>>()
        .map_err(DirectoryEntriesError::Read)?;
    if entries.len() > remaining_entries {
        return Err(DirectoryEntriesError::GlobalLimit);
    }
    if entries.len() > MAX_ENTRIES_PER_DIRECTORY {
        return Err(DirectoryEntriesError::Limit {
            inspected: entries.len(),
        });
    }
    entries.sort_by_key(fs::DirEntry::file_name);
    Ok(entries)
}

fn parse_metadata(path: &Path) -> Result<SkillMetadata, SkillDiagnostic> {
    let metadata = fs::metadata(path).map_err(|source| SkillDiagnostic::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(SkillDiagnostic::NonRegularFile {
            path: path.to_path_buf(),
            target: path.to_path_buf(),
        });
    }
    let file = File::open(path).map_err(|source| SkillDiagnostic::Inspect {
        path: path.to_path_buf(),
        source,
    })?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    read_frontmatter_line(&mut reader, &mut line, path, 0)?;
    if line.trim_end_matches(['\r', '\n']) != "---" {
        return Err(metadata_error(
            path,
            "missing opening YAML frontmatter delimiter",
        ));
    }

    let mut yaml = String::new();
    loop {
        line.clear();
        let bytes = read_frontmatter_line(&mut reader, &mut line, path, yaml.len())?;
        if bytes == 0 {
            return Err(metadata_error(
                path,
                "missing closing YAML frontmatter delimiter",
            ));
        }
        if line.trim_end_matches(['\r', '\n']) == "---" {
            break;
        }
        yaml.push_str(&line);
    }

    let frontmatter: Frontmatter = serde_yaml::from_str(&yaml)
        .map_err(|error| metadata_error(path, format!("invalid YAML: {error}")))?;
    let name = validate_field(path, "name", frontmatter.name, MAX_NAME_CHARS)?;
    if !valid_skill_name(&name) {
        return Err(metadata_error(
            path,
            "`name` must contain only lowercase ASCII letters, digits, and single interior hyphens",
        ));
    }
    let directory_name = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str());
    if directory_name != Some(name.as_str()) {
        return Err(metadata_error(
            path,
            "`name` must match the directory containing SKILL.md",
        ));
    }
    let description = validate_field(
        path,
        "description",
        frontmatter.description,
        MAX_DESCRIPTION_CHARS,
    )?;

    Ok(SkillMetadata {
        name,
        description: description.split_whitespace().collect::<Vec<_>>().join(" "),
        path: path.to_path_buf(),
    })
}

fn valid_skill_name(name: &str) -> bool {
    !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn read_frontmatter_line(
    reader: &mut impl BufRead,
    line: &mut String,
    path: &Path,
    bytes_read: usize,
) -> Result<usize, SkillDiagnostic> {
    let remaining = MAX_FRONTMATTER_BYTES.saturating_sub(bytes_read);
    let bytes = reader
        .take((remaining + 1) as u64)
        .read_line(line)
        .map_err(|source| SkillDiagnostic::Inspect {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes > remaining {
        return Err(metadata_error(
            path,
            format!("frontmatter exceeds {MAX_FRONTMATTER_BYTES} bytes"),
        ));
    }
    Ok(bytes)
}

fn validate_field(
    path: &Path,
    field: &str,
    value: Option<String>,
    max_chars: usize,
) -> Result<String, SkillDiagnostic> {
    let value = value.ok_or_else(|| metadata_error(path, format!("missing `{field}`")))?;
    let value = value.trim();
    if value.is_empty() {
        return Err(metadata_error(path, format!("`{field}` must not be empty")));
    }
    if value.chars().count() > max_chars {
        return Err(metadata_error(
            path,
            format!("`{field}` exceeds {max_chars} characters"),
        ));
    }
    Ok(value.to_owned())
}

fn metadata_error(path: &Path, message: impl Into<String>) -> SkillDiagnostic {
    SkillDiagnostic::Metadata {
        path: path.to_path_buf(),
        message: message.into(),
    }
}

fn render(skills: &[SkillMetadata]) -> (String, usize) {
    let mut rendered = format!("{CATALOG_PREAMBLE}{CATALOG_START_MARKER}\n{CATALOG_RULES}");
    let epilogue = format!("{CATALOG_END_MARKER}\n");
    let mut included = 0;
    for skill in skills {
        let entry = format!(
            "- name: {}\n  description: {}\n  path: {}\n",
            json_string(&skill.name),
            json_string(&skill.description),
            json_string(&skill.path.to_string_lossy()),
        );
        if rendered.len() + entry.len() + epilogue.len() > MAX_RENDERED_BYTES {
            continue;
        }
        rendered.push_str(&entry);
        included += 1;
    }
    rendered.push_str(&epilogue);
    (rendered, skills.len() - included)
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

#[cfg(test)]
mod tests {
    use super::{
        CATALOG_END_MARKER, CATALOG_START_MARKER, DiagnosticCollector, MAX_DIAGNOSTICS,
        MAX_RENDERED_BYTES, SkillCatalog, SkillDiagnostic, contains_catalog,
    };
    use crate::config::SkillsConfig;
    use std::{fs, path::Path};
    use tempfile::tempdir;

    #[test]
    fn disabled_catalog_does_not_scan_configured_roots() {
        let directory = tempdir().unwrap();
        fs::write(directory.path().join("SKILL.md"), "not frontmatter").unwrap();
        let config = SkillsConfig::from_roots(false, vec![directory.path().to_path_buf()]);

        let catalog = SkillCatalog::load(&config);

        assert!(catalog.is_empty());
        assert!(catalog.rendered_instructions().is_none());
        assert!(catalog.diagnostics().is_empty());
    }

    #[test]
    fn parses_only_frontmatter_metadata_and_renders_canonical_path() {
        let directory = tempdir().unwrap();
        let skill_path = write_skill(
            directory.path(),
            "code-review",
            "---\nname: code-review\ndescription: >-\n  Review a change for correctness.\n---\nBODY-SENTINEL\n",
        );
        let config = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let catalog = SkillCatalog::load(&config);
        let rendered = catalog.rendered_instructions().unwrap();

        assert_eq!(catalog.len(), 1);
        assert!(catalog.diagnostics().is_empty());
        assert!(rendered.contains("code-review"));
        assert!(rendered.contains("Review a change for correctness."));
        assert!(rendered.contains(&fs::canonicalize(skill_path).unwrap().display().to_string()));
        assert!(!rendered.contains("BODY-SENTINEL"));
        assert!(rendered.contains("read the selected `SKILL.md` completely"));
        assert!(rendered.contains("relative to the directory containing"));
        assert!(contains_catalog(rendered));
        assert!(rendered.contains(CATALOG_START_MARKER));
        assert!(rendered.ends_with(&format!("{CATALOG_END_MARKER}\n")));
        assert!(!contains_catalog("an ordinary stored system prompt"));
    }

    #[test]
    fn malformed_and_duplicate_skills_produce_deterministic_diagnostics() {
        let directory = tempdir().unwrap();
        let first_root = directory.path().join("a-root");
        let second_root = directory.path().join("b-root");
        let first = write_skill(
            &first_root,
            "duplicate",
            "---\nname: duplicate\ndescription: First.\n---\n",
        );
        let second = write_skill(
            &second_root,
            "duplicate",
            "---\nname: duplicate\ndescription: Second.\n---\n",
        );
        write_skill(&first_root, "malformed", "no frontmatter\n");
        let config = SkillsConfig::from_roots(true, vec![first_root, second_root]);

        let catalog = SkillCatalog::load(&config);

        assert_eq!(catalog.len(), 1);
        assert!(catalog.rendered_instructions().unwrap().contains("First."));
        assert_eq!(catalog.diagnostics().len(), 2);
        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| matches!(diagnostic, SkillDiagnostic::Metadata { .. }))
        );
        assert!(catalog.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            SkillDiagnostic::Duplicate { kept, ignored, .. }
                if kept == &fs::canonicalize(&first).unwrap()
                    && ignored == &fs::canonicalize(&second).unwrap()
        )));
    }

    #[test]
    fn validates_portable_names_directory_match_and_description_length() {
        let directory = tempdir().unwrap();
        for invalid in [
            "Upper",
            "leading-",
            "-trailing",
            "two--hyphens",
            "under_score",
        ] {
            write_skill(
                directory.path(),
                invalid,
                &format!("---\nname: {invalid}\ndescription: Invalid.\n---\n"),
            );
        }
        let long_name = "a".repeat(65);
        write_skill(
            directory.path(),
            &long_name,
            &format!("---\nname: {long_name}\ndescription: Invalid.\n---\n"),
        );
        write_skill(
            directory.path(),
            "wrong-directory",
            "---\nname: different-name\ndescription: Invalid.\n---\n",
        );
        let description = "x".repeat(1_024);
        write_skill(
            directory.path(),
            "valid-name-64",
            &format!("---\nname: valid-name-64\ndescription: {description}\n---\n"),
        );
        let config = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let catalog = SkillCatalog::load(&config);

        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.diagnostics().len(), 7);
        assert!(
            catalog
                .diagnostics()
                .iter()
                .all(|diagnostic| matches!(diagnostic, SkillDiagnostic::Metadata { .. }))
        );
    }

    #[test]
    fn rendering_has_a_strict_total_metadata_budget() {
        let directory = tempdir().unwrap();
        for index in 0..100 {
            write_skill(
                directory.path(),
                &format!("skill-{index:03}"),
                &format!(
                    "---\nname: skill-{index:03}\ndescription: {}\n---\n",
                    "description ".repeat(35)
                ),
            );
        }
        let config = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let catalog = SkillCatalog::load(&config);
        let rendered = catalog.rendered_instructions().unwrap();

        assert!(rendered.len() <= MAX_RENDERED_BYTES);
        assert!(catalog.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            SkillDiagnostic::MetadataBudget { omitted } if *omitted > 0
        )));
    }

    #[test]
    fn global_entry_budget_stops_large_trees() {
        let directory = tempdir().unwrap();
        for group in 0..3 {
            let group = directory.path().join(format!("group-{group}"));
            fs::create_dir(&group).unwrap();
            for entry in 0..100 {
                fs::write(group.join(format!("entry-{entry:03}")), "ignored").unwrap();
            }
        }
        let config = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let catalog = SkillCatalog::load(&config);

        assert!(
            catalog
                .diagnostics()
                .iter()
                .any(|diagnostic| matches!(diagnostic, SkillDiagnostic::EntryBudget { .. }))
        );
    }

    #[test]
    fn diagnostic_retention_is_capped_with_one_truncation_notice() {
        let mut diagnostics = DiagnosticCollector::new();
        for index in 0..MAX_DIAGNOSTICS * 2 {
            diagnostics.push(SkillDiagnostic::Metadata {
                path: format!("skill-{index}").into(),
                message: "invalid".to_owned(),
            });
        }

        let diagnostics = diagnostics.finish();

        assert_eq!(diagnostics.len(), MAX_DIAGNOSTICS);
        assert!(matches!(
            diagnostics.last(),
            Some(SkillDiagnostic::DiagnosticsTruncated { .. })
        ));
        assert_eq!(
            diagnostics
                .iter()
                .filter(|diagnostic| matches!(
                    diagnostic,
                    SkillDiagnostic::DiagnosticsTruncated { .. }
                ))
                .count(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn follows_directory_symlinks_once_without_cycles() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let real = directory.path().join("real");
        write_skill(
            &real,
            "linked",
            "---\nname: linked\ndescription: Linked skill.\n---\n",
        );
        symlink(&real, directory.path().join("alias")).unwrap();
        symlink(directory.path(), real.join("cycle")).unwrap();
        let config = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);

        let catalog = SkillCatalog::load(&config);

        assert_eq!(catalog.len(), 1);
        assert!(catalog.diagnostics().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn configured_root_may_itself_be_a_symlink() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let real_root = directory.path().join("real-root");
        write_skill(
            &real_root,
            "root-skill",
            "---\nname: root-skill\ndescription: Root symlink skill.\n---\n",
        );
        let linked_root = directory.path().join("linked-root");
        symlink(&real_root, &linked_root).unwrap();
        let config = SkillsConfig::from_roots(true, vec![linked_root]);

        let catalog = SkillCatalog::load(&config);

        assert_eq!(catalog.len(), 1);
        assert!(catalog.diagnostics().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rejects_fifo_targets_without_opening_them() {
        use std::{os::unix::fs::symlink, process::Command, sync::mpsc, thread, time::Duration};

        let directory = tempdir().unwrap();
        let fifo_directory = directory.path().join("pipe");
        let linked_directory = directory.path().join("linked-pipe");
        fs::create_dir(&fifo_directory).unwrap();
        fs::create_dir(&linked_directory).unwrap();
        let fifo = fifo_directory.join("SKILL.md");
        assert!(
            Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        symlink(&fifo, linked_directory.join("SKILL.md")).unwrap();
        let config = SkillsConfig::from_roots(true, vec![directory.path().to_path_buf()]);
        let (sender, receiver) = mpsc::channel();

        thread::spawn(move || {
            let catalog = SkillCatalog::load(&config);
            sender
                .send((
                    catalog.len(),
                    catalog.diagnostics().iter().any(|diagnostic| {
                        matches!(diagnostic, SkillDiagnostic::NonRegularFile { .. })
                    }),
                ))
                .unwrap();
        });

        let (count, diagnosed) = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("FIFO discovery must return without opening the FIFO");
        assert_eq!(count, 0);
        assert!(diagnosed);
    }

    #[cfg(unix)]
    #[test]
    fn descendant_symlinks_cannot_escape_their_canonical_root() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let outside_skill = write_skill(
            outside.path(),
            "external",
            "---\nname: external\ndescription: Must stay outside.\n---\n",
        );
        symlink(
            outside.path().join("external"),
            root.path().join("directory-link"),
        )
        .unwrap();
        let local_directory = root.path().join("file-link");
        fs::create_dir(&local_directory).unwrap();
        symlink(&outside_skill, local_directory.join("SKILL.md")).unwrap();
        let config = SkillsConfig::from_roots(true, vec![root.path().to_path_buf()]);

        let catalog = SkillCatalog::load(&config);

        assert!(catalog.is_empty());
        assert_eq!(
            catalog
                .diagnostics()
                .iter()
                .filter(|diagnostic| matches!(diagnostic, SkillDiagnostic::OutsideRoot { .. }))
                .count(),
            2
        );
    }

    fn write_skill(root: &Path, directory: &str, contents: &str) -> std::path::PathBuf {
        let skill_directory = root.join(directory);
        fs::create_dir_all(&skill_directory).unwrap();
        let path = skill_directory.join("SKILL.md");
        fs::write(&path, contents).unwrap();
        path
    }
}
