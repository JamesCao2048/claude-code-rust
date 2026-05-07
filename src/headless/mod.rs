//! Non-interactive (`-p` / headless) mode entry point.
//!
//! Design: `docs/headless/design-headless-mode.md`.
pub mod driver;
pub mod output;
pub mod watchdog;

#[cfg(test)]
mod tests {
    #[test]
    fn module_compiles() {}
}
