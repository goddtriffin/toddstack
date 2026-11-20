//! heavy: a machine-wide admission queue for heavy jobs. See the README.

mod admit;
mod cli;
mod error;
mod history;
mod job;
mod log;
mod machine;
mod report;
mod run;
mod sample;
mod sizes;
mod state;
mod stats;
mod status;
mod tty;

use std::process::ExitCode;

use crate::cli::Command;
use crate::error::Error;
use crate::state::StateDir;

fn real_main() -> Result<u8, Error> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match cli::parse(&args)? {
        Command::Help => {
            println!("{}", cli::USAGE);
            Ok(0)
        }
        Command::Version => {
            println!("heavy {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Command::Status => {
            status::print(&StateDir::from_env()?)?;
            Ok(0)
        }
        Command::History { since, html } => history(&StateDir::from_env()?, since, html.as_deref()),
        Command::Run { flags, cmd } => run::run(&StateDir::from_env()?, &flags, &cmd),
    }
}

fn history(state: &StateDir, since: std::time::Duration, html: Option<&std::path::Path>) -> Result<u8, Error> {
    let now = jiff::Timestamp::now();
    let from = now.checked_sub(since).map_err(|e| Error::Usage(format!("--since: {e}")))?;
    let events = log::read_since(state.base(), from);
    let report = report::compute(&events, from.as_millisecond(), now.as_millisecond());
    if let Some(path) = html {
        std::fs::write(path, report::render_html(&report))?;
        println!("{}", std::fs::canonicalize(path)?.display());
    } else {
        print!("{}", report::render_text(&report));
    }
    Ok(0)
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => ExitCode::from(code),
        Err(Error::Usage(msg)) => {
            eprintln!("heavy: {msg}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("heavy: {e}");
            ExitCode::from(125)
        }
    }
}
