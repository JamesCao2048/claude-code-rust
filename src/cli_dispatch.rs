// SPDX-License-Identifier: Apache-2.0
//! Reserved-subcommand dispatcher: routes `lingxi-ascendc run|verify-env|skill`
//! to the Python engine CLI; otherwise falls through to the TUI.

pub const RESERVED_SUBCOMMANDS: &[&str] = &["run", "verify-env", "skill"];

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    CliMode(String, Vec<String>),
    TuiMode(Vec<String>),
}

pub fn dispatch(args: &[String]) -> Action {
    match args.first().map(String::as_str) {
        Some(s) if RESERVED_SUBCOMMANDS.contains(&s) => {
            Action::CliMode(s.to_owned(), args[1..].to_vec())
        }
        _ => Action::TuiMode(args.to_vec()),
    }
}
