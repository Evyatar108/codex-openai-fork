use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

use crossterm::event::Event;
#[cfg(windows)]
use serde::Serialize;

use crate::tui::TuiEvent;

const TRACE_ENV_VAR: &str = "CODEX_CONSOLE_MODE_TRACE";

#[cfg(windows)]
const TRACE_VERBOSE_ENV_VAR: &str = "CODEX_CONSOLE_MODE_TRACE_VERBOSE";

#[cfg(windows)]
const SIDECAR_FILE_NAME: &str = "console-mode-trace.jsonl";

#[cfg(windows)]
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;

#[cfg(windows)]
const FNV_PRIME: u64 = 0x100000001b3;

#[cfg(windows)]
use std::fs::OpenOptions;
#[cfg(windows)]
use std::io::Write;
#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::OnceLock;
#[cfg(windows)]
use std::time::Instant;
#[cfg(windows)]
use std::time::SystemTime;
#[cfg(windows)]
use std::time::UNIX_EPOCH;

#[cfg(windows)]
use codex_app_server_protocol::ServerNotification;
#[cfg(windows)]
use codex_app_server_protocol::ThreadItem;

#[cfg(windows)]
static TRACE_ENABLED: OnceLock<bool> = OnceLock::new();

#[cfg(windows)]
static TRACE_VERBOSE: OnceLock<bool> = OnceLock::new();

#[cfg(windows)]
static TRACE_STATE: OnceLock<Mutex<TraceState>> = OnceLock::new();

#[cfg(windows)]
#[derive(Default)]
struct TraceState {
    sidecar_path: Option<PathBuf>,
    rollout_path_tail: Option<String>,
    last_seen_mode: Option<u32>,
    last_emitted_bad_mode: Option<u32>,
    last_input_event: Option<TimedMarker<InputEventMarker>>,
    recent_action: Option<TimedMarker<ActionMarker>>,
    recent_terminal_marker: Option<TimedMarker<TerminalMarker>>,
}

#[cfg(windows)]
#[derive(Clone)]
struct TimedMarker<T> {
    marker: T,
    recorded_at: Instant,
}

#[cfg(windows)]
#[derive(Clone, Serialize)]
struct AgedMarker<T> {
    #[serde(flatten)]
    marker: T,
    age_ms: u64,
}

#[cfg(windows)]
#[derive(Clone, Serialize)]
struct InputEventMarker {
    kind: &'static str,
}

#[cfg(windows)]
#[derive(Clone, Serialize)]
struct ActionMarker {
    kind: &'static str,
    phase: &'static str,
    item_id_hash: String,
    thread_id_hash: String,
    turn_id_hash: String,
    source: Option<String>,
    status: String,
    tool: Option<String>,
    process_id_hash: Option<String>,
}

#[cfg(windows)]
#[derive(Clone, Serialize)]
struct TerminalMarker {
    kind: &'static str,
}

#[cfg(windows)]
#[derive(Serialize)]
struct ConsoleModeDeltaRecord {
    kind: &'static str,
    timestamp_ms: u64,
    snapshot_site: &'static str,
    rollout_path_tail: Option<String>,
    previous_mode: Option<ModeDescription>,
    before_reassert_mode: ModeDescription,
    expected_after_reassert_mode: ModeDescription,
    last_input_event: Option<AgedMarker<InputEventMarker>>,
    recent_action: Option<AgedMarker<ActionMarker>>,
    recent_terminal_marker: Option<AgedMarker<TerminalMarker>>,
}

#[cfg(windows)]
#[derive(Serialize)]
struct ModeDescription {
    flags: Vec<&'static str>,
    unexpected_set: Vec<&'static str>,
}

#[cfg(windows)]
struct PreparedRecord {
    sidecar_path: PathBuf,
    record: ConsoleModeDeltaRecord,
}

pub(crate) fn enabled() -> bool {
    enabled_impl()
}

#[cfg(windows)]
fn enabled_impl() -> bool {
    *TRACE_ENABLED.get_or_init(|| trace_enabled_from_env_value(std::env::var(TRACE_ENV_VAR).ok()))
}

#[cfg(not(windows))]
fn enabled_impl() -> bool {
    false
}

#[cfg(windows)]
fn verbose_enabled() -> bool {
    *TRACE_VERBOSE
        .get_or_init(|| trace_enabled_from_env_value(std::env::var(TRACE_VERBOSE_ENV_VAR).ok()))
}

pub(crate) fn set_rollout_path(rollout_path: Option<&Path>) {
    if !enabled() {
        return;
    }

    #[cfg(windows)]
    {
        let Some(rollout_path) = rollout_path else {
            return;
        };
        with_trace_state(|state| state.set_rollout_path(rollout_path));
    }

    #[cfg(not(windows))]
    let _ = rollout_path;
}

pub(crate) fn record_crossterm_event(event: &Event) {
    if !enabled() {
        return;
    }

    #[cfg(windows)]
    {
        let input_event = match event {
            Event::Key(_) => Some(InputEventMarker { kind: "key" }),
            Event::Paste(_) => Some(InputEventMarker { kind: "paste" }),
            Event::Resize(_, _) => {
                with_trace_state(|state| state.record_terminal_marker("resize"));
                Some(InputEventMarker { kind: "resize" })
            }
            Event::FocusGained => {
                with_trace_state(|state| state.record_terminal_marker("focus_gained"));
                Some(InputEventMarker {
                    kind: "focus_gained",
                })
            }
            Event::FocusLost => {
                with_trace_state(|state| state.record_terminal_marker("focus_lost"));
                Some(InputEventMarker { kind: "focus_lost" })
            }
            _ => None,
        };
        if let Some(marker) = input_event {
            with_trace_state(|state| state.last_input_event = Some(TimedMarker::new(marker)));
        }
    }

    #[cfg(not(windows))]
    let _ = event;
}

pub(crate) fn record_tui_event(event: &TuiEvent) {
    if !enabled() {
        return;
    }

    #[cfg(windows)]
    {
        let kind = match event {
            TuiEvent::Key(_) => Some("tui_key"),
            TuiEvent::Paste(_) => Some("tui_paste"),
            TuiEvent::Draw => Some("draw"),
            TuiEvent::Resize => Some("resize"),
        };
        if let Some(kind) = kind {
            with_trace_state(|state| state.record_terminal_marker(kind));
        }
    }

    #[cfg(not(windows))]
    let _ = event;
}

pub(crate) fn record_terminal_marker(kind: &'static str) {
    if !enabled() {
        return;
    }

    #[cfg(windows)]
    with_trace_state(|state| state.record_terminal_marker(kind));

    #[cfg(not(windows))]
    let _ = kind;
}

pub(crate) fn record_server_notification(
    notification: &codex_app_server_protocol::ServerNotification,
) {
    if !enabled() {
        return;
    }

    #[cfg(windows)]
    {
        if let Some(marker) = action_marker_from_notification(notification) {
            with_trace_state(|state| state.recent_action = Some(TimedMarker::new(marker)));
        }
    }

    #[cfg(not(windows))]
    let _ = notification;
}

#[cfg(windows)]
pub(crate) fn record_console_mode_delta_before_reassert(live_mode: u32, expected_mode: u32) {
    record_console_mode_delta_before_reassert_if_enabled(enabled(), live_mode, expected_mode);
}

#[cfg(windows)]
fn record_console_mode_delta_before_reassert_if_enabled(
    enabled: bool,
    live_mode: u32,
    expected_mode: u32,
) {
    if !enabled {
        return;
    }

    with_trace_state(|state| {
        record_console_mode_delta_before_reassert_for_state(
            enabled,
            state,
            live_mode,
            expected_mode,
        )
    });
}

#[cfg(windows)]
fn record_console_mode_delta_before_reassert_for_state(
    enabled: bool,
    state: &mut TraceState,
    live_mode: u32,
    expected_mode: u32,
) {
    if !enabled {
        return;
    }

    let prepared = state.prepare_record(live_mode, expected_mode);
    if let Some(prepared) = prepared
        && let Err(err) = append_record(&prepared.sidecar_path, &prepared.record)
    {
        tracing::warn!(
            target: "codex_console_mode_trace",
            error = %err,
            "failed to append Windows console mode trace record"
        );
    }
}

#[cfg(windows)]
impl<T> TimedMarker<T> {
    fn new(marker: T) -> Self {
        Self {
            marker,
            recorded_at: Instant::now(),
        }
    }
}

#[cfg(windows)]
impl<T: Clone> TimedMarker<T> {
    fn aged(&self) -> AgedMarker<T> {
        AgedMarker {
            marker: self.marker.clone(),
            age_ms: elapsed_ms(self.recorded_at),
        }
    }
}

#[cfg(windows)]
impl TraceState {
    fn set_rollout_path(&mut self, rollout_path: &Path) {
        let Some(parent) = rollout_path.parent() else {
            return;
        };
        self.sidecar_path = Some(parent.join(SIDECAR_FILE_NAME));
        self.rollout_path_tail = Some(sanitized_rollout_path_tail(rollout_path));
    }

    fn record_terminal_marker(&mut self, kind: &'static str) {
        self.recent_terminal_marker = Some(TimedMarker::new(TerminalMarker { kind }));
    }

    fn prepare_record(&mut self, live_mode: u32, expected_mode: u32) -> Option<PreparedRecord> {
        let previous_mode = self.last_seen_mode;
        let mode_changed = previous_mode != Some(live_mode);
        let unexpected_bits = super::unexpected_codex_tui_input_bits(live_mode);
        let should_emit = if unexpected_bits == 0 {
            if mode_changed {
                self.last_emitted_bad_mode = None;
            }
            verbose_enabled() && mode_changed
        } else {
            self.last_emitted_bad_mode != Some(live_mode)
        };

        self.last_seen_mode = Some(live_mode);
        if unexpected_bits != 0 {
            self.last_emitted_bad_mode = Some(live_mode);
        }

        if !should_emit {
            return None;
        }

        let sidecar_path = self
            .sidecar_path
            .clone()
            .or_else(default_sidecar_path)
            .unwrap_or_else(|| {
                std::env::temp_dir()
                    .join(format!("console-mode-trace-{}.jsonl", std::process::id()))
            });

        Some(PreparedRecord {
            sidecar_path,
            record: ConsoleModeDeltaRecord {
                kind: "console-mode-delta",
                timestamp_ms: timestamp_ms(),
                snapshot_site: "tui.event_stream.before_read",
                rollout_path_tail: self.rollout_path_tail.clone(),
                previous_mode: previous_mode.map(mode_description),
                before_reassert_mode: mode_description(live_mode),
                expected_after_reassert_mode: mode_description(expected_mode),
                last_input_event: self.last_input_event.as_ref().map(TimedMarker::aged),
                recent_action: self.recent_action.as_ref().map(TimedMarker::aged),
                recent_terminal_marker: self.recent_terminal_marker.as_ref().map(TimedMarker::aged),
            },
        })
    }
}

#[cfg(windows)]
fn with_trace_state<R>(f: impl FnOnce(&mut TraceState) -> R) -> R {
    let state = TRACE_STATE.get_or_init(|| Mutex::new(TraceState::default()));
    let mut state = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut state)
}

#[cfg(windows)]
fn action_marker_from_notification(notification: &ServerNotification) -> Option<ActionMarker> {
    match notification {
        ServerNotification::ItemStarted(notification) => action_marker_from_item(
            "started",
            &notification.thread_id,
            &notification.turn_id,
            &notification.item,
        ),
        ServerNotification::ItemCompleted(notification) => action_marker_from_item(
            "completed",
            &notification.thread_id,
            &notification.turn_id,
            &notification.item,
        ),
        _ => None,
    }
}

#[cfg(windows)]
fn action_marker_from_item(
    phase: &'static str,
    thread_id: &str,
    turn_id: &str,
    item: &ThreadItem,
) -> Option<ActionMarker> {
    match item {
        ThreadItem::CommandExecution {
            id,
            process_id,
            source,
            status,
            ..
        } => Some(ActionMarker {
            kind: "command_execution",
            phase,
            item_id_hash: hash_id(id),
            thread_id_hash: hash_id(thread_id),
            turn_id_hash: hash_id(turn_id),
            source: Some(format!("{source:?}")),
            status: format!("{status:?}"),
            tool: None,
            process_id_hash: process_id.as_deref().map(hash_id),
        }),
        ThreadItem::CollabAgentToolCall {
            id, tool, status, ..
        } => Some(ActionMarker {
            kind: "collab_agent_tool_call",
            phase,
            item_id_hash: hash_id(id),
            thread_id_hash: hash_id(thread_id),
            turn_id_hash: hash_id(turn_id),
            source: None,
            status: format!("{status:?}"),
            tool: Some(format!("{tool:?}")),
            process_id_hash: None,
        }),
        _ => None,
    }
}

#[cfg(windows)]
fn append_record(path: &Path, record: &ConsoleModeDeltaRecord) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    serde_json::to_writer(&mut file, record)?;
    writeln!(file)?;
    Ok(())
}

#[cfg(windows)]
fn mode_description(mode: u32) -> ModeDescription {
    ModeDescription {
        flags: mode_flags(mode),
        unexpected_set: mode_flags(super::unexpected_codex_tui_input_bits(mode)),
    }
}

#[cfg(windows)]
fn mode_flags(mode: u32) -> Vec<&'static str> {
    let mut flags = Vec::new();
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_PROCESSED_INPUT,
        "ENABLE_PROCESSED_INPUT",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_LINE_INPUT,
        "ENABLE_LINE_INPUT",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_ECHO_INPUT,
        "ENABLE_ECHO_INPUT",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_WINDOW_INPUT,
        "ENABLE_WINDOW_INPUT",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_MOUSE_INPUT,
        "ENABLE_MOUSE_INPUT",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_INSERT_MODE,
        "ENABLE_INSERT_MODE",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_QUICK_EDIT_MODE,
        "ENABLE_QUICK_EDIT_MODE",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_EXTENDED_FLAGS,
        "ENABLE_EXTENDED_FLAGS",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_AUTO_POSITION,
        "ENABLE_AUTO_POSITION",
    );
    push_mode_flag(
        &mut flags,
        mode,
        windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_INPUT,
        "ENABLE_VIRTUAL_TERMINAL_INPUT",
    );
    flags
}

#[cfg(windows)]
fn push_mode_flag(flags: &mut Vec<&'static str>, mode: u32, bit: u32, name: &'static str) {
    if mode & bit != 0 {
        flags.push(name);
    }
}

#[cfg(windows)]
fn default_sidecar_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join(".codex")
            .join("sessions")
            .join(format!("console-mode-trace-{}.jsonl", std::process::id()))
    })
}

#[cfg(windows)]
fn sanitized_rollout_path_tail(path: &Path) -> String {
    let components = path
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => Some(value.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let start = components.len().saturating_sub(4);
    components[start..]
        .iter()
        .map(|component| {
            if component.starts_with("rollout-") {
                format!("rollout-{}.jsonl", hash_id(component))
            } else {
                component.clone()
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(windows)]
fn timestamp_ms() -> u64 {
    match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
        Err(err) => {
            tracing::warn!(
                target: "codex_console_mode_trace",
                error = %err,
                "system clock is before Unix epoch while writing console mode trace"
            );
            0
        }
    }
}

#[cfg(windows)]
fn elapsed_ms(recorded_at: Instant) -> u64 {
    u64::try_from(recorded_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(windows)]
fn hash_id(value: &str) -> String {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("fnv1a64:{hash:016x}")
}

#[cfg(windows)]
fn trace_enabled_from_env_value(value: Option<String>) -> bool {
    value.as_deref().is_some_and(|value| {
        matches!(
            value.to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(test)]
mod tests {
    #[cfg(windows)]
    use super::*;

    #[cfg(windows)]
    use codex_app_server_protocol::CommandExecutionSource;
    #[cfg(windows)]
    use codex_app_server_protocol::CommandExecutionStatus;
    #[cfg(windows)]
    use codex_app_server_protocol::ItemStartedNotification;
    #[cfg(windows)]
    use pretty_assertions::assert_eq;

    #[cfg(windows)]
    #[test]
    fn trace_env_unset_is_disabled() {
        assert!(!trace_enabled_from_env_value(None));
        assert!(trace_enabled_from_env_value(Some("1".to_string())));
        assert!(trace_enabled_from_env_value(Some("true".to_string())));
        assert!(!trace_enabled_from_env_value(Some("0".to_string())));
    }

    #[cfg(windows)]
    #[test]
    fn disabled_trace_writes_no_sidecar_for_bad_mode() {
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let sidecar_path = temp_dir.path().join(SIDECAR_FILE_NAME);
        let mut state = TraceState {
            sidecar_path: Some(sidecar_path.clone()),
            ..Default::default()
        };
        let live_mode = windows_sys::Win32::System::Console::ENABLE_LINE_INPUT
            | windows_sys::Win32::System::Console::ENABLE_ECHO_INPUT
            | windows_sys::Win32::System::Console::ENABLE_PROCESSED_INPUT
            | windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_INPUT;
        let expected_mode = super::super::codex_tui_input_mode(live_mode);
        record_console_mode_delta_before_reassert_for_state(
            trace_enabled_from_env_value(None),
            &mut state,
            live_mode,
            expected_mode,
        );

        assert_eq!(state.last_seen_mode, None);
        assert!(
            !sidecar_path.exists(),
            "trace sidecar must not be created while {TRACE_ENV_VAR} is unset"
        );
    }

    #[cfg(windows)]
    #[test]
    fn server_notification_marker_redacts_sensitive_payloads() {
        let secret = "CODEx-TRACE-PRIVACY-CANARY";
        let item = ThreadItem::CommandExecution {
            id: format!("item-{secret}"),
            command: format!("echo {secret}"),
            cwd: codex_utils_absolute_path::AbsolutePathBuf::try_from(PathBuf::from(format!(
                "C:\\sensitive\\{secret}"
            )))
            .expect("absolute path"),
            process_id: Some(format!("pid-{secret}")),
            source: CommandExecutionSource::UnifiedExecStartup,
            status: CommandExecutionStatus::Completed,
            command_actions: Vec::new(),
            aggregated_output: Some(format!("output {secret}")),
            exit_code: Some(0),
            duration_ms: Some(1),
        };
        let notification = ServerNotification::ItemStarted(ItemStartedNotification {
            item,
            thread_id: format!("thread-{secret}"),
            turn_id: format!("turn-{secret}"),
            started_at_ms: 1,
        });

        let marker = action_marker_from_notification(&notification).expect("action marker");
        let marker_json = serde_json::to_string(&marker).expect("serialize marker");

        assert!(!marker_json.contains(secret));
        assert_eq!(marker.kind, "command_execution");
        assert_eq!(marker.source.as_deref(), Some("UnifiedExecStartup"));
        assert_eq!(marker.status, "Completed");
    }
}
