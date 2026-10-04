//! Ishtaria world server.
//!
//! Current state: configuration loading and identity bootstrap only.
//! See the architecture chapter of ishtaria-docs for the planned layout
//! (simulation shards over H3 cells, federation endpoint, persistence).

use ishtaria_core::{RulesetVersion, ServerName};
use serde::Deserialize;
use std::{env, fs, process::ExitCode};

const DEFAULT_CONFIG: &str = "/etc/ishtaria/server.toml";

#[derive(Debug, Deserialize)]
struct Config {
    server_name: String,
    ruleset: String,
    #[serde(default = "default_listen")]
    listen: String,
}

fn default_listen() -> String {
    "0.0.0.0:7400".into()
}

fn main() -> ExitCode {
    let path = env::args().nth(1).unwrap_or_else(|| DEFAULT_CONFIG.into());
    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let cfg: Config = match toml::from_str(&raw) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("invalid config {path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let name: ServerName = match cfg.server_name.parse() {
        Ok(n) => n,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let ruleset: RulesetVersion = match cfg.ruleset.parse() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("invalid ruleset '{}': {e}", cfg.ruleset);
            return ExitCode::FAILURE;
        }
    };
    println!(
        "ishtaria-server {} – world {name}, ruleset {ruleset}, listen {}",
        env!("CARGO_PKG_VERSION"),
        cfg.listen
    );
    println!("simulation loop not implemented yet");
    ExitCode::SUCCESS
}
