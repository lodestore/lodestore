#![forbid(unsafe_code)]

mod cli;
mod client;
mod cmd;
mod open;
mod output;
mod profile;
mod secrets;

use std::process::ExitCode;

use clap::Parser;
use lodestore_format::Error;

use crate::cli::{UnlockError, UsageError};
use crate::client::TokenRefused;

const EXIT_ERROR: u8 = 1;
const EXIT_USAGE: u8 = 2;
const EXIT_REPO_NOT_FOUND: u8 = 10;
const EXIT_REPO_EXISTS: u8 = 11;
const EXIT_UNLOCK: u8 = 12;
const EXIT_CHECK_FAILED: u8 = 3;
const EXIT_DRIFT: u8 = 4;
const EXIT_TOKEN: u8 = 13;
const EXIT_INTERRUPTED: u8 = 130;

fn main() -> ExitCode {
    let json_requested = std::env::args_os().any(|a| a == "--json");
    let cli = match cli::Cli::try_parse() {
        Ok(cli) => cli,
        Err(err) if err.use_stderr() && json_requested => {
            eprintln!(
                "{}",
                output::error_line(EXIT_USAGE, err.to_string().trim_end())
            );
            return ExitCode::from(EXIT_USAGE);
        }
        Err(err) => err.exit(),
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    let json = cli.json;
    match cmd::run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            let code = exit_code_for(&err);
            if json {
                eprintln!("{}", output::error_line(code, &format!("{err:#}")));
            } else {
                eprintln!("error: {err:#}");
            }
            ExitCode::from(code)
        }
    }
}

fn exit_code_for(err: &anyhow::Error) -> u8 {
    for cause in err.chain() {
        if cause.is::<UsageError>() {
            return EXIT_USAGE;
        }
        if cause.is::<TokenRefused>() {
            return EXIT_TOKEN;
        }
        if cause.is::<UnlockError>() {
            return EXIT_UNLOCK;
        }
        match cause.downcast_ref::<Error>() {
            Some(Error::NotInitialized) => return EXIT_REPO_NOT_FOUND,
            Some(Error::AlreadyInitialized) => return EXIT_REPO_EXISTS,
            Some(Error::WrongSecret) => return EXIT_UNLOCK,
            Some(Error::Interrupted) => return EXIT_INTERRUPTED,
            _ => {}
        }
    }
    EXIT_ERROR
}
