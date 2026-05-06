// Copyright 2025 Simon Peter Rothgang
// SPDX-License-Identifier: Apache-2.0

pub mod agent;
pub mod app;
pub mod embedded_resources;
pub mod error;
pub mod logging;
pub mod perf;
pub mod ui;

/// Inject `env` overrides from `~/.claude/settings.json` and the project-local
/// `.claude/settings.local.json` into the current process environment.
///
/// Mirrors Claude Code (Node) launcher behavior so that values like `ANTHROPIC_AUTH_TOKEN`,
/// `ANTHROPIC_BASE_URL`, and model overrides configured in `settings.json` reach both this
/// process (e.g. `has_credentials()`) and the spawned Node bridge (which reads `process.env`).
///
/// Existing environment variables already exported by the shell take precedence — settings.json
/// only fills in the gaps. Returns the keys that were actually injected, for logging.
///
/// # Safety
/// Must be called before any other thread is spawned (i.e. very early in `main`). `set_var`
/// is not thread-safe on Unix.
pub unsafe fn apply_settings_env_overrides() -> Vec<String> {
    let overrides = crate::app::config::store::load_settings_env_overrides();
    let mut applied = Vec::new();
    for (key, value) in overrides {
        if std::env::var_os(&key).is_some() {
            continue;
        }
        // SAFETY: caller guarantees single-threaded context.
        unsafe {
            std::env::set_var(&key, &value);
        }
        applied.push(key);
    }
    applied
}

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, ValueEnum, PartialEq, Eq)]
pub enum CliPermissionMode {
    #[value(name = "default")]
    Default,
    #[value(name = "auto")]
    Auto,
    #[value(name = "acceptEdits", alias = "accept-edits")]
    AcceptEdits,
    #[value(name = "plan")]
    Plan,
    #[value(name = "dontAsk", alias = "dont-ask")]
    DontAsk,
    #[value(name = "bypassPermissions", alias = "bypass-permissions")]
    BypassPermissions,
}

impl CliPermissionMode {
    #[must_use]
    pub const fn as_stored(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Auto => "auto",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::DontAsk => "dontAsk",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

#[derive(Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum DiagnosticsPreset {
    Runtime,
    Session,
    Render,
    Bridge,
    Full,
}

impl DiagnosticsPreset {
    #[must_use]
    pub fn filter_directives(&self) -> &'static str {
        match self {
            Self::Runtime => {
                "info,bridge.lifecycle=debug,bridge.protocol=debug,app.session=debug,app.tool=debug,app.command=debug,app.permission=debug,app.network=debug,app.update=debug"
            }
            Self::Session => {
                "info,bridge.lifecycle=debug,bridge.protocol=debug,app.session=debug,app.permission=debug,app.command=debug"
            }
            Self::Render => {
                "info,app.render=trace,app.cache=debug,app.input=debug,app.paste=debug,app.perf=info"
            }
            Self::Bridge => {
                "info,bridge.lifecycle=debug,bridge.protocol=debug,bridge.sdk=debug,bridge.permission=debug,bridge.mcp=debug"
            }
            Self::Full => {
                "info,app.render=trace,app.perf=info,bridge.lifecycle=debug,bridge.protocol=debug,bridge.sdk=debug,bridge.permission=debug,bridge.mcp=debug,app.session=debug,app.tool=debug,app.command=debug,app.permission=debug,app.network=debug,app.update=debug,app.cache=debug,app.input=debug,app.paste=debug,app.config=debug,app.auth=debug"
            }
        }
    }
}

#[derive(Parser, Debug)]
#[command(name = "lingxi-ascendc", about = "Lingxi AscendC Operator Development Tool")]
#[command(
    after_help = "Examples:\n  lingxi-ascendc --enable-logs --diagnostics-preset session\n  lingxi-ascendc --enable-logs --diagnostics-preset render\n  lingxi-ascendc --features perf --enable-logs --enable-perf --diagnostics-preset full"
)]
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Disable startup update checks.
    #[arg(long)]
    pub no_update_check: bool,

    /// Working directory (defaults to cwd)
    #[arg(long, short = 'C')]
    pub dir: Option<std::path::PathBuf>,

    /// Path to the agent bridge script (defaults to agent-sdk/dist/bridge.js).
    #[arg(long)]
    pub bridge_script: Option<std::path::PathBuf>,

    /// Enable runtime diagnostics using a default log path when `--log-file` is omitted.
    #[arg(long)]
    pub enable_logs: bool,

    /// Named diagnostics preset for common logging workflows.
    /// Ignored when `--log-filter` is provided explicitly.
    #[arg(long, value_enum)]
    pub diagnostics_preset: Option<DiagnosticsPreset>,

    /// Write tracing diagnostics to a file.
    ///
    /// When omitted but logging is otherwise enabled via `--enable-logs`,
    /// `--diagnostics-preset`, `--log-filter`, `--log-append`, or `RUST_LOG`,
    /// a default log path is used.
    #[arg(long, value_name = "PATH")]
    pub log_file: Option<std::path::PathBuf>,

    /// Tracing filter directives (example: `info,app.render=trace`).
    /// Overrides `--diagnostics-preset` and falls back to `RUST_LOG` when omitted.
    #[arg(long, value_name = "FILTER")]
    pub log_filter: Option<String>,

    /// Append to the active log file instead of resetting the current log window on startup.
    #[arg(long)]
    pub log_append: bool,

    /// Enable perf telemetry using a default sidecar path when `--perf-log` is omitted.
    /// Requires a binary built with `--features perf`.
    #[arg(long)]
    pub enable_perf: bool,

    /// Write high-frequency perf telemetry to a sidecar JSON file (requires `--features perf` build).
    #[arg(long, value_name = "PATH")]
    pub perf_log: Option<std::path::PathBuf>,

    /// Append to `--perf-log` instead of truncating on startup.
    #[arg(long)]
    pub perf_append: bool,

    /// Override the startup permission mode. Takes precedence over settings.json.
    /// Values: default, auto, acceptEdits, plan, dontAsk, bypassPermissions.
    #[arg(long, value_name = "MODE", value_enum)]
    pub permission_mode: Option<CliPermissionMode>,

    /// Skip all permission prompts. Equivalent to `--permission-mode bypassPermissions`.
    #[arg(long, conflicts_with = "permission_mode")]
    pub dangerously_skip_permissions: bool,
}

impl Cli {
    /// Resolve the effective startup permission mode override from CLI flags.
    /// Returns `None` when neither flag is set and the value should fall back to settings.json.
    #[must_use]
    pub fn resolved_permission_mode(&self) -> Option<CliPermissionMode> {
        if self.dangerously_skip_permissions {
            return Some(CliPermissionMode::BypassPermissions);
        }
        self.permission_mode
    }
}

#[derive(Subcommand, Debug, PartialEq, Eq)]
pub enum Command {
    /// Resume a previous session by ID, or pick from recent sessions
    Resume {
        /// Session ID to resume directly. Omit to show a session picker.
        session_id: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::{Cli, Command};
    use clap::Parser;

    #[test]
    fn cli_without_subcommand_starts_new_session() {
        let cli = Cli::try_parse_from(["lingxi-ascendc"]).expect("parse");
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_resume_without_id_requests_picker() {
        let cli = Cli::try_parse_from(["lingxi-ascendc", "resume"]).expect("parse");
        assert_eq!(cli.command, Some(Command::Resume { session_id: None }));
    }

    #[test]
    fn cli_resume_with_id_resumes_directly() {
        let cli = Cli::try_parse_from(["lingxi-ascendc", "resume", "abc-123"]).expect("parse");
        assert_eq!(cli.command, Some(Command::Resume { session_id: Some("abc-123".to_owned()) }));
    }

    #[test]
    fn cli_rejects_legacy_resume_flag() {
        assert!(Cli::try_parse_from(["lingxi-ascendc", "--resume", "abc-123"]).is_err());
    }
}
