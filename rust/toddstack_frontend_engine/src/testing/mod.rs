//! Headless harness: a fake world (clock, timers, store, seeded random, scripted HTTP), a
//! session that answers a bridge's effects against it, a JSON-lines scenario runner with
//! snapshots, and an NDJSON REPL. Test and CLI use only (reads fixtures from disk).

mod matching;
mod repl;
mod scenario;
mod session;
mod world;

pub use matching::{exact_diff, partial_diff};
pub use repl::run as run_repl;
pub use scenario::{Failure, Options, run_file};
pub use session::{Answerer, Factory, HttpHandler, HttpResponse, Session, factory};
pub use world::{Script, World};
