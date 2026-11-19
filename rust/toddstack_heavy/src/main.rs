//! heavy: a machine-wide admission queue for heavy jobs. See the README.

mod admit;
mod cli;
mod error;
mod history;
mod job;
mod machine;
mod run;
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
        Command::Run { flags, cmd } => run::run(&StateDir::from_env()?, &flags, &cmd),
    }
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
