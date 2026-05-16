use lingxi_ascendc::cli_dispatch::{dispatch, Action};

#[test]
fn run_subcommand_routes_to_cli_mode() {
    let args = vec!["run".to_string(), "--workflow".to_string(), "generate_tilelang".to_string()];
    match dispatch(&args) {
        Action::CliMode(sub, rest) => {
            assert_eq!(sub, "run");
            assert_eq!(rest, vec!["--workflow", "generate_tilelang"]);
        }
        _ => panic!("expected CliMode"),
    }
}

#[test]
fn verify_env_subcommand_routes_to_cli_mode() {
    let args = vec!["verify-env".to_string(), "--target".to_string(), "cjm_cann1".to_string()];
    match dispatch(&args) {
        Action::CliMode(sub, _) => assert_eq!(sub, "verify-env"),
        _ => panic!("expected CliMode"),
    }
}

#[test]
fn skill_subcommand_routes_to_cli_mode() {
    let args = vec![
        "skill".to_string(),
        "ascendc-verify".to_string(),
        "--workspace".to_string(),
        ".".to_string(),
        "--action-id".to_string(),
        "agent-simple-attempt0".to_string(),
    ];
    match dispatch(&args) {
        Action::CliMode(sub, rest) => {
            assert_eq!(sub, "skill");
            assert_eq!(rest[0], "ascendc-verify");
        }
        _ => panic!("expected CliMode"),
    }
}

#[test]
fn batch_subcommand_routes_to_cli_mode() {
    let args = vec![
        "batch".to_string(),
        "--workflow".to_string(),
        "generate_ascendc_via_tilelang".to_string(),
        "--select".to_string(),
        "L1/1-3".to_string(),
    ];
    match dispatch(&args) {
        Action::CliMode(sub, rest) => {
            assert_eq!(sub, "batch");
            assert_eq!(
                rest,
                vec!["--workflow", "generate_ascendc_via_tilelang", "--select", "L1/1-3"]
            );
        }
        _ => panic!("expected CliMode"),
    }
}

#[test]
fn unknown_subcommand_falls_to_tui() {
    let args = vec!["--continue".to_string(), "msg".to_string()];
    match dispatch(&args) {
        Action::TuiMode(passed) => assert_eq!(passed, args),
        _ => panic!("expected TuiMode"),
    }
}

#[test]
fn empty_args_falls_to_tui() {
    match dispatch(&[]) {
        Action::TuiMode(p) => assert!(p.is_empty()),
        _ => panic!("expected TuiMode"),
    }
}
