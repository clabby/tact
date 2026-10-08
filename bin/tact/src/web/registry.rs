//! The per-user registry of running Tact instances.
//!
//! Each instance writes `<pid>.json` into the registry directory while it serves and removes it on
//! exit. Files outlive crashed processes, so readers treat the registry as a list of candidates:
//! an instance exists only if it answers.

use serde::{Deserialize, Serialize};
use std::{
    fs, io,
    path::{Path, PathBuf},
};
use thiserror::Error;

#[derive(Debug, Error)]
#[error("failed to write the instance registry file {path}: {source}")]
pub(crate) struct RegistryError {
    path: PathBuf,
    #[source]
    source: io::Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct InstanceRecord {
    pub(crate) pid: u32,
    pub(crate) port: u16,
    pub(crate) workspace: PathBuf,
    pub(crate) started_at: u64,
}

/// The registry file of this process; dropping it unregisters the instance.
pub(crate) struct Registration {
    path: PathBuf,
}

impl Registration {
    /// Publishes `record` atomically so readers never see a partial file.
    pub(crate) fn create(directory: &Path, record: &InstanceRecord) -> Result<Self, RegistryError> {
        let path = directory.join(format!("{}.json", record.pid));
        let error = |source| RegistryError {
            path: path.clone(),
            source,
        };
        fs::create_dir_all(directory).map_err(error)?;
        let temporary = directory.join(format!("{}.tmp", record.pid));
        let contents = serde_json::to_vec(record).expect("registry records serialize to JSON");
        fs::write(&temporary, contents).map_err(error)?;
        fs::rename(&temporary, &path).map_err(error)?;
        Ok(Self { path })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        drop(fs::remove_file(&self.path));
    }
}

/// Every parseable record in `directory`. Unreadable and malformed files are skipped.
pub(crate) fn read_all(directory: &Path) -> Vec<InstanceRecord> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut records: Vec<InstanceRecord> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .filter_map(|path| serde_json::from_slice(&fs::read(path).ok()?).ok())
        .collect();
    records.sort_by_key(|record| record.pid);
    records
}

#[cfg(test)]
mod tests {
    use super::{InstanceRecord, Registration, read_all};
    use std::fs;

    fn record(pid: u32) -> InstanceRecord {
        InstanceRecord {
            pid,
            port: 7878,
            workspace: "/work".into(),
            started_at: 1,
        }
    }

    #[test]
    fn registration_lives_exactly_as_long_as_the_instance() {
        let directory = tempfile::tempdir().unwrap();

        let registration = Registration::create(directory.path(), &record(7)).unwrap();
        assert_eq!(read_all(directory.path()), [record(7)]);

        drop(registration);
        assert!(read_all(directory.path()).is_empty());
    }

    #[test]
    fn readers_skip_stale_and_malformed_files() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("1.json"), "not json").unwrap();
        fs::write(directory.path().join("notes.txt"), "{}").unwrap();
        let _registration = Registration::create(directory.path(), &record(9)).unwrap();

        assert_eq!(read_all(directory.path()), [record(9)]);
    }

    #[test]
    fn registering_again_replaces_a_stale_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut stale = record(5);
        stale.port = 1;
        fs::write(
            directory.path().join("5.json"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();

        let _registration = Registration::create(directory.path(), &record(5)).unwrap();

        assert_eq!(read_all(directory.path()), [record(5)]);
    }
}
