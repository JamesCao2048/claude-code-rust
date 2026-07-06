//! Non-interactive (`-p` / headless) mode entry point.
//!
//! Design: `docs/headless/design-headless-mode.md`.
//!
//! [`run_print`] orchestrates the full headless pipeline:
//!   resolve permission mode → classify prompt source (refusing TTY-without-prompt)
//!   → spawn bridge → init/connect/stream/shutdown → translate to exit code.
pub mod driver;
pub mod output;
pub mod watchdog;

use crate::agent::client::{BridgeClient, BridgeReader, BridgeWriter};
use crate::agent::wire::{BridgeCommand, BridgeEvent, CommandEnvelope, SessionLaunchSettings};
use crate::headless::driver::{
    HeadlessExit, PromptSource, ResolvedPermissionMode, classify_prompt, connect_phase, init_phase,
    resolve_permission_mode, shutdown_phase, streaming_phase,
};
use crate::headless::output::{Emitter, StreamJsonEmitter, TextEmitter};
use crate::headless::watchdog::Watchdog;
use crate::{CliPermissionMode, PrintArgs, PrintOutputFormat};
use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Read as _};
use std::path::Path;
use std::time::Duration;

const ENV_REQUIRE_EXPLICIT_PERMISSION_MODE: &str =
    "LINGXI_HEADLESS_REQUIRE_EXPLICIT_PERMISSION_MODE";

/// Run the headless `print` command end-to-end. Returns the process exit code.
///
/// `cwd` is the resolved workspace directory (already validated by main.rs).
/// `bridge_script` is the optional `--bridge-script` override.
/// `cli_permission_mode` / `cli_dangerously_skip` come from the top-level
/// `Cli` flags (we don't pass the whole `Cli` to keep the signature
/// independent of TUI-only fields).
pub async fn run_print(
    args: &PrintArgs,
    cwd: &Path,
    bridge_script: Option<&Path>,
    cli_permission_mode: Option<CliPermissionMode>,
    cli_dangerously_skip: bool,
) -> i32 {
    let require_explicit =
        std::env::var(ENV_REQUIRE_EXPLICIT_PERMISSION_MODE).map(|v| v == "1").unwrap_or(false);

    let permission_mode = match resolve_permission_mode(
        cli_dangerously_skip,
        cli_permission_mode.map(CliPermissionMode::as_stored),
        require_explicit,
    ) {
        ResolvedPermissionMode::RefuseExplicitRequired => {
            eprintln!(
                "error: --permission-mode required by org policy ({ENV_REQUIRE_EXPLICIT_PERMISSION_MODE}=1)"
            );
            eprintln!("hint: pass --permission-mode dontAsk or --dangerously-skip-permissions");
            return HeadlessExit::UsageError.code();
        }
        ResolvedPermissionMode::Mode { mode, warning } => {
            if let Some(w) = warning {
                eprintln!("{w}");
            }
            mode
        }
    };

    let prompt = match classify_prompt(args.prompt.as_deref(), std::io::stdin().is_terminal()) {
        PromptSource::Provided(p) => p,
        PromptSource::ReadStdin => match read_stdin_to_string() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("error: failed to read prompt from stdin: {e}");
                return HeadlessExit::UsageError.code();
            }
        },
        PromptSource::RefuseTty => {
            eprintln!("error: stdin is a TTY and no positional prompt was provided");
            eprintln!("hint: lingxi-ascendc -p \"your prompt here\"");
            return HeadlessExit::UsageError.code();
        }
    };
    let prompt = prompt.trim().to_owned();
    if prompt.is_empty() {
        eprintln!("error: prompt is empty");
        return HeadlessExit::UsageError.code();
    }

    let launch_settings =
        build_launch_settings(permission_mode, args.model.as_deref(), args.max_turns);

    let bridge_launcher = match crate::agent::bridge::resolve_bridge_launcher(bridge_script) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: failed to resolve bridge: {e:#}");
            return HeadlessExit::ConnectionFailed.code();
        }
    };
    let client = match BridgeClient::spawn(&bridge_launcher) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to spawn bridge: {e:#}");
            return HeadlessExit::ConnectionFailed.code();
        }
    };
    let (mut writer, mut reader) = client.split();

    let exit_code = match args.output_format {
        PrintOutputFormat::StreamJson => {
            // `std::io::stdout()` returns a `Stdout` (Send). Avoid `.lock()` —
            // its guard is `!Send` and we'd then need to drop the Send bound on
            // `Emitter`. Re-locking on every write is fine: headless emits one
            // line per event and stdout writes are already line-buffered.
            let mut emitter = StreamJsonEmitter::new(std::io::stdout());
            run_session(&mut reader, &mut writer, &mut emitter, args, cwd, &prompt, launch_settings)
                .await
        }
        PrintOutputFormat::Text => {
            let mut emitter = TextEmitter::new(std::io::stdout(), std::io::stderr());
            run_session(&mut reader, &mut writer, &mut emitter, args, cwd, &prompt, launch_settings)
                .await
        }
    };

    // Tear down regardless of outcome. We swallow shutdown errors — at this
    // point we already have an exit code; logging is the right channel.
    if let Err(e) = shutdown_phase(writer).await {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "headless_shutdown_failed",
            message = "headless shutdown returned error",
            error = %e,
        );
    }

    exit_code
}

async fn run_session(
    reader: &mut BridgeReader,
    writer: &mut BridgeWriter,
    emitter: &mut dyn Emitter,
    args: &PrintArgs,
    cwd: &Path,
    prompt: &str,
    launch_settings: SessionLaunchSettings,
) -> i32 {
    let cwd_str = cwd.to_string_lossy().into_owned();

    // 1. Initialize.
    if let Err(e) = writer
        .send(CommandEnvelope {
            request_id: None,
            command: BridgeCommand::Initialize {
                cwd: cwd_str.clone(),
                metadata: BTreeMap::default(),
            },
        })
        .await
    {
        eprintln!("error: failed to send initialize: {e:#}");
        return HeadlessExit::ConnectionFailed.code();
    }
    let init_env = match init_phase(reader).await {
        Ok(env) => env,
        Err(exit) => return exit.code(),
    };
    if emit_event_safely(emitter, &init_env.event).is_err() {
        return HeadlessExit::ConnectionFailed.code();
    }

    // 2. Create session.
    if let Err(e) = writer
        .send(CommandEnvelope {
            request_id: None,
            command: BridgeCommand::CreateSession {
                cwd: cwd_str.clone(),
                resume: args.resume.clone(),
                launch_settings: launch_settings.clone(),
                metadata: BTreeMap::default(),
            },
        })
        .await
    {
        eprintln!("error: failed to send create_session: {e:#}");
        return HeadlessExit::ConnectionFailed.code();
    }
    let connect_env = match connect_phase(reader).await {
        Ok(env) => env,
        Err(exit) => return exit.code(),
    };
    let session_id = match &connect_env.event {
        BridgeEvent::Connected { session_id, .. }
        | BridgeEvent::SessionReplaced { session_id, .. } => session_id.clone(),
        // connect_phase guarantees one of the above on Ok().
        _ => unreachable!("connect_phase returned non-Connected event"),
    };
    if emit_event_safely(emitter, &connect_env.event).is_err() {
        return HeadlessExit::ConnectionFailed.code();
    }

    // 3. Send prompt.
    let chunks = vec![crate::agent::types::PromptChunk {
        kind: "text".to_owned(),
        value: serde_json::Value::String(prompt.to_owned()),
    }];
    if let Err(e) = writer
        .send(CommandEnvelope {
            request_id: None,
            command: BridgeCommand::Prompt { session_id: session_id.clone(), chunks },
        })
        .await
    {
        eprintln!("error: failed to send prompt: {e:#}");
        return HeadlessExit::ConnectionFailed.code();
    }

    // 4. Stream until terminal.
    let idle = Duration::from_secs(args.idle_timeout_secs);
    let hard = args.timeout_secs.map(Duration::from_secs);
    let mut watchdog = Watchdog::new(idle, hard);
    let outcome = streaming_phase(reader, writer, emitter, &mut watchdog, &session_id).await;
    if let Err(e) = emitter.finish() {
        tracing::warn!(
            target: crate::logging::targets::BRIDGE_PROTOCOL,
            event_name = "headless_emitter_finish_failed",
            error = %e,
        );
    }

    // Surface non-`Completed` reason on stderr for human tail-watching.
    if let HeadlessExit::NonCompletedTerminal(reason) = &outcome {
        let reason_str = reason.map_or("unknown", |r| r.as_stored());
        eprintln!("error: terminal_reason={reason_str}");
    }

    outcome.code()
}

/// Emit an event, logging (but otherwise tolerating) IO errors. A broken
/// pipe on stdout — common when the consumer is `head` or a closed pager —
/// must not be reported as a bridge failure; we just stop emitting.
fn emit_event_safely(emitter: &mut dyn Emitter, ev: &BridgeEvent) -> Result<(), ()> {
    match emitter.emit_event(ev) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => {
            // Downstream consumer closed; treat as silent success and stop emitting.
            tracing::debug!(
                target: crate::logging::targets::BRIDGE_PROTOCOL,
                event_name = "headless_stdout_broken_pipe",
            );
            Err(())
        }
        Err(e) => {
            eprintln!("error: failed to write output: {e}");
            Err(())
        }
    }
}

fn read_stdin_to_string() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

fn build_launch_settings(
    permission_mode: &str,
    model: Option<&str>,
    max_turns: Option<u32>,
) -> SessionLaunchSettings {
    let mut settings = serde_json::Map::new();
    settings
        .insert("permissions".to_owned(), serde_json::json!({ "defaultMode": permission_mode }));
    if let Some(m) = model {
        settings.insert("model".to_owned(), serde_json::Value::String(m.to_owned()));
    }
    if let Some(n) = max_turns {
        settings.insert("maxTurns".to_owned(), serde_json::Value::from(n));
    }
    SessionLaunchSettings {
        settings: Some(serde_json::Value::Object(settings)),
        ..SessionLaunchSettings::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_compiles() {}

    #[test]
    fn build_launch_settings_includes_permission_mode() {
        let s = build_launch_settings("dontAsk", None, None);
        let v = s.settings.expect("settings");
        let perms = v.get("permissions").expect("permissions");
        assert_eq!(perms.get("defaultMode").and_then(|x| x.as_str()), Some("dontAsk"));
        assert!(v.get("model").is_none());
        assert!(v.get("maxTurns").is_none());
    }

    #[test]
    fn build_launch_settings_includes_model_and_max_turns_when_set() {
        let s = build_launch_settings("bypassPermissions", Some("opus-4-7"), Some(7));
        let v = s.settings.expect("settings");
        assert_eq!(v.get("model").and_then(|x| x.as_str()), Some("opus-4-7"));
        assert_eq!(v.get("maxTurns").and_then(|x| x.as_u64()), Some(7));
    }
}
