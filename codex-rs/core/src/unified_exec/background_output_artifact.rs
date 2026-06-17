use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_features::Feature;
use codex_utils_absolute_path::AbsolutePathBuf;
use tokio::fs::File;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tracing::warn;

use crate::session::session::Session;
use crate::session::turn_context::TurnContext;

const BACKGROUND_OUTPUT_ARTIFACTS_DIR: &str = "background-output";

#[derive(Debug)]
enum WriterState {
    Unopened,
    Open(File),
    Failed,
}

/// Session-scoped sink for background process output that is written while bytes
/// are still streaming, before the shared head/tail transcript can omit middle
/// content.
#[derive(Debug)]
pub(crate) struct BackgroundOutputArtifact {
    path: AbsolutePathBuf,
    state: Mutex<WriterState>,
    wrote_any: AtomicBool,
    failed: AtomicBool,
    producer_finished: AtomicBool,
    producer_finished_notify: Notify,
}

impl BackgroundOutputArtifact {
    // SANDBOX PATCH: default-off background wake spill artifact for large
    // background completion notifications.
    pub(crate) fn new_if_enabled(
        session: &Session,
        turn: &TurnContext,
        call_id: &str,
        process_id: i32,
    ) -> Option<Arc<Self>> {
        if !session.enabled(Feature::BackgroundProcessNotification) {
            return None;
        }

        Some(Arc::new(Self::new_for_path(Self::path_for_session(
            &turn.config.codex_home,
            &session.thread_id().to_string(),
            call_id,
            process_id,
        ))))
    }

    pub(crate) async fn write_chunk(&self, bytes: &[u8]) {
        if bytes.is_empty() || self.failed.load(Ordering::Acquire) {
            return;
        }

        if let Err(err) = self.write_chunk_inner(bytes).await {
            self.mark_failed(err).await;
        }
    }

    pub(crate) fn artifact_path(&self) -> Option<AbsolutePathBuf> {
        if self.producer_finished.load(Ordering::Acquire)
            && self.wrote_any.load(Ordering::Acquire)
            && !self.failed.load(Ordering::Acquire)
        {
            Some(self.path.clone())
        } else {
            None
        }
    }

    pub(crate) fn mark_producer_finished(&self) {
        self.producer_finished.store(true, Ordering::Release);
        self.producer_finished_notify.notify_waiters();
    }

    pub(crate) async fn producer_finished(&self) {
        if self.producer_finished.load(Ordering::Acquire) {
            return;
        }
        self.producer_finished_notify.notified().await;
    }

    pub(crate) async fn mark_incomplete(&self, reason: &'static str) {
        if self.failed.swap(true, Ordering::AcqRel) {
            return;
        }

        let mut state = self.state.lock().await;
        *state = WriterState::Failed;
        warn!(
            path = %self.path.to_string_lossy(),
            reason,
            "background output artifact is incomplete"
        );
    }

    fn new_for_path(path: AbsolutePathBuf) -> Self {
        Self {
            path,
            state: Mutex::new(WriterState::Unopened),
            wrote_any: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            producer_finished: AtomicBool::new(false),
            producer_finished_notify: Notify::new(),
        }
    }

    fn path_for_session(
        codex_home: &AbsolutePathBuf,
        conversation_id: &str,
        call_id: &str,
        process_id: i32,
    ) -> AbsolutePathBuf {
        codex_home
            .join(crate::rollout::SESSIONS_SUBDIR)
            .join(sanitize_path_component(conversation_id, "conversation"))
            .join(BACKGROUND_OUTPUT_ARTIFACTS_DIR)
            .join(format!(
                "{}-{}.log",
                sanitize_path_component(call_id, "call"),
                sanitize_path_component(&process_id.to_string(), "process")
            ))
    }

    async fn write_chunk_inner(&self, bytes: &[u8]) -> io::Result<()> {
        let mut state = self.state.lock().await;
        if matches!(*state, WriterState::Failed) {
            return Ok(());
        }

        if matches!(*state, WriterState::Unopened) {
            if let Some(parent) = self.path.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .truncate(true)
                .open(&self.path)
                .await?;
            *state = WriterState::Open(file);
        }

        let WriterState::Open(file) = &mut *state else {
            return Ok(());
        };
        file.write_all(bytes).await?;
        self.wrote_any.store(true, Ordering::Release);
        Ok(())
    }

    async fn mark_failed(&self, err: io::Error) {
        if self.failed.swap(true, Ordering::AcqRel) {
            return;
        }

        let mut state = self.state.lock().await;
        *state = WriterState::Failed;
        warn!(
            path = %self.path.to_string_lossy(),
            error = %err,
            "failed to write background output artifact"
        );
    }
}

fn sanitize_path_component(value: &str, fallback: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use codex_utils_absolute_path::AbsolutePathBuf;
    use pretty_assertions::assert_eq;
    use tempfile::TempDir;

    use super::BackgroundOutputArtifact;
    use super::sanitize_path_component;

    fn absolute_path(path: PathBuf) -> AbsolutePathBuf {
        AbsolutePathBuf::try_from(path).expect("absolute path")
    }

    #[tokio::test]
    async fn write_chunk_lazily_creates_artifact_file() {
        let temp = TempDir::new().expect("tempdir");
        let path = absolute_path(temp.path().join("nested").join("artifact.log"));
        let artifact = BackgroundOutputArtifact::new_for_path(path.clone());

        assert_eq!(artifact.artifact_path(), None);

        artifact.write_chunk(b"hello ").await;
        artifact.write_chunk(b"world").await;

        assert_eq!(
            tokio::fs::read(&path).await.expect("artifact bytes"),
            b"hello world"
        );
        assert_eq!(artifact.artifact_path(), None);
        artifact.mark_producer_finished();
        assert_eq!(artifact.artifact_path(), Some(path));
    }

    #[tokio::test]
    async fn write_chunk_failure_suppresses_artifact_reference() {
        let temp = TempDir::new().expect("tempdir");
        let file_parent = temp.path().join("not-a-dir");
        std::fs::write(&file_parent, b"parent file").expect("write parent file");
        let path = absolute_path(file_parent.join("artifact.log"));
        let artifact = BackgroundOutputArtifact::new_for_path(path);

        artifact.write_chunk(b"lost").await;

        assert_eq!(artifact.artifact_path(), None);
    }

    #[test]
    fn path_for_session_sanitizes_dynamic_components() {
        let temp = TempDir::new().expect("tempdir");
        let codex_home = absolute_path(temp.path().to_path_buf());
        let path = BackgroundOutputArtifact::path_for_session(
            &codex_home,
            "conversation/id",
            "call:id<>",
            42,
        );
        let path = path.to_string_lossy();

        assert!(path.contains("conversation_id"));
        assert!(path.contains("call_id__-42.log"));
    }

    #[test]
    fn sanitize_path_component_uses_fallback_for_empty_values() {
        assert_eq!(sanitize_path_component("", "fallback"), "fallback");
        assert_eq!(
            sanitize_path_component("abc-DEF_123", "fallback"),
            "abc-DEF_123"
        );
        assert_eq!(sanitize_path_component("a/b:c", "fallback"), "a_b_c");
    }
}
