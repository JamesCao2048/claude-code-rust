//! Integration tests for the headless `-p` mode.
//!
//! These spawn the real `lingxi-ascendc` binary against a stub bridge
//! (`tests/fixtures/headless/stub_bridge.mjs`) that emits scripted JSON.
//! Each test crafts a small "script" of EMIT/SLEEP/IGNORE_SHUTDOWN/EXIT_NOW
//! directives and asserts on the binary's exit code, stdout, and stderr.
//!
//! Tests are skipped if `node` is not on PATH (mirroring the production
//! bridge requirement).

use assert_cmd::Command;
use std::io::Write as _;
use tempfile::NamedTempFile;

/// One stub-bridge script. Compose with the helper constructors below for
/// readability at the call site.
pub struct StubScript(pub Vec<String>);

impl StubScript {
    pub fn new(lines: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self(lines.into_iter().map(Into::into).collect())
    }
    pub fn write_to_temp(&self) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(self.0.join("\n").as_bytes()).expect("write");
        f.write_all(b"\n").expect("write trailing");
        f
    }
}

pub fn node_available() -> bool {
    which::which("node").is_ok()
}

/// Build a `Command` for the headless binary, wired to invoke the stub bridge
/// against the given script. Caller chains `.assert()` for expectations.
pub fn cmd_for_stub(extra_args: &[&str], script: &StubScript) -> (Command, NamedTempFile) {
    let stub_js = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/headless/stub_bridge.mjs");
    let script_file = script.write_to_temp();
    let mut cmd = Command::cargo_bin("lingxi-ascendc").expect("cargo bin");
    cmd.args(["--bridge-script", stub_js.to_str().expect("utf8 path")])
        .args(extra_args)
        .env("STUB_SCRIPT", script_file.path())
        .env("LINGXI_HEADLESS_REQUIRE_EXPLICIT_PERMISSION_MODE", "0");
    (cmd, script_file)
}

// Canned event payloads — keep these as String so callers can format session ids etc.
pub fn ev_initialized() -> String {
    r#"{"event":"initialized","result":{"agent_name":"stub","agent_version":"0.0","auth_methods":[],"capabilities":{"prompt_image":false,"prompt_embedded_context":false,"supports_session_listing":false,"supports_resume_session":false}}}"#.to_owned()
}

pub fn ev_connected(session_id: &str) -> String {
    // CurrentModel requires the full field set — see `crate::agent::types::CurrentModel`.
    // We hand-roll a minimal but spec-compliant payload here.
    let model = r#"{"requested_id":null,"resolved_id":"stub","display_name_short":"Stub","display_name_long":"Stub Model","catalog_id":null,"supports_effort":false,"supported_effort_levels":[],"supports_fast_mode":null,"supports_auto_mode":null,"supports_adaptive_thinking":null,"is_authoritative":true}"#;
    format!(
        r#"{{"event":"connected","session_id":"{session_id}","cwd":"/tmp","current_model":{model},"available_models":[],"mode":null,"history_updates":null}}"#
    )
}

pub fn ev_turn_complete(session_id: &str, terminal_reason: Option<&str>) -> String {
    match terminal_reason {
        Some(r) => format!(r#"{{"event":"turn_complete","session_id":"{session_id}","terminal_reason":"{r}"}}"#),
        None => format!(r#"{{"event":"turn_complete","session_id":"{session_id}"}}"#),
    }
}

pub fn ev_permission_request(session_id: &str) -> String {
    // PermissionRequest contains a full ToolCall + Vec<PermissionOption>.
    // ToolCall is hefty — see `crate::agent::types::ToolCall`. Provide the
    // minimum field set with empty arrays / None for everything optional.
    let tool_call = r#"{"tool_call_id":"tc1","title":"Bash","kind":"BashTool","status":"pending","content":[],"raw_input":null,"raw_output":null,"output_metadata":null,"task_metadata":null,"locations":[],"meta":null}"#;
    let options = r#"[{"option_id":"allow","name":"Allow","description":null,"kind":"allow_once"}]"#;
    format!(
        r#"{{"event":"permission_request","session_id":"{session_id}","request":{{"tool_call":{tool_call},"options":{options},"display":null}}}}"#
    )
}

pub fn ev_chunk(session_id: &str, text: &str) -> String {
    // SessionUpdate::AgentMessageChunk { content: ContentBlock::Text { text } }
    // wrapped in the BridgeEvent::SessionUpdate envelope. ContentBlock variants
    // are tagged with `type`; the text variant is `{"type":"text","text":"..."}`
    // (verified against agent::types module).
    let escaped = serde_json::Value::String(text.to_owned()).to_string();
    format!(
        r#"{{"event":"session_update","session_id":"{session_id}","update":{{"type":"agent_message_chunk","content":{{"type":"text","text":{escaped}}}}}}}"#
    )
}

// ---------------------------------------------------------------------------
// T16: happy path, stream-json default format.
// ---------------------------------------------------------------------------
#[test]
fn t16_happy_path_stream_json() {
    if !node_available() { eprintln!("skip: node not on PATH"); return; }
    let session = "s-t16";
    let script = StubScript::new(vec![
        format!("EMIT {}", ev_initialized()),
        format!("EMIT {}", ev_connected(session)),
        format!("EMIT {}", ev_chunk(session, "hi")),
        format!("EMIT {}", ev_turn_complete(session, Some("completed"))),
    ]);
    let (mut cmd, _f) = cmd_for_stub(&["-p", "ping"], &script);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let lines: Vec<_> = stdout.lines().collect();
    // First and last lines have known shapes.
    assert!(
        lines.first().is_some_and(|l| l.contains(r#""type":"system.init""#)),
        "expected system.init first; stdout:\n{stdout}"
    );
    assert!(
        lines.iter().any(|l| l.contains(r#""type":"system.session""#)),
        "expected system.session present; stdout:\n{stdout}"
    );
    assert!(
        lines.last().is_some_and(|l| l.contains(r#""type":"result.success""#)),
        "expected result.success last; stdout:\n{stdout}"
    );
    // Every line has the v:1 schema marker.
    for line in &lines {
        assert!(line.contains(r#""v":1"#), "missing v:1 in line: {line}");
    }
}

// ---------------------------------------------------------------------------
// T17: happy path, text format.
// ---------------------------------------------------------------------------
#[test]
fn t17_happy_path_text() {
    if !node_available() { eprintln!("skip: node not on PATH"); return; }
    let session = "s-t17";
    let script = StubScript::new(vec![
        format!("EMIT {}", ev_initialized()),
        format!("EMIT {}", ev_connected(session)),
        format!("EMIT {}", ev_chunk(session, "hello world")),
        format!("EMIT {}", ev_turn_complete(session, Some("completed"))),
    ]);
    // `--output-format` is on the `print` subcommand; the top-level `-p`
    // alias has no flag-passthrough, so use the subcommand form here.
    let (mut cmd, _f) = cmd_for_stub(&["print", "--output-format", "text", "ping"], &script);
    let out = cmd.assert().success().get_output().clone();
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    // Text emitter writes prose to stdout; should contain the assistant text.
    assert!(stdout.contains("hello world"), "stdout missing prose; got: {stdout:?}");
    // No NDJSON markers should appear on stdout.
    assert!(!stdout.contains(r#""type":"system.init""#));
}

// ---------------------------------------------------------------------------
// T18: hang-after-result — bridge ignores `Shutdown`, driver must hard-kill
// within `SHUTDOWN_TIMEOUT_SECS` (5s) and still report exit 0.
// ---------------------------------------------------------------------------
#[test]
fn t18_hang_after_result_force_kills_within_grace() {
    if !node_available() { eprintln!("skip: node not on PATH"); return; }
    let session = "s-t18";
    let script = StubScript::new(vec![
        format!("EMIT {}", ev_initialized()),
        format!("EMIT {}", ev_connected(session)),
        format!("EMIT {}", ev_turn_complete(session, Some("completed"))),
        "IGNORE_SHUTDOWN".to_owned(),
        // Keep stub alive forever — driver's grace timeout must fire.
        "SLEEP 60000".to_owned(),
    ]);
    let started = std::time::Instant::now();
    let (mut cmd, _f) = cmd_for_stub(&["-p", "ping"], &script);
    cmd.timeout(std::time::Duration::from_secs(15)).assert().success();
    // Grace is 5s; allow generous slack for cargo test overhead.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(12),
        "hang-after-result should resolve via force-kill within grace, took {:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// T19: a `permission_request` sneaks past the `bypassPermissions` mode → exit 2.
// ---------------------------------------------------------------------------
#[test]
fn t19_rogue_permission_request_exit_2() {
    if !node_available() { eprintln!("skip: node not on PATH"); return; }
    let session = "s-t19";
    let script = StubScript::new(vec![
        format!("EMIT {}", ev_initialized()),
        format!("EMIT {}", ev_connected(session)),
        format!("EMIT {}", ev_permission_request(session)),
    ]);
    let (mut cmd, _f) = cmd_for_stub(&["-p", "ping"], &script);
    cmd.assert().code(2);
}

// ---------------------------------------------------------------------------
// T20: empty stdin / no positional → exit 64. (Cannot give the child a TTY
// from a test runner, so we exercise the "missing prompt + piped empty
// stdin" path; both end at the same `UsageError` exit code, which is what
// the contract guarantees.)
// ---------------------------------------------------------------------------
#[test]
fn t20_no_prompt_with_empty_stdin_exits_64() {
    if !node_available() { eprintln!("skip: node not on PATH"); return; }
    // No script needed — the binary should refuse before spawning the bridge.
    let stub_js = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/headless/stub_bridge.mjs");
    let mut cmd = Command::cargo_bin("lingxi-ascendc").expect("cargo bin");
    cmd.args(["--bridge-script", stub_js.to_str().unwrap()])
        .args(["-p", ""])
        .env("LINGXI_HEADLESS_REQUIRE_EXPLICIT_PERMISSION_MODE", "0")
        .write_stdin("");
    cmd.assert().code(64);
}

// ---------------------------------------------------------------------------
// T21: terminal_reason=max_turns → exit 1, stderr names the reason.
// ---------------------------------------------------------------------------
#[test]
fn t21_terminal_reason_max_turns_exit_1() {
    if !node_available() { eprintln!("skip: node not on PATH"); return; }
    let session = "s-t21";
    let script = StubScript::new(vec![
        format!("EMIT {}", ev_initialized()),
        format!("EMIT {}", ev_connected(session)),
        format!("EMIT {}", ev_turn_complete(session, Some("max_turns"))),
    ]);
    let (mut cmd, _f) = cmd_for_stub(&["-p", "ping"], &script);
    let out = cmd.assert().code(1).get_output().clone();
    let stdout = String::from_utf8(out.stdout).expect("utf8");
    let stderr = String::from_utf8(out.stderr).expect("utf8");
    assert!(
        stdout.contains(r#""type":"result.terminated""#),
        "expected result.terminated NDJSON line; stdout: {stdout}"
    );
    assert!(
        stdout.contains(r#""terminal_reason":"max_turns""#),
        "expected max_turns reason in NDJSON; stdout: {stdout}"
    );
    assert!(
        stderr.contains("max_turns"),
        "expected max_turns mention on stderr; stderr: {stderr}"
    );
}
