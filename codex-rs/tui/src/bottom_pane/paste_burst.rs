//! Paste-burst detection for terminals without bracketed paste.
//!
//! On some platforms (notably Windows), pastes often arrive as a rapid stream of
//! `KeyCode::Char` and `KeyCode::Enter` key events rather than as a single "paste" event.
//! In that mode, the composer needs to:
//!
//! - Prevent transient UI side effects (e.g. toggles bound to `?`) from triggering on pasted text.
//! - Ensure Enter is treated as a newline *inside the paste*, not as "submit the message".
//! - Avoid slowing the normal typing path while still recovering short prefixes once a stream
//!   proves paste-like.
//!
//! This module provides the `PasteBurst` state machine. `ChatComposer` feeds it only "plain"
//! character events (no Ctrl/Alt) and uses its decisions to either:
//!
//! - insert ordinary typing immediately,
//! - retroactively capture an already-inserted prefix once a stream proves paste-like,
//! - buffer a burst as a single pasted string, or
//! - keep an active burst open across bounded slow terminal delivery.
//!
//! # Call Pattern
//!
//! `PasteBurst` is a pure state machine: it never mutates the textarea directly. The caller feeds
//! it events and then applies the chosen action:
//!
//! - For each plain `KeyCode::Char`, call [`PasteBurst::on_plain_char`] (ASCII) or
//!   [`PasteBurst::on_plain_char_no_hold`] (non-ASCII/IME).
//! - If the decision indicates buffering, the caller removes any requested retro-captured prefix
//!   from its own text buffer and appends to `PasteBurst.buffer` via
//!   [`PasteBurst::append_char_to_buffer`].
//! - On a UI tick, call [`PasteBurst::flush_if_due`]. If it returns [`FlushResult::Paste`], treat
//!   the returned string as an explicit paste.
//! - Before applying a plain char or Enter while a burst is active, call
//!   [`PasteBurst::should_flush_before_burst_input`]. If it returns false, offer the current input
//!   to the burst before applying any idle flush so late-but-bounded continuations can rearm the
//!   burst instead of splitting it.
//! - Before applying non-char input (arrow keys, Ctrl/Alt modifiers, etc.), use
//!   [`PasteBurst::flush_before_modified_input`] to avoid leaving buffered text "stuck", and then
//!   [`PasteBurst::clear_window_after_non_char`] so subsequent typing does not get grouped into a
//!   previous burst.
//!
//! # State Variables
//!
//! This state machine is encoded in a few fields with slightly different meanings:
//!
//! - `active`: true while we are still *actively* accepting characters into the current burst.
//! - `buffer`: accumulated burst text that will eventually flush as a single `Paste(String)`.
//!   A non-empty buffer is treated as "in burst context" even if `active` has been cleared.
//! - `last_plain_char_time`/`consecutive_plain_char_burst`: the timing/count heuristic for
//!   "paste-like" streams.
//! - `burst_window_until`: the Enter suppression window ("Enter inserts newline") that outlives the
//!   buffer itself.
//!
//! # Timing Model
//!
//! There are two timeout concepts:
//!
//! - `PASTE_BURST_CHAR_INTERVAL`: maximum delay between consecutive "plain" chars for them to be
//!   considered part of the initial paste classification window.
//! - `PASTE_BURST_ACTIVE_REARM_TIMEOUT`: once buffering is active, how long a likely-continuation
//!   char/Enter may arrive and still rearm the same burst before the pending paste is flushed.
//!
//! `flush_if_due()` intentionally uses `>` (not `>=`) when comparing elapsed time, so tests and UI
//! ticks should cross the threshold by at least 1ms (see `recommended_flush_delay()`).
//!
//! # Retro Capture Details
//!
//! Retro-capture exists to handle the case where we initially inserted characters as "normal
//! typing", but later decide that the stream is paste-like. When that happens, we retroactively
//! remove a prefix of already-inserted text from the textarea and move it into the burst buffer so
//! the eventual `handle_paste(...)` sees a contiguous pasted string.
//!
//! Retro-capture has two modes:
//!
//! - `Conservative`: used for IME/non-ASCII paths. Short whitespace-free prefixes remain normal
//!   typing unless the prefix looks paste-like.
//! - `ProvenPaste`: used once an ASCII stream or a fast prefix+Enter sequence crosses the paste
//!   threshold. Short prefixes such as `hi` may be removed from the textarea and moved into the
//!   burst buffer.
//!
//! Retro-capture is expressed in terms of characters, not bytes:
//!
//! - `CharDecision::BeginBuffer { retro_chars, capture }` uses `retro_chars` as a character count.
//! - `decide_begin_buffer(now, before_cursor, retro_chars)` turns that into a UTF-8 byte range by
//!   calling `retro_start_index()`.
//! - `RetroGrab.start_byte` is a byte index into the `before_cursor` slice; callers must clamp the
//!   cursor to a char boundary before slicing so `start_byte..cursor` is always valid UTF-8.
//!
//! # Clearing vs Flushing
//!
//! There are two ways callers end burst handling, and they are not interchangeable:
//!
//! - `flush_before_modified_input()` returns buffered text so the caller can apply it through the
//!   normal paste path before handling an unrelated input.
//! - `clear_window_after_non_char()` clears the *classification window* so subsequent typing does
//!   not get grouped into the previous burst. It assumes the caller has already flushed any buffer
//!   because it clears `last_plain_char_time`, which means `flush_if_due()` will not flush a
//!   non-empty buffer until another plain char updates the timestamp.
//!
//! # States (Conceptually)
//!
//! - **Idle**: no buffered text.
//! - **Classifying**: recent plain chars were inserted immediately while we watch timing/count
//!   metadata to decide whether they are paste-like.
//! - **Active buffer**: `active`/`buffer` holds paste-like content until it times out and flushes.
//! - **Enter suppress window**: `burst_window_until` keeps Enter treated as newline briefly after
//!   burst activity so multiline pastes stay grouped.
//!
//! # ASCII vs Non-ASCII
//!
//! - [`PasteBurst::on_plain_char`] never holds the first ASCII char. It returns
//!   [`CharDecision::Insert`] until the stream crosses a paste-like threshold.
//! - [`PasteBurst::on_plain_char_no_hold`] uses conservative retro-capture for IME/non-ASCII paths.
//!
//! # Contract With `ChatComposer`
//!
//! `PasteBurst` does not mutate the UI text buffer on its own. The caller (`ChatComposer`) must
//! interpret decisions and apply the corresponding UI edits:
//!
//! - For each plain ASCII `KeyCode::Char`, call [`PasteBurst::on_plain_char`].
//!   - [`CharDecision::Insert`]: insert the char normally.
//!   - [`CharDecision::BeginBuffer { retro_chars, capture }`]: consider retro-capturing the
//!     already-inserted prefix by calling [`PasteBurst::decide_begin_buffer`]. If it returns
//!     `Some`, remove the returned `start_byte..cursor` range from the textarea and then call
//!     [`PasteBurst::append_char_to_buffer`] for the current char. If it returns `None`, fall back
//!     to normal insertion.
//!   - [`CharDecision::BufferAppend`]: call [`PasteBurst::append_char_to_buffer`].
//!
//! - For each plain non-ASCII `KeyCode::Char`, call [`PasteBurst::on_plain_char_no_hold`] and then:
//!   - If it returns `Some(CharDecision::BufferAppend)`, call
//!     [`PasteBurst::append_char_to_buffer`].
//!   - If it returns `Some(CharDecision::BeginBuffer { retro_chars, capture })`, call
//!     [`PasteBurst::decide_begin_buffer`] as above (and if buffering starts, remove the grabbed
//!     prefix from the textarea and then append the current char to the buffer).
//!   - If it returns `None`, insert normally.
//!
//! - Before applying non-char input (or any input that should not join a burst), call
//!   [`PasteBurst::flush_before_modified_input`] and pass the returned string (if any) through the
//!   normal paste path.
//!
//! - Periodically (e.g. on a UI tick), call [`PasteBurst::flush_if_due`].
//!   - [`FlushResult::Paste`]: treat the returned string as an explicit paste.
//!
//! - When a non-plain key is pressed (Ctrl/Alt-modified input, arrows, etc.), callers should use
//!   [`PasteBurst::clear_window_after_non_char`] to prevent the next keystroke from being
//!   incorrectly grouped into a previous burst.

use std::time::Duration;
use std::time::Instant;

// Heuristic thresholds for detecting paste-like input bursts.
// Detect quickly to limit how much already-inserted prefix may be retro-captured.
const PASTE_BURST_MIN_CHARS: u16 = 3;
const PASTE_ENTER_SUPPRESS_WINDOW: Duration = Duration::from_millis(120);

// Maximum delay between consecutive chars to be considered part of a paste burst.
const PASTE_BURST_CHAR_INTERVAL: Duration = Duration::from_millis(8);

// SANDBOX PATCH: keep classified non-bracketed paste bursts rearmable across bounded slow
// terminal/PTY delivery. Windows Terminal currently has no bracketed-paste event path, so this
// fallback must not split a physical paste just because one chunk is slower than the old 60ms idle
// timeout.
const PASTE_BURST_ACTIVE_REARM_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(crate) struct PasteBurst {
    last_plain_char_time: Option<Instant>,
    consecutive_plain_char_burst: u16,
    consecutive_ascii_plain_char_burst: u16,
    burst_window_until: Option<Instant>,
    buffer: String,
    active: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetroCaptureMode {
    Conservative,
    ProvenPaste,
}

pub(crate) enum CharDecision {
    /// Insert/render this char immediately as normal typing.
    Insert,
    /// Start buffering and retroactively capture some already-inserted chars.
    BeginBuffer {
        retro_chars: u16,
        capture: RetroCaptureMode,
    },
    /// We are currently buffering; append the current char into the buffer.
    BufferAppend,
}

pub(crate) enum EnterDecision {
    /// Enter was appended into an already-active burst.
    Buffered,
    /// Start buffering by retroactively capturing already-inserted chars, then append newline.
    BeginBuffer {
        retro_chars: u16,
        capture: RetroCaptureMode,
    },
    /// Treat Enter as ordinary submit/newline handling.
    Submit,
}

pub(crate) struct RetroGrab {
    pub start_byte: usize,
    pub grabbed: String,
}

pub(crate) enum FlushResult {
    Paste(String),
    None,
}

impl PasteBurst {
    /// Recommended delay to wait between simulated keypresses so ordinary
    /// typing does not remain in the initial burst-classification window.
    ///
    /// Primarily used by tests and by the TUI to reliably cross the
    /// paste-burst timing threshold.
    pub fn recommended_flush_delay() -> Duration {
        PASTE_BURST_CHAR_INTERVAL + Duration::from_millis(1)
    }

    #[cfg(test)]
    pub(crate) fn recommended_active_flush_delay() -> Duration {
        PASTE_BURST_ACTIVE_REARM_TIMEOUT + Duration::from_millis(1)
    }

    /// Entry point: decide how to treat a plain char with current timing.
    pub fn on_plain_char(&mut self, _ch: char, now: Instant) -> CharDecision {
        if self.is_active_internal() {
            self.note_plain_char(now);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return CharDecision::BufferAppend;
        }

        self.note_plain_char(now);
        self.consecutive_ascii_plain_char_burst = self.consecutive_plain_char_burst;

        if self.consecutive_plain_char_burst >= PASTE_BURST_MIN_CHARS {
            return CharDecision::BeginBuffer {
                retro_chars: self.consecutive_plain_char_burst.saturating_sub(1),
                capture: RetroCaptureMode::ProvenPaste,
            };
        }

        CharDecision::Insert
    }

    /// Like on_plain_char(), but never holds the first char.
    ///
    /// Used for non-ASCII input paths (e.g., IMEs) where holding a character can
    /// feel like dropped input, while still allowing burst-based paste detection.
    ///
    /// Note: This method will only ever return BufferAppend or BeginBuffer.
    pub fn on_plain_char_no_hold(&mut self, now: Instant) -> Option<CharDecision> {
        self.note_plain_char(now);
        self.consecutive_ascii_plain_char_burst = 0;

        if self.active {
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return Some(CharDecision::BufferAppend);
        }

        if self.consecutive_plain_char_burst >= PASTE_BURST_MIN_CHARS {
            return Some(CharDecision::BeginBuffer {
                retro_chars: self.consecutive_plain_char_burst.saturating_sub(1),
                capture: RetroCaptureMode::Conservative,
            });
        }

        None
    }

    /// Decide whether Enter should be captured as part of a paste burst.
    ///
    /// Enter can either continue an active burst or prove that a short fast prefix (for example
    /// `h`, `i`, `Enter`) was paste-like. It does not start a burst from a single preceding char.
    pub fn on_enter(&mut self, now: Instant) -> EnterDecision {
        if self.is_active_internal() {
            self.buffer.push('\n');
            self.last_plain_char_time = Some(now);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            return EnterDecision::Buffered;
        }

        let follows_fast_prefix = self
            .last_plain_char_time
            .is_some_and(|t| now.duration_since(t) <= PASTE_BURST_CHAR_INTERVAL);
        if follows_fast_prefix && self.consecutive_ascii_plain_char_burst >= 2 {
            return EnterDecision::BeginBuffer {
                retro_chars: self.consecutive_ascii_plain_char_burst,
                capture: RetroCaptureMode::ProvenPaste,
            };
        }

        EnterDecision::Submit
    }

    fn note_plain_char(&mut self, now: Instant) {
        match self.last_plain_char_time {
            Some(prev) if now.duration_since(prev) <= PASTE_BURST_CHAR_INTERVAL => {
                self.consecutive_plain_char_burst =
                    self.consecutive_plain_char_burst.saturating_add(1)
            }
            _ => self.consecutive_plain_char_burst = 1,
        }
        self.last_plain_char_time = Some(now);
    }

    /// Flushes any buffered burst if the inter-key timeout has elapsed.
    ///
    /// Returns:
    ///
    /// - [`FlushResult::Paste`] when a paste burst was active and buffered text is emitted as one
    ///   pasted string.
    /// - [`FlushResult::None`] when the timeout has not elapsed, or there is nothing to flush.
    pub fn flush_if_due(&mut self, now: Instant) -> FlushResult {
        let timed_out = self
            .last_plain_char_time
            .is_some_and(|t| now.duration_since(t) > PASTE_BURST_ACTIVE_REARM_TIMEOUT);
        if timed_out && self.is_active_internal() {
            self.active = false;
            let out = std::mem::take(&mut self.buffer);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            FlushResult::Paste(out)
        } else {
            FlushResult::None
        }
    }

    /// Returns true when a new plain char/Enter should first close the existing burst.
    pub fn should_flush_before_burst_input(&self, now: Instant) -> bool {
        self.is_active_internal()
            && self
                .last_plain_char_time
                .is_some_and(|t| now.duration_since(t) > PASTE_BURST_ACTIVE_REARM_TIMEOUT)
    }

    /// While bursting: accumulate a newline into the buffer instead of
    /// submitting the textarea.
    ///
    /// Returns true if a newline was appended (we are in a burst context),
    /// false otherwise.
    pub fn append_newline_if_active(&mut self, now: Instant) -> bool {
        if self.is_active() {
            self.buffer.push('\n');
            self.last_plain_char_time = Some(now);
            self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
            true
        } else {
            false
        }
    }

    /// Decide if Enter should insert a newline (burst context) vs submit.
    pub fn newline_should_insert_instead_of_submit(&self, now: Instant) -> bool {
        let in_burst_window = self.burst_window_until.is_some_and(|until| now <= until);
        self.is_active() || in_burst_window
    }

    /// Keep the burst window alive.
    pub fn extend_window(&mut self, now: Instant) {
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    /// Begin buffering with retroactively grabbed text.
    pub fn begin_with_retro_grabbed(&mut self, grabbed: String, now: Instant) {
        if !grabbed.is_empty() {
            self.buffer.push_str(&grabbed);
        }
        self.active = true;
        self.last_plain_char_time = Some(now);
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    /// Append a char into the burst buffer.
    pub fn append_char_to_buffer(&mut self, ch: char, now: Instant) {
        self.buffer.push(ch);
        self.last_plain_char_time = Some(now);
        self.burst_window_until = Some(now + PASTE_ENTER_SUPPRESS_WINDOW);
    }

    /// Try to append a char into the burst buffer only if a burst is already active.
    ///
    /// Returns true when the char was captured into the existing burst, false otherwise.
    pub fn try_append_char_if_active(&mut self, ch: char, now: Instant) -> bool {
        if self.active || !self.buffer.is_empty() {
            self.append_char_to_buffer(ch, now);
            true
        } else {
            false
        }
    }

    /// Decide whether to begin buffering by retroactively capturing recent
    /// chars from the slice before the cursor.
    ///
    /// In conservative mode, if the retro-grabbed slice contains any whitespace or is sufficiently
    /// long (>= 16 characters), treat it as paste-like while not triggering on short IME words. In
    /// proven-paste mode, the caller has already observed a paste-like ASCII threshold or
    /// prefix+Enter sequence, so short prefixes may be captured too.
    ///
    /// Returns Some(RetroGrab) with the start byte and grabbed text when we
    /// decide to buffer retroactively; otherwise None.
    pub fn decide_begin_buffer(
        &mut self,
        now: Instant,
        before: &str,
        retro_chars: usize,
        capture: RetroCaptureMode,
    ) -> Option<RetroGrab> {
        let start_byte = retro_start_index(before, retro_chars);
        let grabbed = before[start_byte..].to_string();
        let looks_pastey =
            grabbed.chars().any(char::is_whitespace) || grabbed.chars().count() >= 16;
        if capture == RetroCaptureMode::ProvenPaste || looks_pastey {
            // Note: caller is responsible for removing this slice from UI text.
            self.begin_with_retro_grabbed(grabbed.clone(), now);
            Some(RetroGrab {
                start_byte,
                grabbed,
            })
        } else {
            None
        }
    }

    /// Before applying modified/non-char input: flush buffered burst immediately.
    pub fn flush_before_modified_input(&mut self) -> Option<String> {
        if !self.is_active() {
            return None;
        }
        self.active = false;
        let out = std::mem::take(&mut self.buffer);
        Some(out)
    }

    /// Clear only the timing window.
    ///
    /// Does not emit or clear the buffered text itself; callers should have
    /// already flushed (if needed) via one of the flush methods above.
    pub fn clear_window_after_non_char(&mut self) {
        self.consecutive_plain_char_burst = 0;
        self.consecutive_ascii_plain_char_burst = 0;
        self.last_plain_char_time = None;
        self.burst_window_until = None;
        self.active = false;
    }

    /// Returns true if we are in any paste-burst related transient state
    /// (actively buffering or have a non-empty buffer).
    pub fn is_active(&self) -> bool {
        self.is_active_internal()
    }

    fn is_active_internal(&self) -> bool {
        self.active || !self.buffer.is_empty()
    }

    pub fn clear_after_explicit_paste(&mut self) {
        self.last_plain_char_time = None;
        self.consecutive_plain_char_burst = 0;
        self.consecutive_ascii_plain_char_burst = 0;
        self.burst_window_until = None;
        self.active = false;
        self.buffer.clear();
    }
}

pub(crate) fn retro_start_index(before: &str, retro_chars: usize) -> usize {
    if retro_chars == 0 {
        return before.len();
    }
    before
        .char_indices()
        .rev()
        .nth(retro_chars.saturating_sub(1))
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Behavior: for ASCII input we insert the first char immediately and do not enter burst state.
    #[test]
    fn ascii_first_char_inserts_immediately_without_burst_state() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(burst.on_plain_char('a', t0), CharDecision::Insert));

        let t1 = t0 + PasteBurst::recommended_flush_delay() + Duration::from_millis(1);
        assert!(matches!(burst.flush_if_due(t1), FlushResult::None));
        assert!(!burst.is_active());
    }

    /// Behavior: once an ASCII stream crosses the paste threshold, the caller may retro-capture the
    /// already-inserted prefix and flush the whole buffered payload as a paste.
    #[test]
    fn ascii_threshold_starts_buffer_with_proven_retro_capture() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(burst.on_plain_char('a', t0), CharDecision::Insert));

        let t1 = t0 + Duration::from_millis(1);
        assert!(matches!(burst.on_plain_char('b', t1), CharDecision::Insert));

        let t2 = t1 + Duration::from_millis(1);
        let CharDecision::BeginBuffer {
            retro_chars,
            capture,
        } = burst.on_plain_char('c', t2)
        else {
            panic!("third fast char should begin buffer");
        };
        assert_eq!(retro_chars, 2);
        assert_eq!(capture, RetroCaptureMode::ProvenPaste);
        let grab = burst
            .decide_begin_buffer(t2, "ab", retro_chars as usize, capture)
            .expect("proven paste should capture short prefix");
        assert_eq!(grab.grabbed, "ab");
        burst.append_char_to_buffer('c', t2);

        let t3 = t2 + PasteBurst::recommended_active_flush_delay() + Duration::from_millis(1);
        assert!(matches!(
            burst.flush_if_due(t3),
            FlushResult::Paste(ref s) if s == "abc"
        ));
    }

    /// Behavior: when non-char input is about to be applied, we flush any buffered burst state
    /// immediately so state doesn't leak across inputs.
    #[test]
    fn flush_before_modified_input_emits_active_buffer() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        burst.begin_with_retro_grabbed("a".to_string(), t0);

        assert_eq!(burst.flush_before_modified_input(), Some("a".to_string()));
        assert!(!burst.is_active());
    }

    /// Behavior: retro-grab buffering is only enabled when the already-inserted prefix looks
    /// paste-like (whitespace or "long enough") so short IME bursts don't get misclassified.
    #[test]
    fn decide_begin_buffer_only_triggers_for_pastey_prefixes() {
        let mut burst = PasteBurst::default();
        let now = Instant::now();

        assert!(
            burst
                .decide_begin_buffer(
                    now,
                    "ab",
                    /*retro_chars*/ 2,
                    RetroCaptureMode::Conservative
                )
                .is_none()
        );
        assert!(!burst.is_active());

        let grab = burst
            .decide_begin_buffer(
                now,
                "a b",
                /*retro_chars*/ 2,
                RetroCaptureMode::Conservative,
            )
            .expect("whitespace should be considered paste-like");
        assert_eq!(grab.start_byte, 1);
        assert_eq!(grab.grabbed, " b");
        assert!(burst.is_active());
    }

    /// Behavior: proven-paste retro capture may grab a short whitespace-free prefix.
    #[test]
    fn proven_paste_retro_capture_accepts_short_prefix() {
        let mut burst = PasteBurst::default();
        let now = Instant::now();

        let grab = burst
            .decide_begin_buffer(
                now,
                "hi",
                /*retro_chars*/ 2,
                RetroCaptureMode::ProvenPaste,
            )
            .expect("proven paste should capture short prefix");
        assert_eq!(grab.start_byte, 0);
        assert_eq!(grab.grabbed, "hi");
        assert!(burst.is_active());
    }

    /// Behavior: Enter after a fast two-char prefix proves a short multiline paste.
    #[test]
    fn enter_after_fast_prefix_starts_burst() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        assert!(matches!(burst.on_plain_char('h', t0), CharDecision::Insert));
        let t1 = t0 + Duration::from_millis(1);
        assert!(matches!(burst.on_plain_char('i', t1), CharDecision::Insert));

        let EnterDecision::BeginBuffer {
            retro_chars,
            capture,
        } = burst.on_enter(t1 + Duration::from_millis(1))
        else {
            panic!("fast prefix + Enter should begin buffer");
        };
        assert_eq!(retro_chars, 2);
        assert_eq!(capture, RetroCaptureMode::ProvenPaste);
    }

    /// Behavior: active bursts accept bounded slow continuations beyond the old Windows 60ms idle
    /// timeout and flush as one paste after the rearm window expires.
    #[test]
    fn active_burst_accepts_slow_bounded_continuation_before_flush() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        burst.begin_with_retro_grabbed("hi".to_string(), t0);

        let t1 = t0 + Duration::from_millis(100);
        assert!(!burst.should_flush_before_burst_input(t1));
        assert!(matches!(
            burst.on_plain_char('!', t1),
            CharDecision::BufferAppend
        ));
        burst.append_char_to_buffer('!', t1);

        let t2 = t1 + PasteBurst::recommended_active_flush_delay() + Duration::from_millis(1);
        assert!(matches!(
            burst.flush_if_due(t2),
            FlushResult::Paste(ref s) if s == "hi!"
        ));
    }

    /// Behavior: after a paste-like burst, we keep an "enter suppression window" alive briefly so
    /// a slightly-late Enter still inserts a newline instead of submitting.
    #[test]
    fn newline_suppression_window_outlives_buffer_flush() {
        let mut burst = PasteBurst::default();
        let t0 = Instant::now();
        burst.begin_with_retro_grabbed("ab".to_string(), t0);

        let t2 = t0 + PasteBurst::recommended_active_flush_delay() + Duration::from_millis(1);
        assert!(matches!(burst.flush_if_due(t2), FlushResult::Paste(ref s) if s == "ab"));
        assert!(!burst.is_active());

        assert!(burst.newline_should_insert_instead_of_submit(t2));
        let t3 = t2 + PASTE_ENTER_SUPPRESS_WINDOW + Duration::from_millis(1);
        assert!(!burst.newline_should_insert_instead_of_submit(t3));
    }
}
