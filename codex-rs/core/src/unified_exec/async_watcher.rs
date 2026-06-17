use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use codex_features::Feature;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::Sleep;

use super::BackgroundCompletionEvent;
use super::BackgroundOutputArtifact;
use super::UnifiedExecContext;
use super::process::UnifiedExecProcess;
use crate::exec::MAX_EXEC_OUTPUT_DELTAS_PER_CALL;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::session::TurnInput;
use crate::tools::events::ToolEmitter;
use crate::tools::events::ToolEventCtx;
use crate::tools::events::ToolEventFailure;
use crate::tools::events::ToolEventStage;
use crate::unified_exec::head_tail_buffer::HeadTailBuffer;
use crate::util::escape_xml_text;
use codex_protocol::exec_output::ExecToolCallOutput;
use codex_protocol::exec_output::StreamOutput;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ExecCommandOutputDeltaEvent;
use codex_protocol::protocol::ExecCommandSource;
use codex_protocol::protocol::ExecOutputStream;
use codex_utils_absolute_path::AbsolutePathBuf;

pub(crate) const TRAILING_OUTPUT_GRACE: Duration = Duration::from_millis(100);

/// Upper bound for a single ExecCommandOutputDelta chunk emitted by unified exec.
///
/// The unified exec output buffer already caps *retained* output (see
/// `UNIFIED_EXEC_OUTPUT_MAX_BYTES`), but we also cap per-event payload size so
/// downstream event consumers (especially app-server JSON-RPC) don't have to
/// process arbitrarily large delta payloads.
const UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES: usize = 8192;

/// Spawn a background task that continuously reads from the PTY, appends to the
/// shared transcript, and emits ExecCommandOutputDelta events on UTF‑8
/// boundaries.
pub(crate) fn start_streaming_output(
    process: &UnifiedExecProcess,
    context: &UnifiedExecContext,
    transcript: Arc<Mutex<HeadTailBuffer>>,
) {
    let mut receiver = process.output_receiver();
    let output_drained = process.output_drained_notify();
    let output_drained_flag = process.output_drained_flag();
    let exit_token = process.cancellation_token();

    let session_ref = Arc::clone(&context.session);
    let turn_ref = Arc::clone(&context.turn);
    let call_id = context.call_id.clone();

    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;

        let mut pending = Vec::<u8>::new();
        let mut emitted_deltas: usize = 0;

        let mut grace_sleep: Option<Pin<Box<Sleep>>> = None;

        loop {
            tokio::select! {
                _ = exit_token.cancelled(), if grace_sleep.is_none() => {
                    let deadline = Instant::now() + TRAILING_OUTPUT_GRACE;
                    grace_sleep.replace(Box::pin(tokio::time::sleep_until(deadline)));
                }

                _ = async {
                    if let Some(sleep) = grace_sleep.as_mut() {
                        sleep.as_mut().await;
                    }
                }, if grace_sleep.is_some() => {
                    output_drained_flag.store(true, std::sync::atomic::Ordering::Release);
                    output_drained.notify_waiters();
                    break;
                }

                received = receiver.recv() => {
                    let chunk = match received {
                        Ok(chunk) => chunk,
                        Err(RecvError::Lagged(_)) => {
                            continue;
                        },
                        Err(RecvError::Closed) => {
                            output_drained_flag.store(true, std::sync::atomic::Ordering::Release);
                            output_drained.notify_waiters();
                            break;
                        }
                    };

                    process_chunk(
                        &mut pending,
                        &transcript,
                        &call_id,
                        &session_ref,
                        &turn_ref,
                        &mut emitted_deltas,
                        chunk,
                    ).await;
                }
            }
        }
    });
}

/// Spawn a background watcher that waits for the PTY to exit and then emits a
/// single ExecCommandEnd event with the aggregated transcript.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_exit_watcher(
    process: Arc<UnifiedExecProcess>,
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    process_id: i32,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    // SANDBOX PATCH: optional streaming-time artifact surfaced only for truncated wakes.
    output_artifact: Option<Arc<BackgroundOutputArtifact>>,
    notified: Arc<AtomicBool>,
    started_at: Instant,
) {
    let exit_token = process.cancellation_token();
    let output_drained = process.output_drained_notify();

    tokio::spawn(async move {
        exit_token.cancelled().await;
        output_drained.notified().await;

        let duration = Instant::now().saturating_duration_since(started_at);
        if let Some(message) = process.failure_message() {
            emit_failed_exec_end_for_unified_exec(
                session_ref,
                turn_ref,
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                transcript,
                String::new(),
                message,
                duration,
            )
            .await;
        } else {
            let exit_code = process.exit_code().unwrap_or(-1);
            emit_exec_end_for_unified_exec(
                Arc::clone(&session_ref),
                Arc::clone(&turn_ref),
                call_id,
                command,
                cwd,
                Some(process_id.to_string()),
                Arc::clone(&transcript),
                String::new(),
                exit_code,
                duration,
            )
            .await;
            if session_ref.enabled(Feature::BackgroundProcessNotification)
                && notified
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                let (output, output_was_truncated) = {
                    let guard = transcript.lock().await;
                    let retained = guard.to_bytes();
                    let omitted = guard.omitted_bytes();
                    let output_was_truncated = background_completion_output_was_truncated(
                        &retained,
                        omitted,
                        BACKGROUND_COMPLETION_OUTPUT_MAX_BYTES,
                    );
                    let output = build_background_completion_output(
                        &retained,
                        omitted,
                        BACKGROUND_COMPLETION_OUTPUT_MAX_BYTES,
                    );
                    (output, output_was_truncated)
                };
                // SANDBOX PATCH: expose a recovery artifact only when the inline wake
                // preview actually omitted output and the streaming spill succeeded.
                let output_artifact_path = if output_was_truncated {
                    if let Some(artifact) = output_artifact.as_ref() {
                        artifact.producer_finished().await;
                        artifact
                            .artifact_path()
                            .map(|path| path.to_string_lossy().into_owned())
                    } else {
                        None
                    }
                } else {
                    None
                };
                let message = background_completion_message(BackgroundCompletionEvent {
                    process_id,
                    exit_code,
                    output,
                    output_artifact_path,
                });
                session_ref
                    .input_queue
                    .queue_response_items_for_next_turn(vec![TurnInput::ResponseItem(
                        message.into(),
                    )])
                    .await;
                session_ref.request_pending_work_wake().await;
            }
        }
    });
}

pub(crate) fn background_completion_message(event: BackgroundCompletionEvent) -> ResponseInputItem {
    let task_id = escape_xml_text(&event.process_id.to_string());
    let exit_code = escape_xml_text(&event.exit_code.to_string());
    let summary = escape_xml_text(&format!(
        "Background shell command completed (exit code {exit_code})"
    ));
    let output = escape_xml_text(&event.output);
    // SANDBOX PATCH: keep the legacy inline <output> preview and append the
    // optional recovery reference immediately after it.
    let output_artifact_path = event
        .output_artifact_path
        .as_deref()
        .map(|path| {
            format!(
                "<output_artifact_path>{}</output_artifact_path>",
                escape_xml_text(path)
            )
        })
        .unwrap_or_default();
    let text = format!(
        "<task_notification><task_id>{task_id}</task_id><status>completed</status><exit_code>{exit_code}</exit_code><summary>{summary}</summary><output>{output}</output>{output_artifact_path}</task_notification>"
    );
    ResponseInputItem::Message {
        role: "user".to_string(),
        content: vec![ContentItem::InputText { text }],
        phase: None,
    }
}

/// Upper bound for the aggregated process output embedded inline in a background completion
/// `<task_notification>`.
///
/// The shared transcript is already capped at `UNIFIED_EXEC_OUTPUT_MAX_BYTES` (1 MiB), but
/// that wake notification is injected into the agent context on *every* background
/// completion, so we apply a tighter head+tail window here. (The removed
/// `await_background_completion` tool defaulted to `DEFAULT_MAX_OUTPUT_TOKENS` = 10_000
/// tokens, on the order of 40 KiB; 16 KiB keeps the most relevant head and tail of typical
/// command output without flooding context.)
const BACKGROUND_COMPLETION_OUTPUT_MAX_BYTES: usize = 16 * 1024;

/// Build the bounded process-output string embedded in a background completion notification.
///
/// `retained` is the transcript content still held by the head/tail buffer (already missing
/// `buffer_omitted` bytes that the 1 MiB cap dropped from the middle). When the combined
/// output exceeds `max_bytes`, keep a head and tail window on UTF-8 char boundaries and
/// replace the middle with a single marker reporting the total omitted byte count; when only
/// the upstream 1 MiB cap dropped bytes, append the same marker so the omission stays
/// visible to the model.
fn build_background_completion_output(
    retained: &[u8],
    buffer_omitted: usize,
    max_bytes: usize,
) -> String {
    let text = String::from_utf8_lossy(retained);
    let total_len = text.len();

    if total_len <= max_bytes {
        if buffer_omitted == 0 {
            return text.into_owned();
        }
        return format!(
            "{text}\n[... output truncated, {buffer_omitted} bytes omitted from the middle ...]"
        );
    }

    let head_budget = max_bytes / 2;
    let tail_budget = max_bytes - head_budget;
    let head_end = (0..=head_budget)
        .rev()
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(0);
    let tail_start = (total_len.saturating_sub(tail_budget)..=total_len)
        .find(|&i| text.is_char_boundary(i))
        .unwrap_or(total_len);
    let omitted = buffer_omitted.saturating_add(tail_start.saturating_sub(head_end));
    format!(
        "{head}\n[... output truncated, {omitted} bytes omitted from the middle ...]\n{tail}",
        head = &text[..head_end],
        tail = &text[tail_start..],
    )
}

// SANDBOX PATCH: mirrors build_background_completion_output's truncation
// branches without changing the existing inline preview function.
fn background_completion_output_was_truncated(
    retained: &[u8],
    buffer_omitted: usize,
    max_bytes: usize,
) -> bool {
    buffer_omitted > 0 || String::from_utf8_lossy(retained).len() > max_bytes
}

async fn process_chunk(
    pending: &mut Vec<u8>,
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    call_id: &str,
    session_ref: &Arc<Session>,
    turn_ref: &Arc<TurnContext>,
    emitted_deltas: &mut usize,
    chunk: Vec<u8>,
) {
    pending.extend_from_slice(&chunk);
    while let Some(prefix) = split_valid_utf8_prefix(pending) {
        {
            let mut guard = transcript.lock().await;
            guard.push_chunk(prefix.to_vec());
        }

        if *emitted_deltas >= MAX_EXEC_OUTPUT_DELTAS_PER_CALL {
            continue;
        }

        let event = ExecCommandOutputDeltaEvent {
            call_id: call_id.to_string(),
            stream: ExecOutputStream::Stdout,
            chunk: prefix,
        };
        session_ref
            .send_event(turn_ref.as_ref(), EventMsg::ExecCommandOutputDelta(event))
            .await;
        *emitted_deltas += 1;
    }
}

/// Emit an ExecCommandEnd event for a unified exec session, using the transcript
/// as the primary source of aggregated_output and falling back to the provided
/// text when the transcript is empty.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    process_id: Option<String>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    exit_code: i32,
    duration: Duration,
) {
    let aggregated_output = resolve_aggregated_output(&transcript, fallback_output).await;
    let output = ExecToolCallOutput {
        exit_code,
        stdout: StreamOutput::new(aggregated_output.clone()),
        stderr: StreamOutput::new(String::new()),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Success {
                output,
                applied_patch_delta: None,
            },
        )
        .await;
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn emit_failed_exec_end_for_unified_exec(
    session_ref: Arc<Session>,
    turn_ref: Arc<TurnContext>,
    call_id: String,
    command: Vec<String>,
    cwd: AbsolutePathBuf,
    process_id: Option<String>,
    transcript: Arc<Mutex<HeadTailBuffer>>,
    fallback_output: String,
    message: String,
    duration: Duration,
) {
    let stdout = if fallback_output.is_empty() {
        resolve_aggregated_output(&transcript, fallback_output).await
    } else {
        fallback_output
    };
    let aggregated_output = if stdout.is_empty() {
        message.clone()
    } else {
        format!("{stdout}\n{message}")
    };
    let output = ExecToolCallOutput {
        exit_code: -1,
        stdout: StreamOutput::new(stdout),
        stderr: StreamOutput::new(message),
        aggregated_output: StreamOutput::new(aggregated_output),
        duration,
        timed_out: false,
    };
    let event_ctx = ToolEventCtx::new(
        session_ref.as_ref(),
        turn_ref.as_ref(),
        &call_id,
        /*turn_diff_tracker*/ None,
    );
    let emitter = ToolEmitter::unified_exec(
        &command,
        cwd,
        ExecCommandSource::UnifiedExecStartup,
        process_id,
    );
    emitter
        .emit(
            event_ctx,
            ToolEventStage::Failure(ToolEventFailure::Output(output)),
        )
        .await;
}

fn split_valid_utf8_prefix(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    split_valid_utf8_prefix_with_max(buffer, UNIFIED_EXEC_OUTPUT_DELTA_MAX_BYTES)
}

fn split_valid_utf8_prefix_with_max(buffer: &mut Vec<u8>, max_bytes: usize) -> Option<Vec<u8>> {
    if buffer.is_empty() {
        return None;
    }

    let max_len = buffer.len().min(max_bytes);
    let mut split = max_len;
    while split > 0 {
        if std::str::from_utf8(&buffer[..split]).is_ok() {
            let prefix = buffer[..split].to_vec();
            buffer.drain(..split);
            return Some(prefix);
        }

        if max_len - split > 4 {
            break;
        }
        split -= 1;
    }

    // If no valid UTF-8 prefix was found, emit the first byte so the stream
    // keeps making progress and the transcript reflects all bytes.
    let byte = buffer.drain(..1).collect();
    Some(byte)
}

async fn resolve_aggregated_output(
    transcript: &Arc<Mutex<HeadTailBuffer>>,
    fallback: String,
) -> String {
    let guard = transcript.lock().await;
    if guard.retained_bytes() == 0 {
        return fallback;
    }

    String::from_utf8_lossy(&guard.to_bytes()).to_string()
}

#[cfg(test)]
#[path = "async_watcher_tests.rs"]
mod tests;
