// Copyright 2025 Simon Peter Rothgang
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use lingxi_ascendc::error::AppError;
use lingxi_ascendc::{Cli, Command};
use std::time::Instant;
use tracing::info_span;

#[allow(clippy::exit)]
fn main() {
    match run() {
        Ok(exit_code) => std::process::exit(exit_code),
        Err(err) => {
            if let Some(app_error) = extract_app_error(&err) {
                eprintln!("{}", app_error.user_message());
                std::process::exit(app_error.exit_code());
            }
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}

fn run() -> anyhow::Result<i32> {
    let cli = Cli::parse();
    // SAFETY: called before any threads are spawned (single-threaded at this point).
    // Must run before logging init so that env-driven log filters from settings.json apply,
    // and before bridge resolution so the spawned Node child inherits ANTHROPIC_* values.
    let injected_keys = unsafe { lingxi_ascendc::apply_settings_env_overrides() };
    let _logging = lingxi_ascendc::logging::LoggingRuntime::init(&cli)?;
    if !injected_keys.is_empty() {
        tracing::info!(
            target: lingxi_ascendc::logging::targets::APP_LIFECYCLE,
            event_name = "settings_env_injected",
            message = "applied env overrides from settings.json",
            keys = ?injected_keys,
        );
    }
    let perf_path = lingxi_ascendc::logging::resolve_perf_path(&cli)?;

    lingxi_ascendc::embedded_resources::EmbeddedResourceDir::cleanup_orphans();
    let resource_dir = lingxi_ascendc::embedded_resources::EmbeddedResourceDir::extract()
        .map_err(|e| anyhow::anyhow!("failed to extract embedded resources: {e}"))?;
    let workspace_dir = cli
        .dir
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let resource_path_os = resource_dir.path().as_os_str().to_owned();
    let pythonpath = match std::env::var_os("PYTHONPATH") {
        Some(existing) if !existing.is_empty() => {
            let mut combined = resource_path_os.clone();
            combined.push(if cfg!(windows) { ";" } else { ":" });
            combined.push(&existing);
            combined
        }
        _ => resource_path_os,
    };
    // SAFETY: called before any threads are spawned (single-threaded at this point)
    unsafe {
        std::env::set_var("LINGXI_RESOURCE_DIR", resource_dir.path());
        std::env::set_var("LINGXI_WORKSPACE", &workspace_dir);
        std::env::set_var("PYTHONPATH", &pythonpath);
    }

    #[cfg(not(feature = "perf"))]
    if perf_path.is_some() {
        return Err(anyhow::anyhow!(
            "perf telemetry requires a binary built with `--features perf`"
        ));
    }

    {
        let startup_bootstrap_span = info_span!(
            target: lingxi_ascendc::logging::targets::APP_LIFECYCLE,
            "startup_bootstrap",
            resume_requested = matches!(
                cli.command,
                Some(lingxi_ascendc::Command::Resume { .. })
            ),
            perf_telemetry_requested = perf_path.is_some(),
            explicit_bridge_script = cli.bridge_script.is_some(),
        );
        let _entered = startup_bootstrap_span.enter();
        let resolve_started = Instant::now();
        let bridge_launcher =
            lingxi_ascendc::agent::bridge::resolve_bridge_launcher(cli.bridge_script.as_deref())?;
        let duration_ms = u64::try_from(resolve_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        tracing::info!(
            target: lingxi_ascendc::logging::targets::BRIDGE_LIFECYCLE,
            event_name = "bridge_launcher_resolved",
            message = "resolved agent bridge launcher",
            duration_ms,
            launcher = %bridge_launcher.describe(),
        );
    }

    let rt = tokio::runtime::Runtime::new()?;
    let local_set = tokio::task::LocalSet::new();

    // Headless `print` / `-p` short-circuits the TUI: spawn the bridge
    // directly, stream events to stdout/stderr, exit with a code from
    // design §3.5. See `lingxi_ascendc::headless::run_print`.
    if cli.print.is_some() || matches!(cli.command, Some(Command::Print(_))) {
        let print_args = match cli.resolve_command()? {
            Command::Print(args) => args,
            other => unreachable!("expected Print, got {other:?}"),
        };
        let cli_perm = cli.permission_mode;
        let cli_skip = cli.dangerously_skip_permissions;
        let bridge_script = cli.bridge_script.clone();
        let exit_code = rt.block_on(local_set.run_until(async move {
            lingxi_ascendc::headless::run_print(
                &print_args,
                &workspace_dir,
                bridge_script.as_deref(),
                cli_perm,
                cli_skip,
            )
            .await
        }));
        return Ok(exit_code);
    }

    rt.block_on(local_set.run_until(async move {
        // Phase 1: create app in Connecting state (instant, no I/O)
        let mut app = lingxi_ascendc::app::create_app(&cli);

        // Phase 2: start non-session startup work + TUI.
        // The bridge itself is started from the TUI loop only after trust is accepted.
        lingxi_ascendc::app::start_update_check(&app, &cli);
        lingxi_ascendc::app::start_service_status_check(&app);
        let result = lingxi_ascendc::app::run_tui(&mut app).await;
        maybe_print_resume_hint(&app, result.is_ok());

        // Kill any spawned terminal child processes before exiting
        lingxi_ascendc::agent::events::kill_all_terminals(&app.terminals);

        if let Some(app_error) = app.exit_error.take() {
            return Err(anyhow::Error::new(app_error));
        }

        result.map(|()| 0)
    }))
}

fn extract_app_error(err: &anyhow::Error) -> Option<AppError> {
    err.chain().find_map(|cause| cause.downcast_ref::<AppError>().cloned())
}

fn maybe_print_resume_hint(app: &lingxi_ascendc::app::App, success: bool) {
    if !success {
        return;
    }
    let Some(session_id) = app.session_id.as_ref() else {
        return;
    };
    eprintln!("Resume this session: lingxi-ascendc resume {session_id}");
}
