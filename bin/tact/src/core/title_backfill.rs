//! Bounded background generation for new and previously saved sessions.

use crate::{
    app::config::Config,
    core::{
        storage::{SessionStorage, StorageError},
        titles,
    },
};
use std::{path::PathBuf, time::Duration};
use tokio::task::JoinSet;

pub(crate) async fn run(config: Config) {
    loop {
        let path = config.path().to_owned();
        let batch = tokio::task::spawn_blocking(move || {
            let storage = SessionStorage::open(&path)?;
            storage.inherit_titles()?;
            let mut claimed = Vec::new();
            for (session, prompt) in storage.untitled_prompts(2)? {
                let fallback = titles::fallback(&prompt);
                if storage.claim_title(&session, &fallback)? {
                    claimed.push((session, prompt));
                }
            }
            Ok::<_, StorageError>(claimed)
        })
        .await;
        let mut tasks = JoinSet::new();
        if let Ok(Ok(batch)) = batch {
            for (session, prompt) in batch {
                let config = config.clone();
                tasks.spawn(async move {
                    if let Some(title) = titles::generate(&config, &prompt).await {
                        let path: PathBuf = config.path().into();
                        let _ = tokio::task::spawn_blocking(move || {
                            SessionStorage::open(&path)?.complete_title(&session, &title)
                        })
                        .await;
                    }
                });
            }
        }
        while tasks.join_next().await.is_some() {}
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
