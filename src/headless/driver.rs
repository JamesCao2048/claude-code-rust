//! Headless event-loop state machine. See T8–T13 in the implementation plan.
//!
//! T8/T9 ship the pre-event-loop helpers: permission-mode resolution and
//! prompt-source classification (TTY refusal). The state machine itself
//! (init/connect/streaming/shutdown phases) lands in T10–T13.

/// Outcome of resolving the headless permission mode.
///
/// `Mode { mode, warning }` means the driver should send `mode` to the bridge
/// and, if `warning` is `Some`, write its contents to stderr before any other
/// output. `RefuseExplicitRequired` means the org-CI policy is in force and
/// the user must pass `--permission-mode` (or `--dangerously-skip-permissions`)
/// — the driver should print the design's hint message and exit 64.
#[derive(Debug, PartialEq, Eq)]
pub enum ResolvedPermissionMode {
    Mode {
        mode: &'static str,
        warning: Option<String>,
    },
    RefuseExplicitRequired,
}

/// Resolve the effective headless permission mode from CLI flags + the org-CI
/// env hatch. Pure function — easy to unit-test the §3.4 truth table.
///
/// Precedence (matches design doc §3.4):
/// 1. `--dangerously-skip-permissions` → `bypassPermissions` (no warning).
/// 2. Explicit `--permission-mode X` → `X` (no warning, even when the env hatch
///    is set — explicit beats policy).
/// 3. Neither set, env hatch unset/`0` → `bypassPermissions` with stderr warning.
/// 4. Neither set, env hatch `1` → `RefuseExplicitRequired`.
#[must_use]
pub fn resolve_permission_mode(
    cli_dangerously_skip: bool,
    cli_mode: Option<&str>,
    require_explicit: bool,
) -> ResolvedPermissionMode {
    if cli_dangerously_skip {
        return ResolvedPermissionMode::Mode {
            mode: "bypassPermissions",
            warning: None,
        };
    }
    if let Some(mode) = cli_mode {
        // Leak: caller passes a string borrowed for the program lifetime.
        // Since CliPermissionMode::as_stored returns a `&'static str` already,
        // accept &str and cast via match for the well-known set; otherwise leak.
        let mode_static: &'static str = match mode {
            "default" => "default",
            "auto" => "auto",
            "acceptEdits" => "acceptEdits",
            "plan" => "plan",
            "dontAsk" => "dontAsk",
            "bypassPermissions" => "bypassPermissions",
            other => Box::leak(other.to_owned().into_boxed_str()),
        };
        return ResolvedPermissionMode::Mode {
            mode: mode_static,
            warning: None,
        };
    }
    if require_explicit {
        return ResolvedPermissionMode::RefuseExplicitRequired;
    }
    ResolvedPermissionMode::Mode {
        mode: "bypassPermissions",
        warning: Some(
            "warning: --permission-mode not set; defaulting to bypassPermissions\n\
             warning: set LINGXI_HEADLESS_REQUIRE_EXPLICIT_PERMISSION_MODE=1 to require explicit opt-in"
                .to_owned(),
        ),
    }
}

/// Classification of where the headless driver should obtain the prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum PromptSource {
    /// Caller provided the prompt as a positional argument.
    Provided(String),
    /// No positional, stdin is piped — driver should `read_to_string` from stdin.
    ReadStdin,
    /// No positional and stdin is a TTY — driver must exit 64 with a usage error
    /// rather than silently waiting for the user to type something into a TUI-less
    /// process. Mirrors `claude -p` and protects scripts that lost an argv.
    RefuseTty,
}

/// Decide where to read the prompt based on `prompt` (CLI positional) and
/// whether stdin is currently a TTY. The TTY check is injected as a parameter
/// so this function stays trivially testable.
#[must_use]
pub fn classify_prompt(prompt: Option<&str>, stdin_is_tty: bool) -> PromptSource {
    match (prompt, stdin_is_tty) {
        (Some(p), _) => PromptSource::Provided(p.to_owned()),
        (None, true) => PromptSource::RefuseTty,
        (None, false) => PromptSource::ReadStdin,
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[test]
    fn dangerously_skip_wins_over_everything() {
        let r = resolve_permission_mode(true, None, true);
        assert!(matches!(
            r,
            ResolvedPermissionMode::Mode {
                mode: "bypassPermissions",
                warning: None
            }
        ));
    }

    #[test]
    fn explicit_mode_used_as_is_no_warning() {
        let r = resolve_permission_mode(false, Some("dontAsk"), false);
        let ResolvedPermissionMode::Mode { mode, warning } = r else {
            panic!("expected Mode")
        };
        assert_eq!(mode, "dontAsk");
        assert!(warning.is_none());
    }

    #[test]
    fn explicit_mode_beats_env_hatch() {
        let r = resolve_permission_mode(false, Some("dontAsk"), true);
        let ResolvedPermissionMode::Mode { mode, warning } = r else {
            panic!("expected Mode")
        };
        assert_eq!(mode, "dontAsk");
        assert!(warning.is_none());
    }

    #[test]
    fn neither_set_with_env_unset_warns_and_bypasses() {
        let r = resolve_permission_mode(false, None, false);
        let ResolvedPermissionMode::Mode { mode, warning } = r else {
            panic!("expected Mode")
        };
        assert_eq!(mode, "bypassPermissions");
        let w = warning.expect("warning required when defaulting to bypass");
        assert!(w.contains("defaulting to bypassPermissions"));
        assert!(w.contains("LINGXI_HEADLESS_REQUIRE_EXPLICIT_PERMISSION_MODE"));
    }

    #[test]
    fn neither_set_with_env_set_refuses() {
        assert_eq!(
            resolve_permission_mode(false, None, true),
            ResolvedPermissionMode::RefuseExplicitRequired
        );
    }
}

#[cfg(test)]
mod prompt_source_tests {
    use super::*;

    #[test]
    fn positional_prompt_used_regardless_of_tty() {
        assert_eq!(
            classify_prompt(Some("hi"), true),
            PromptSource::Provided("hi".to_owned())
        );
        assert_eq!(
            classify_prompt(Some("hi"), false),
            PromptSource::Provided("hi".to_owned())
        );
    }

    #[test]
    fn missing_prompt_with_piped_stdin_reads_stdin() {
        assert_eq!(classify_prompt(None, false), PromptSource::ReadStdin);
    }

    #[test]
    fn missing_prompt_with_tty_stdin_refuses() {
        assert_eq!(classify_prompt(None, true), PromptSource::RefuseTty);
    }
}

// ---------------------------------------------------------------------------
// T10: init/connect phases with hardcoded timeouts.
// ---------------------------------------------------------------------------

use crate::agent::types::TerminalReason;
use crate::agent::wire::{BridgeEvent, EventEnvelope};
use std::time::Duration;
use tokio::time::timeout;

/// Init-phase timeout: max wall-clock between bridge spawn and first
/// `Initialized` event. Matches design §2 F3a (15s, not user-tunable).
pub const INIT_TIMEOUT_SECS: u64 = 15;

/// Connect-phase timeout: max wall-clock between sending `connect` and the
/// first `Connected` / `SessionReplaced` event. Matches design §2 F3b.
pub const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Graceful-shutdown grace period before the driver hard-kills the bridge
/// child. T12 wires this in; defined here so the constant block stays together.
pub const SHUTDOWN_TIMEOUT_SECS: u64 = 5;

/// Abstract event source so we can drive `init_phase` / `connect_phase` from a
/// `BridgeClient` in production and from a `VecDeque<EventEnvelope>` in tests
/// without coupling either of them to the other.
///
/// `Ok(Some(env))` — next event delivered.
/// `Ok(None)` — channel closed (EOF). Driver maps this to
/// `HeadlessExit::BridgeClosedUnexpectedly` once we're past the init phase
/// (during init the same EOF still surfaces as `BridgeClosedUnexpectedly`,
/// matching design §2 F5).
/// `Err(_)` — transport error. Treated like EOF for exit-code purposes.
#[async_trait::async_trait]
pub trait EventSource: Send {
    async fn next(&mut self) -> anyhow::Result<Option<EventEnvelope>>;
}

/// Terminal exit states for the headless driver. The variants cover every row
/// of design doc §3.5; `code()` is the canonical mapping that T13 will lock
/// down with golden tests.
#[derive(Debug, PartialEq, Eq, Clone)]
#[must_use]
pub enum HeadlessExit {
    /// `TurnComplete { terminal_reason: Some(Completed) | None }`. Exit 0.
    Completed,
    /// `TurnComplete { terminal_reason: Some(non-Completed) }` or `TurnError`.
    /// Carries the reason so the emitter / stderr can name it. Exit 1.
    NonCompletedTerminal(Option<TerminalReason>),
    /// `permission_request` / `question_request` / `elicitation_request` leaked
    /// past the bypass policy. Exit 2.
    InteractivePromptLeaked,
    /// `AuthRequired` event. Exit 3.
    AuthRequired,
    /// `ConnectionFailed` event, init timeout, or connect timeout. Exit 4.
    ConnectionFailed,
    /// Bridge stdout EOF before any terminal event. Exit 5.
    BridgeClosedUnexpectedly,
    /// `--idle-timeout-secs` watchdog elapsed mid-stream. Exit 124.
    IdleTimeout,
    /// `--timeout-secs` hard watchdog elapsed. Exit 124.
    HardTimeout,
    /// CLI usage error (TTY stdin w/o prompt, `RefuseExplicitRequired`, …).
    /// Exit 64.
    UsageError,
}

impl HeadlessExit {
    /// Return the process exit code for this terminal state. Mapping locked
    /// against design §3.5; T13 covers the table with golden tests.
    #[must_use]
    pub const fn code(&self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::NonCompletedTerminal(_) => 1,
            Self::InteractivePromptLeaked => 2,
            Self::AuthRequired => 3,
            Self::ConnectionFailed => 4,
            Self::BridgeClosedUnexpectedly => 5,
            Self::UsageError => 64,
            Self::IdleTimeout | Self::HardTimeout => 124,
        }
    }
}

/// Wait for the first `Initialized` event from the bridge.
///
/// Returns the matching envelope on success. Maps:
/// - `ConnectionFailed` event → `Err(ConnectionFailed)` (design §2 F3a/F9).
/// - elapsed `INIT_TIMEOUT_SECS` → `Err(ConnectionFailed)` (design §2 F3a).
/// - `Ok(None)` from the source (EOF) → `Err(BridgeClosedUnexpectedly)`
///   (design §2 F5).
/// - source error → `Err(BridgeClosedUnexpectedly)` (treats transport errors
///   the same as EOF; the caller still tears down the child).
/// - any non-`Initialized` / non-`ConnectionFailed` event → ignored, keep
///   waiting until the deadline. Bridge protocol pins `Initialized` first, so
///   this is a defensive belt-and-suspenders.
pub async fn init_phase(src: &mut dyn EventSource) -> Result<EventEnvelope, HeadlessExit> {
    let deadline = Duration::from_secs(INIT_TIMEOUT_SECS);
    loop {
        let next = match timeout(deadline, src.next()).await {
            Ok(inner) => inner,
            Err(_elapsed) => return Err(HeadlessExit::ConnectionFailed),
        };
        match next {
            Ok(Some(env)) => match &env.event {
                BridgeEvent::Initialized { .. } => return Ok(env),
                BridgeEvent::ConnectionFailed { .. } => {
                    return Err(HeadlessExit::ConnectionFailed)
                }
                // Anything else: bridge protocol pins `Initialized` first, so
                // this path is defensive — keep waiting until the deadline.
                _ => {}
            },
            Ok(None) | Err(_) => return Err(HeadlessExit::BridgeClosedUnexpectedly),
        }
    }
}

/// Wait for the first `Connected` / `SessionReplaced` event from the bridge.
///
/// Returns the matching envelope on success. Maps:
/// - `AuthRequired` → `Err(AuthRequired)` (design §2 F6).
/// - `ConnectionFailed` → `Err(ConnectionFailed)` (design §2 F9).
/// - elapsed `CONNECT_TIMEOUT_SECS` → `Err(ConnectionFailed)` (design §2 F3b).
/// - `Ok(None)` / source error → `Err(BridgeClosedUnexpectedly)`.
/// - any other event (e.g. stray `SessionUpdate`) → ignored.
pub async fn connect_phase(src: &mut dyn EventSource) -> Result<EventEnvelope, HeadlessExit> {
    let deadline = Duration::from_secs(CONNECT_TIMEOUT_SECS);
    loop {
        let next = match timeout(deadline, src.next()).await {
            Ok(inner) => inner,
            Err(_elapsed) => return Err(HeadlessExit::ConnectionFailed),
        };
        match next {
            Ok(Some(env)) => match &env.event {
                BridgeEvent::Connected { .. } | BridgeEvent::SessionReplaced { .. } => {
                    return Ok(env)
                }
                BridgeEvent::AuthRequired { .. } => return Err(HeadlessExit::AuthRequired),
                BridgeEvent::ConnectionFailed { .. } => {
                    return Err(HeadlessExit::ConnectionFailed)
                }
                // Stray `SessionUpdate` etc. before connect — keep waiting.
                _ => {}
            },
            Ok(None) | Err(_) => return Err(HeadlessExit::BridgeClosedUnexpectedly),
        }
    }
}

#[cfg(test)]
mod phase_tests {
    use super::*;
    use crate::agent::types::{AgentCapabilities, AuthMethod, CurrentModel, InitializeResult};
    use std::collections::VecDeque;

    /// Mock `EventSource` backed by a queue of `(envelope, sleep_before)`
    /// pairs. The `sleep_before` lets us simulate slow bridges under
    /// `tokio::test(start_paused = true)`. After the queue drains we return
    /// `Ok(None)` (EOF) on every subsequent `next()` call so callers never
    /// hang past the test scenario.
    struct MockSource {
        queue: VecDeque<(Option<EventEnvelope>, Duration)>,
    }

    impl MockSource {
        fn new(items: Vec<(Option<EventEnvelope>, Duration)>) -> Self {
            Self {
                queue: items.into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl EventSource for MockSource {
        async fn next(&mut self) -> anyhow::Result<Option<EventEnvelope>> {
            match self.queue.pop_front() {
                Some((evt, sleep)) => {
                    if !sleep.is_zero() {
                        tokio::time::sleep(sleep).await;
                    }
                    Ok(evt)
                }
                None => Ok(None),
            }
        }
    }

    fn sample_init_result() -> InitializeResult {
        InitializeResult {
            agent_name: "lingxi".into(),
            agent_version: "0.0.0-test".into(),
            auth_methods: Vec::<AuthMethod>::new(),
            capabilities: AgentCapabilities {
                prompt_image: false,
                prompt_embedded_context: false,
                supports_session_listing: false,
                supports_resume_session: false,
            },
        }
    }

    fn initialized_envelope() -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::Initialized {
                result: sample_init_result(),
            },
        }
    }

    fn connection_failed_envelope() -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::ConnectionFailed {
                message: "boom".into(),
            },
        }
    }

    fn auth_required_envelope() -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::AuthRequired {
                method_name: "claude_oauth".into(),
                method_description: "log in".into(),
            },
        }
    }

    fn connected_envelope() -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::Connected {
                session_id: "s1".into(),
                cwd: "/tmp".into(),
                current_model: CurrentModel {
                    requested_id: None,
                    resolved_id: "claude".into(),
                    display_name_short: "Claude".into(),
                    display_name_long: "Claude".into(),
                    catalog_id: None,
                    supports_effort: false,
                    supported_effort_levels: Vec::new(),
                    supports_fast_mode: None,
                    supports_auto_mode: None,
                    supports_adaptive_thinking: None,
                    is_authoritative: true,
                },
                available_models: Vec::new(),
                mode: None,
                history_updates: None,
            },
        }
    }

    // ---- init_phase ----

    #[tokio::test(start_paused = true)]
    async fn init_phase_returns_envelope_when_initialized_arrives_in_time() {
        let mut src = MockSource::new(vec![(
            Some(initialized_envelope()),
            Duration::from_secs(2),
        )]);
        let env = init_phase(&mut src).await.expect("init should succeed");
        assert!(matches!(env.event, BridgeEvent::Initialized { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn init_phase_times_out_after_15s_silence() {
        // No event ever delivered: source sleeps 60s before yielding nothing.
        let mut src = MockSource::new(vec![(None, Duration::from_secs(60))]);
        let err = init_phase(&mut src).await.expect_err("must time out");
        assert_eq!(err, HeadlessExit::ConnectionFailed);
        assert_eq!(err.code(), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn init_phase_maps_connection_failed_to_exit_4() {
        let mut src = MockSource::new(vec![(
            Some(connection_failed_envelope()),
            Duration::ZERO,
        )]);
        let err = init_phase(&mut src).await.expect_err("must fail");
        assert_eq!(err, HeadlessExit::ConnectionFailed);
    }

    #[tokio::test(start_paused = true)]
    async fn init_phase_eof_before_initialized_is_bridge_closed() {
        // Empty queue → MockSource immediately returns Ok(None).
        let mut src = MockSource::new(Vec::new());
        let err = init_phase(&mut src).await.expect_err("must fail");
        assert_eq!(err, HeadlessExit::BridgeClosedUnexpectedly);
        assert_eq!(err.code(), 5);
    }

    // ---- connect_phase ----

    #[tokio::test(start_paused = true)]
    async fn connect_phase_returns_connected_in_time() {
        let mut src = MockSource::new(vec![(
            Some(connected_envelope()),
            Duration::from_secs(5),
        )]);
        let env = connect_phase(&mut src).await.expect("connect should succeed");
        assert!(matches!(env.event, BridgeEvent::Connected { .. }));
    }

    #[tokio::test(start_paused = true)]
    async fn connect_phase_auth_required_maps_to_exit_3() {
        let mut src = MockSource::new(vec![(
            Some(auth_required_envelope()),
            Duration::ZERO,
        )]);
        let err = connect_phase(&mut src).await.expect_err("must fail");
        assert_eq!(err, HeadlessExit::AuthRequired);
        assert_eq!(err.code(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_phase_eof_before_connected_is_bridge_closed() {
        let mut src = MockSource::new(Vec::new());
        let err = connect_phase(&mut src).await.expect_err("must fail");
        assert_eq!(err, HeadlessExit::BridgeClosedUnexpectedly);
    }
}

// ---------------------------------------------------------------------------
// T11: streaming-phase loop with watchdog + emitter + prompt-leak detection.
// ---------------------------------------------------------------------------

use crate::headless::output::Emitter;
use crate::headless::watchdog::{Watchdog, WatchdogTick};

/// Side-channel back to the bridge that the streaming loop uses when it must
/// abort an in-flight turn (interactive prompt leaked, idle/hard watchdog
/// expired). Kept narrow — only `cancel_turn` is needed by T11. T12 will
/// broaden the trait to cover `shutdown` / `force_kill_and_wait`.
#[async_trait::async_trait]
pub trait Sender: Send {
    /// Best-effort cancellation. Errors are intentionally ignored by the
    /// streaming loop: we're already on a terminal path and the bridge is
    /// about to be torn down regardless.
    async fn cancel_turn(&mut self, session_id: &str) -> anyhow::Result<()>;
}

/// Drive the streaming phase: pump events through the emitter, reset the idle
/// watchdog on every event, intercept interactive-prompt leaks, and translate
/// terminal events / timeouts into a [`HeadlessExit`].
///
/// The loop is `tokio::select!` with a `biased;` policy so a watchdog firing
/// in the same poll as a queued event still gives the event a chance —
/// matters under paused-time tests where multiple wake-ups land at once.
///
/// Non-fatal events (`SlashError`, `RuntimeReloadFailed`, all session
/// updates, init / connect echoes) are emitted and the loop continues. The
/// terminal events produce a `HeadlessExit` per design §3.5:
///
/// | Event                                        | HeadlessExit                            |
/// |----------------------------------------------|-----------------------------------------|
/// | `TurnComplete{Completed|None}`               | `Completed`                             |
/// | `TurnComplete{other}`                        | `NonCompletedTerminal(Some(other))`     |
/// | `TurnError`                                  | `NonCompletedTerminal(reason or None)`  |
/// | `PermissionRequest` / `QuestionRequest` / `ElicitationRequest` | `InteractivePromptLeaked` (cancel_turn first; emit skipped) |
/// | `AuthRequired`                               | `AuthRequired`                          |
/// | `ConnectionFailed`                           | `ConnectionFailed`                      |
/// | EOF / source error                           | `BridgeClosedUnexpectedly`              |
/// | `WatchdogTick::IdleExpired`                  | `IdleTimeout` (cancel_turn first)       |
/// | `WatchdogTick::HardExpired`                  | `HardTimeout` (cancel_turn first)       |
pub async fn streaming_phase(
    src: &mut dyn EventSource,
    sender: &mut dyn Sender,
    emitter: &mut dyn Emitter,
    watchdog: &mut Watchdog,
    session_id: &str,
) -> HeadlessExit {
    loop {
        tokio::select! {
            biased;
            ev = src.next() => {
                match ev {
                    Ok(Some(envelope)) => {
                        watchdog.note_activity();
                        match envelope.event {
                            // Interactive prompts: cancel best-effort and bail.
                            // The emit is intentionally skipped (design §2 F8) —
                            // these events are sensitive (raw tool I/O,
                            // free-form question text) and would only confuse a
                            // non-interactive consumer that has nowhere to
                            // forward them.
                            BridgeEvent::PermissionRequest { .. }
                            | BridgeEvent::QuestionRequest { .. }
                            | BridgeEvent::ElicitationRequest { .. } => {
                                let _ = sender.cancel_turn(session_id).await;
                                return HeadlessExit::InteractivePromptLeaked;
                            }
                            BridgeEvent::TurnComplete { terminal_reason, .. } => {
                                let _ = emitter.emit_event(&envelope.event);
                                return match terminal_reason {
                                    Some(TerminalReason::Completed) | None => {
                                        HeadlessExit::Completed
                                    }
                                    Some(other) => {
                                        HeadlessExit::NonCompletedTerminal(Some(other))
                                    }
                                };
                            }
                            BridgeEvent::TurnError { terminal_reason, .. } => {
                                let _ = emitter.emit_event(&envelope.event);
                                return HeadlessExit::NonCompletedTerminal(terminal_reason);
                            }
                            BridgeEvent::AuthRequired { .. } => {
                                let _ = emitter.emit_event(&envelope.event);
                                return HeadlessExit::AuthRequired;
                            }
                            BridgeEvent::ConnectionFailed { .. } => {
                                let _ = emitter.emit_event(&envelope.event);
                                return HeadlessExit::ConnectionFailed;
                            }
                            // Non-fatal — emit and continue.
                            // SlashError / RuntimeReloadFailed are explicitly
                            // non-terminal per design §2 F8; SessionUpdate /
                            // Connected / Initialized echoes flow through here
                            // too.
                            _ => {
                                let _ = emitter.emit_event(&envelope.event);
                            }
                        }
                    }
                    Ok(None) | Err(_) => return HeadlessExit::BridgeClosedUnexpectedly,
                }
            }
            tick = watchdog.tick() => {
                match tick {
                    WatchdogTick::IdleExpired => {
                        let _ = sender.cancel_turn(session_id).await;
                        return HeadlessExit::IdleTimeout;
                    }
                    WatchdogTick::HardExpired => {
                        let _ = sender.cancel_turn(session_id).await;
                        return HeadlessExit::HardTimeout;
                    }
                    WatchdogTick::Continue => {
                        // Spurious wake — keep looping. `Watchdog::tick` only
                        // returns `Continue` when both deadlines are still in
                        // the future; defensive in case the timer wakes early.
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod streaming_tests {
    use super::*;
    use crate::agent::types::{ContentBlock, SessionUpdate};
    use crate::headless::output::Emitter;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Mock `EventSource` for streaming tests. Mirrors `phase_tests::MockSource`
    /// but lives in this module so we can extend it without disturbing T10's
    /// fixtures. Each item is `(Option<EventEnvelope>, sleep_before)`; after
    /// the queue drains, every subsequent `next()` returns `Ok(None)`.
    struct MockEventSource {
        queue: VecDeque<(Option<EventEnvelope>, Duration)>,
    }

    impl MockEventSource {
        fn new(items: Vec<(Option<EventEnvelope>, Duration)>) -> Self {
            Self { queue: items.into() }
        }
    }

    #[async_trait::async_trait]
    impl EventSource for MockEventSource {
        async fn next(&mut self) -> anyhow::Result<Option<EventEnvelope>> {
            match self.queue.pop_front() {
                Some((evt, sleep)) => {
                    if !sleep.is_zero() {
                        tokio::time::sleep(sleep).await;
                    }
                    Ok(evt)
                }
                None => Ok(None),
            }
        }
    }

    /// Records every `cancel_turn` call so tests can assert the exact session
    /// id and call count. `Arc<Mutex<…>>` keeps the recorder shareable: tests
    /// hold a clone for assertions while the sender owns one for writes.
    #[derive(Clone, Default)]
    struct MockSender {
        cancels: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Sender for MockSender {
        async fn cancel_turn(&mut self, session_id: &str) -> anyhow::Result<()> {
            self.cancels.lock().unwrap().push(session_id.to_owned());
            Ok(())
        }
    }

    /// Records every `emit_event` call by `event_name` for ordering assertions.
    /// `emit_warning` / `emit_error` / `finish` are no-ops; the streaming loop
    /// never calls them today.
    #[derive(Clone, Default)]
    struct MockEmitter {
        events: Arc<Mutex<Vec<String>>>,
    }

    impl Emitter for MockEmitter {
        fn emit_event(&mut self, ev: &BridgeEvent) -> std::io::Result<()> {
            self.events.lock().unwrap().push(ev.event_name().to_owned());
            Ok(())
        }
        fn emit_warning(&mut self, _msg: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn emit_error(&mut self, _msg: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn finish(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn agent_message_envelope(text: &str) -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::SessionUpdate {
                session_id: "s1".into(),
                update: SessionUpdate::AgentMessageChunk {
                    content: ContentBlock::Text { text: text.into() },
                },
            },
        }
    }

    fn turn_complete_envelope(reason: Option<TerminalReason>) -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::TurnComplete {
                session_id: "s1".into(),
                terminal_reason: reason,
            },
        }
    }

    fn turn_error_envelope(reason: Option<TerminalReason>) -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::TurnError {
                session_id: "s1".into(),
                message: "boom".into(),
                error_kind: None,
                sdk_result_subtype: None,
                assistant_error: None,
                terminal_reason: reason,
            },
        }
    }

    fn slash_error_envelope() -> EventEnvelope {
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::SlashError {
                session_id: "s1".into(),
                message: "bad slash".into(),
            },
        }
    }

    fn permission_request_envelope() -> EventEnvelope {
        use crate::agent::types::{PermissionOption, PermissionRequest, ToolCall};
        EventEnvelope {
            request_id: None,
            event: BridgeEvent::PermissionRequest {
                session_id: "s1".into(),
                request: PermissionRequest {
                    tool_call: ToolCall {
                        tool_call_id: "tc1".into(),
                        title: "ls".into(),
                        kind: "BashTool".into(),
                        status: "in_progress".into(),
                        content: Vec::new(),
                        raw_input: None,
                        raw_output: None,
                        output_metadata: None,
                        task_metadata: None,
                        locations: Vec::new(),
                        meta: None,
                    },
                    options: vec![PermissionOption {
                        option_id: "allow".into(),
                        name: "Allow".into(),
                        description: None,
                        kind: "allow_once".into(),
                    }],
                    display: None,
                },
            },
        }
    }

    // ---- happy path & terminal events ----

    #[tokio::test(start_paused = true)]
    async fn happy_path_chunk_then_complete_returns_completed() {
        let mut src = MockEventSource::new(vec![
            (Some(agent_message_envelope("hi")), Duration::ZERO),
            (
                Some(turn_complete_envelope(Some(TerminalReason::Completed))),
                Duration::ZERO,
            ),
        ]);
        let mut sender = MockSender::default();
        let mut emitter = MockEmitter::default();
        let mut wd = Watchdog::new(Duration::from_secs(60), None);

        let exit = streaming_phase(&mut src, &mut sender, &mut emitter, &mut wd, "s1").await;

        assert_eq!(exit, HeadlessExit::Completed);
        let events = emitter.events.lock().unwrap().clone();
        assert_eq!(events, vec!["session_update", "turn_complete"]);
        assert!(
            sender.cancels.lock().unwrap().is_empty(),
            "happy path must not call cancel_turn"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn permission_request_returns_prompt_leaked_and_cancels() {
        let mut src = MockEventSource::new(vec![(
            Some(permission_request_envelope()),
            Duration::ZERO,
        )]);
        let mut sender = MockSender::default();
        let mut emitter = MockEmitter::default();
        let mut wd = Watchdog::new(Duration::from_secs(60), None);

        let exit = streaming_phase(&mut src, &mut sender, &mut emitter, &mut wd, "s1").await;

        assert_eq!(exit, HeadlessExit::InteractivePromptLeaked);
        assert!(
            emitter.events.lock().unwrap().is_empty(),
            "interactive prompt must not be emitted"
        );
        let cancels = sender.cancels.lock().unwrap().clone();
        assert_eq!(cancels, vec!["s1".to_owned()]);
    }

    #[tokio::test(start_paused = true)]
    async fn turn_error_returns_non_completed_terminal_with_reason() {
        let mut src = MockEventSource::new(vec![(
            Some(turn_error_envelope(None)),
            Duration::ZERO,
        )]);
        let mut sender = MockSender::default();
        let mut emitter = MockEmitter::default();
        let mut wd = Watchdog::new(Duration::from_secs(60), None);

        let exit = streaming_phase(&mut src, &mut sender, &mut emitter, &mut wd, "s1").await;

        assert_eq!(exit, HeadlessExit::NonCompletedTerminal(None));
        assert_eq!(exit.code(), 1);
        let events = emitter.events.lock().unwrap().clone();
        assert_eq!(events, vec!["turn_error"]);
    }

    #[tokio::test(start_paused = true)]
    async fn slash_error_is_non_fatal_and_loop_continues() {
        let mut src = MockEventSource::new(vec![
            (Some(slash_error_envelope()), Duration::ZERO),
            (
                Some(turn_complete_envelope(Some(TerminalReason::Completed))),
                Duration::ZERO,
            ),
        ]);
        let mut sender = MockSender::default();
        let mut emitter = MockEmitter::default();
        let mut wd = Watchdog::new(Duration::from_secs(60), None);

        let exit = streaming_phase(&mut src, &mut sender, &mut emitter, &mut wd, "s1").await;

        assert_eq!(exit, HeadlessExit::Completed);
        let events = emitter.events.lock().unwrap().clone();
        assert_eq!(events, vec!["slash_error", "turn_complete"]);
        assert!(sender.cancels.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn eof_after_chunk_is_bridge_closed_unexpectedly() {
        let mut src = MockEventSource::new(vec![(
            Some(agent_message_envelope("partial")),
            Duration::ZERO,
        )]);
        let mut sender = MockSender::default();
        let mut emitter = MockEmitter::default();
        let mut wd = Watchdog::new(Duration::from_secs(60), None);

        let exit = streaming_phase(&mut src, &mut sender, &mut emitter, &mut wd, "s1").await;

        assert_eq!(exit, HeadlessExit::BridgeClosedUnexpectedly);
        assert_eq!(exit.code(), 5);
    }

    #[tokio::test(start_paused = true)]
    async fn idle_watchdog_expiration_cancels_turn_and_returns_idle_timeout() {
        // Source never yields — only the watchdog wakes us up.
        let mut src = MockEventSource::new(vec![(None, Duration::from_secs(3600))]);
        let mut sender = MockSender::default();
        let mut emitter = MockEmitter::default();
        let mut wd = Watchdog::new(Duration::from_secs(2), None);

        let exit = streaming_phase(&mut src, &mut sender, &mut emitter, &mut wd, "s1").await;

        assert_eq!(exit, HeadlessExit::IdleTimeout);
        assert_eq!(exit.code(), 124);
        let cancels = sender.cancels.lock().unwrap().clone();
        assert_eq!(cancels, vec!["s1".to_owned()]);
        assert!(
            emitter.events.lock().unwrap().is_empty(),
            "watchdog path must not emit events"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn turn_complete_max_turns_returns_non_completed_terminal_max_turns() {
        let mut src = MockEventSource::new(vec![(
            Some(turn_complete_envelope(Some(TerminalReason::MaxTurns))),
            Duration::ZERO,
        )]);
        let mut sender = MockSender::default();
        let mut emitter = MockEmitter::default();
        let mut wd = Watchdog::new(Duration::from_secs(60), None);

        let exit = streaming_phase(&mut src, &mut sender, &mut emitter, &mut wd, "s1").await;

        assert_eq!(
            exit,
            HeadlessExit::NonCompletedTerminal(Some(TerminalReason::MaxTurns))
        );
        let events = emitter.events.lock().unwrap().clone();
        assert_eq!(events, vec!["turn_complete"]);
        assert!(sender.cancels.lock().unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------
// T12: shutdown sequence.
//
// `BridgeClient::shutdown_with_grace` (in `agent/client.rs`) does the actual
// work — sends `Shutdown`, waits up to `grace`, then `SIGKILL`s on timeout.
// Driver-side this is a one-liner; we keep it here so callers don't need to
// reach into `agent::client` themselves and so future shutdown logic (extra
// teardown signals, telemetry) can land in one spot.
//
// The hang-after-result regression (design §9 test 3) is covered by the T18
// integration test against the stub bridge — unit-testing the timeout path
// in isolation would mean abstracting `tokio::process::Child`, which is a lot
// of trait machinery for no extra signal beyond what T18 already provides.
// ---------------------------------------------------------------------------

/// Tear the bridge down. Sends `Shutdown`, waits up to `SHUTDOWN_TIMEOUT_SECS`
/// for graceful exit, then hard-kills.
pub async fn shutdown_phase(
    client: crate::agent::client::BridgeClient,
) -> anyhow::Result<std::process::ExitStatus> {
    client
        .shutdown_with_grace(std::time::Duration::from_secs(SHUTDOWN_TIMEOUT_SECS))
        .await
}

// ---------------------------------------------------------------------------
// T13: exit-code mapping golden tests.
//
// Locks `HeadlessExit::code()` against design §3.5. If a future change adds a
// new variant or shifts a code, these tests force the change to be deliberate.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod exit_code_tests {
    use super::*;

    #[test]
    fn completed_exits_zero() {
        assert_eq!(HeadlessExit::Completed.code(), 0);
    }

    #[test]
    fn non_completed_terminal_exits_one_for_every_terminal_reason() {
        // Cover every TerminalReason variant; all non-Completed reasons → 1.
        let reasons = [
            TerminalReason::BlockingLimit,
            TerminalReason::RapidRefillBreaker,
            TerminalReason::PromptTooLong,
            TerminalReason::ImageError,
            TerminalReason::ModelError,
            TerminalReason::AbortedStreaming,
            TerminalReason::AbortedTools,
            TerminalReason::StopHookPrevented,
            TerminalReason::HookStopped,
            TerminalReason::ToolDeferred,
            TerminalReason::MaxTurns,
        ];
        for r in reasons {
            assert_eq!(
                HeadlessExit::NonCompletedTerminal(Some(r)).code(),
                1,
                "{r:?} should map to exit 1"
            );
        }
        // Bare TurnError with no reason: also 1.
        assert_eq!(HeadlessExit::NonCompletedTerminal(None).code(), 1);
    }

    #[test]
    fn interactive_prompt_leak_exits_two() {
        assert_eq!(HeadlessExit::InteractivePromptLeaked.code(), 2);
    }

    #[test]
    fn auth_required_exits_three() {
        assert_eq!(HeadlessExit::AuthRequired.code(), 3);
    }

    #[test]
    fn connection_failed_exits_four() {
        // Same code for live ConnectionFailed event AND init/connect timeouts —
        // the driver maps the timeouts onto this variant in init/connect_phase.
        assert_eq!(HeadlessExit::ConnectionFailed.code(), 4);
    }

    #[test]
    fn bridge_eof_exits_five() {
        assert_eq!(HeadlessExit::BridgeClosedUnexpectedly.code(), 5);
    }

    #[test]
    fn usage_error_exits_64() {
        assert_eq!(HeadlessExit::UsageError.code(), 64);
    }

    #[test]
    fn idle_and_hard_timeout_both_exit_124() {
        assert_eq!(HeadlessExit::IdleTimeout.code(), 124);
        assert_eq!(HeadlessExit::HardTimeout.code(), 124);
    }
}
