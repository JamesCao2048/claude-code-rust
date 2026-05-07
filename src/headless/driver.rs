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
