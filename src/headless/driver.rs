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
