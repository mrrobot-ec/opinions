//! Staging-only resolution crash barrier (Phase 6 D29).
//!
//! Composition selects this adapter only after the immutable staging +
//! explicit `CHAOS_ENABLED=1` arm is validated. Production uses the
//! application layer's transparent `NoopCrashPoint`.

use std::fs::OpenOptions;
use std::future::pending;
use std::io::Write;
use std::path::{Path, PathBuf};

use application::error::StoreError;
use application::ports::ResolutionCrashPoint;
use async_trait::async_trait;

pub const READY_FILE_ENV: &str = "CHAOS_RESOLUTION_READY_FILE";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CrashPointConfigError {
    #[error("{READY_FILE_ENV} is required when the resolution crash point is armed")]
    MissingReadyFile,
}

/// Armed staging adapter. `fire` publishes a readiness file atomically and
/// then never returns: the outer chaos controller must observe readiness and
/// kill the process, proving the intended pre-commit window was reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagingCrashPoint {
    readiness_file: PathBuf,
}

impl StagingCrashPoint {
    /// # Errors
    /// An absent or blank path is a startup configuration error.
    pub fn from_env_value(value: Option<&str>) -> Result<Self, CrashPointConfigError> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Err(CrashPointConfigError::MissingReadyFile);
        };
        Ok(Self {
            readiness_file: PathBuf::from(value),
        })
    }

    #[must_use]
    pub fn readiness_file(&self) -> &Path {
        &self.readiness_file
    }

    fn publish_readiness(&self) -> Result<(), StoreError> {
        let parent = self.readiness_file.parent().ok_or_else(|| {
            StoreError::Backend("resolution crash readiness path has no parent".into())
        })?;
        let file_name = self
            .readiness_file
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                StoreError::Backend("resolution crash readiness path has no file name".into())
            })?;
        let temporary = parent.join(format!(
            ".{file_name}.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        let result = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)?;
            file.write_all(b"ready\n")?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.readiness_file)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(|error| {
            StoreError::Backend(format!(
                "publish resolution crash readiness {}: {error}",
                self.readiness_file.display()
            ))
        })
    }
}

#[async_trait]
impl ResolutionCrashPoint for StagingCrashPoint {
    async fn fire(&self) -> Result<(), StoreError> {
        self.publish_readiness()?;
        pending::<Result<(), StoreError>>().await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use application::ports::NoopCrashPoint;

    use super::*;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "opinions-crash-point-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn armed_adapter_requires_a_non_blank_readiness_path() {
        assert_eq!(
            StagingCrashPoint::from_env_value(None),
            Err(CrashPointConfigError::MissingReadyFile)
        );
        assert_eq!(
            StagingCrashPoint::from_env_value(Some("  ")),
            Err(CrashPointConfigError::MissingReadyFile)
        );
        let point = StagingCrashPoint::from_env_value(Some(" /tmp/ready ")).unwrap();
        assert_eq!(point.readiness_file(), Path::new("/tmp/ready"));
    }

    #[tokio::test]
    async fn staging_point_publishes_atomically_then_awaits_kill() {
        let dir = temp_dir();
        let ready = dir.join("resolution.ready");
        let point = StagingCrashPoint::from_env_value(ready.to_str()).unwrap();
        let task = tokio::spawn(async move { point.fire().await });
        for _ in 0..100 {
            if ready.is_file() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(std::fs::read(&ready).unwrap(), b"ready\n");
        assert!(!task.is_finished());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn readiness_publication_failure_is_typed_and_cleans_temp_files() {
        let dir = temp_dir();
        let missing_parent = dir.join("missing").join("resolution.ready");
        let point = StagingCrashPoint::from_env_value(missing_parent.to_str()).unwrap();
        let error = point.publish_readiness().unwrap_err();
        assert!(
            matches!(error, StoreError::Backend(message) if message.contains("publish resolution crash readiness"))
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn readiness_path_must_have_a_parent_and_file_name() {
        let no_parent = StagingCrashPoint {
            readiness_file: PathBuf::new(),
        };
        assert!(matches!(
            no_parent.publish_readiness(),
            Err(StoreError::Backend(message)) if message.contains("no parent")
        ));

        let no_file_name = StagingCrashPoint {
            readiness_file: PathBuf::from("."),
        };
        assert!(matches!(
            no_file_name.publish_readiness(),
            Err(StoreError::Backend(message)) if message.contains("no file name")
        ));
    }

    #[tokio::test]
    async fn production_noop_remains_observationally_transparent() {
        assert_eq!(NoopCrashPoint.fire().await, Ok(()));
    }
}
