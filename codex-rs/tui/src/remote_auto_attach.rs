// SANDBOX PATCH: remote_auto_attach — a trimmed copy of `remote_on_flow`
// (chatwidget/slash_dispatch.rs) that fires the same intent `/remote on` fires,
// automatically, at TUI startup when `Feature::RemoteAutoAttach` is enabled:
// ensure the per-machine Happy daemon is running, then attach via the SINGLE
// attach path (`AppEvent::SetRemoteSession`). The one behavioral delta from
// `remote_on_flow` is the creds-absent branch: instead of launching the
// interactive device-flow onboard, this unprompted background path emits ONE
// passive `Info` onboarding hint and returns (Plan Q2 exception). Genuine
// failures stay LOUD: an `ensure_running` error surfaces a `RemoteSessionNotice`
// error here, and attach failures surface through `maybe_attach_reporting`
// inside `apply_remote_session_toggle` (Plan Q2). The module is fork-authored
// with zero upstream-canonical conflict surface — only the one-line startup
// trigger in `app.rs` and the `mod` line in `lib.rs` are upstream-canonical.

use crate::app_event::AppEvent;
use crate::app_event::RemoteSessionNoticeLevel;
use crate::app_event_sender::AppEventSender;
use codex_happy::daemon_supervisor::SessionPlaneSupervisor as _;

/// Auto-attach this Codex session to the Happy app at startup, firing the same
/// intent `/remote on` fires (daemon `ensure_running` -> attach) minus the
/// interactive onboard. Non-blocking: spawned as a background task so the first
/// interactive prompt is never delayed.
pub(crate) async fn auto_attach_flow(tx: AppEventSender) {
    // Silent no-op when `~/.happy` cannot be resolved: an un-onboarded machine
    // with no Happy home is plain vanilla codex, not an error.
    let Some(home) = codex_happy::auth::happy_home_dir() else {
        return;
    };

    // Q2 exception: creds absent (never onboarded) -> ONE passive Info hint
    // inviting onboarding, NOT a loud error and NOT an auto-launched device flow.
    if !codex_happy::attach::has_credentials() {
        tx.send(AppEvent::RemoteSessionNotice {
            text: "Codex can mirror this session to the Happy app.".to_string(),
            hint: Some("Run `/remote on` once to connect this machine.".to_string()),
            level: RemoteSessionNoticeLevel::Info,
            open_url: None,
        });
        return;
    }

    // Ensure the per-machine session daemon is running (start-if-absent; never
    // stops/restarts a foreign daemon). Q2: LOUD on failure.
    let supervisor = codex_happy::daemon_supervisor::NodeDaemonSupervisor::with_os_host(home);
    if let Err(err) = supervisor.ensure_running().await {
        tx.send(AppEvent::RemoteSessionNotice {
            text: format!("Happy auto-attach could not start: {err}."),
            hint: Some(
                "Install happy-cli (the per-machine session server), or run `/remote on`."
                    .to_string(),
            ),
            level: RemoteSessionNoticeLevel::Error,
            open_url: None,
        });
        return;
    }

    // Attach via the ONE existing path. `apply_remote_session_toggle` flips
    // `Feature::RemoteSession` on for status coherence (Q1) and attaches loudly
    // via `maybe_attach_reporting` (Q2), so attach failures surface too.
    tx.send(AppEvent::SetRemoteSession { enabled: true });
}
