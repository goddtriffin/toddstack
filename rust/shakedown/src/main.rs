//! shakedown: one local testing tool for every project. See the README for the commands and `shakedown.toml`.

mod budgets;
mod config;
mod doctor;
mod error;
mod pixels;
mod report;
mod steps;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Arg, ArgAction, ArgMatches, Command, value_parser};

use crate::budgets::Verdict;
use crate::config::Config;
use crate::error::Error;
use crate::report::PixelFailure;
use crate::steps::{Job, Node, Status};

const SIZES: &str = "budgets.sizes";

fn cli() -> Command {
    Command::new("shakedown")
        .version(env!("CARGO_PKG_VERSION"))
        .about("One local testing tool for every project")
        .subcommand_required(true)
        .arg_required_else_help(true)
        .arg(Arg::new("config").long("config").global(true).value_parser(value_parser!(PathBuf)).help("Path to shakedown.toml (default: the nearest one, searching upward from here)"))
        .subcommand(
            Command::new("check")
                .about("Run the whole local gate: configured steps plus built-in ones, in parallel")
                .arg(Arg::new("only").long("only").value_delimiter(',').help("Run only these steps (comma separated); manual steps run only when named here"))
                .arg(Arg::new("json").long("json").action(ArgAction::SetTrue).help("Also write target/shakedown/summary.json")),
        )
        .subcommand(
            Command::new("budgets")
                .about("Size and speed budgets")
                .subcommand_required(true)
                .subcommand(Command::new("sizes").about("Exact byte ceilings from [budgets.sizes]")),
        )
        .subcommand(
            Command::new("pixels")
                .about("Pixel compare and PNG tools")
                .subcommand_required(true)
                .subcommand(
                    Command::new("compare")
                        .about("Compare an actual PNG with a reference; writes a diff image and report.html on failure")
                        .arg(Arg::new("reference").required(true).value_parser(value_parser!(PathBuf)).help("The approved reference"))
                        .arg(Arg::new("actual").required(true).value_parser(value_parser!(PathBuf)).help("The new image"))
                        .arg(Arg::new("out").long("out").value_parser(value_parser!(PathBuf)).default_value("target/shakedown/pixels").help("Where to write the diff and report on failure")),
                )
                .subcommand(
                    Command::new("encode")
                        .about("Re-encode a PNG the way references are stored (RGB, channels rounded to a multiple of --step)")
                        .arg(Arg::new("input").required(true).value_parser(value_parser!(PathBuf)))
                        .arg(Arg::new("output").required(true).value_parser(value_parser!(PathBuf)))
                        .arg(Arg::new("step").long("step").value_parser(value_parser!(u8)).default_value("8").help("Rounding step; 1 keeps every value")),
                ),
        )
        .subcommand(Command::new("doctor").about("Check the tools the platforms need, and print the fix for each miss"))
}

fn main() -> ExitCode {
    match run(&cli().get_matches()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let kind = match e {
                Error::Failed(_) => "FAIL",
                Error::Config(_) => "error",
                Error::Environment(_) => "environment",
            };
            eprintln!("{kind}: {e}");
            ExitCode::from(e.code())
        }
    }
}

fn run(m: &ArgMatches) -> Result<(), Error> {
    let explicit = m.get_one::<PathBuf>("config").map(PathBuf::as_path);
    match m.subcommand() {
        Some(("doctor", _)) => doctor_cmd(),
        Some(("pixels", sub)) => pixels_cmd(sub),
        Some(("check", sub)) => {
            let (config, root) = load(explicit)?;
            let only: Vec<String> = sub
                .get_many::<String>("only")
                .map_or_else(Vec::new, |v| v.cloned().collect());
            check(&config, &root, &only, sub.get_flag("json"))
        }
        Some(("budgets", _)) => {
            let (config, root) = load(explicit)?;
            sizes_cmd(&config, &root)
        }
        _ => Err(Error::Config("unknown command".into())),
    }
}

fn load(explicit: Option<&Path>) -> Result<(Config, PathBuf), Error> {
    let path = if let Some(p) = explicit {
        p.to_path_buf()
    } else {
        let cwd = std::env::current_dir().map_err(|e| Error::Config(e.to_string()))?;
        cwd.ancestors()
            .map(|d| d.join("shakedown.toml"))
            .find(|p| p.is_file())
            .ok_or_else(|| {
                Error::Config("no shakedown.toml here or in any parent directory".into())
            })?
    };
    let config = Config::load(&path)?;
    let root = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf);
    Ok((config, root))
}

fn out_dir(root: &Path) -> PathBuf {
    root.join("target/shakedown")
}

fn sizes_status(config: &Config, root: &Path) -> (Status, String, String) {
    let rows = budgets::check(&config.budgets.sizes, root);
    let output = rows
        .iter()
        .map(budgets::Row::line)
        .collect::<Vec<_>>()
        .join("\n");
    let over = rows
        .iter()
        .filter(|r| matches!(r.verdict, Verdict::Over { .. }))
        .count();
    let skipped = rows
        .iter()
        .filter(|r| matches!(r.verdict, Verdict::Skipped(_)))
        .count();
    let status = if over > 0 {
        Status::Fail(format!("{over} over budget"))
    } else if skipped > 0 {
        Status::Fail(format!(
            "{skipped} not built (a skipped artifact is never a pass)"
        ))
    } else {
        Status::Ok
    };
    (status, format!("{} sizes", rows.len()), output)
}

fn check(config: &Config, root: &Path, only: &[String], json: bool) -> Result<(), Error> {
    let unused = config.unused_sections();
    if !unused.is_empty() {
        eprintln!(
            "note: sections not used by this version: {}",
            unused.join(", ")
        );
    }
    let builtin_listed = config.check.builtin.iter().any(|b| b == SIZES);
    let known: Vec<&str> = config
        .check
        .steps
        .iter()
        .map(|s| s.name.as_str())
        .chain(builtin_listed.then_some(SIZES))
        .collect();
    if let Some(bad) = only.iter().find(|o| !known.contains(&o.as_str())) {
        return Err(Error::Config(format!(
            "--only {bad}: no such step (have: {})",
            known.join(", ")
        )));
    }
    let wanted = |name: &str, manual: bool| {
        if only.is_empty() {
            !manual
        } else {
            only.iter().any(|o| o == name)
        }
    };
    let selected: Vec<&crate::config::Step> = config
        .check
        .steps
        .iter()
        .filter(|s| wanted(&s.name, s.manual))
        .collect();
    let mut nodes: Vec<Node> = selected
        .iter()
        .map(|s| Node {
            name: s.name.clone(),
            // A dependency outside the selection (--only) is not run, so it is not waited for.
            after: s
                .after
                .iter()
                .filter(|d| selected.iter().any(|x| &x.name == *d))
                .cloned()
                .collect(),
            job: Job::Shell(s.run.clone()),
        })
        .collect();
    if builtin_listed && wanted(SIZES, false) {
        nodes.push(Node {
            name: SIZES.into(),
            after: Vec::new(),
            job: Job::Builtin(SIZES.into()),
        });
    }
    if nodes.is_empty() {
        return Err(Error::Config(
            "nothing to run: add steps under [check] in shakedown.toml".into(),
        ));
    }
    let dir = out_dir(root).join("check");
    let results = steps::run(&nodes, root, &dir, &|_| sizes_status(config, root));
    print_results(&results);
    if json {
        let rows = if builtin_listed {
            budgets::check(&config.budgets.sizes, root)
        } else {
            Vec::new()
        };
        std::fs::write(
            out_dir(root).join("summary.json"),
            report::summary_json(&config.project, &results, &rows),
        )
        .map_err(|e| Error::Environment(format!("cannot write summary.json: {e}")))?;
    }
    if results.iter().all(|r| r.status == Status::Ok) {
        return Ok(());
    }
    let html = dir.join("report.html");
    std::fs::write(&html, report::steps_html(&results))
        .map_err(|e| Error::Environment(format!("cannot write report: {e}")))?;
    println!("report: target/shakedown/check/report.html");
    Err(Error::Failed(format!(
        "{} of {} steps did not pass",
        results.iter().filter(|r| r.status != Status::Ok).count(),
        results.len()
    )))
}

fn print_results(results: &[steps::StepResult]) {
    for r in results {
        match &r.status {
            Status::Ok if r.detail.is_empty() => println!("ok {} {:.1}s", r.name, r.secs),
            Status::Ok => println!("ok {} {} {:.1}s", r.name, r.detail, r.secs),
            Status::Fail(why) => println!("FAIL {} {why}", r.name),
            Status::Skipped(why) => println!("skip {} ({why})", r.name),
        }
    }
    for r in results
        .iter()
        .filter(|r| matches!(r.status, Status::Fail(_)))
    {
        let lines: Vec<&str> = r.output.lines().collect();
        let tail = lines.len().saturating_sub(40);
        println!(
            "--- {} (last {} of {} lines; full log: target/shakedown/check/{}.log)",
            r.name,
            lines.len() - tail,
            lines.len(),
            r.name
        );
        for l in &lines[tail..] {
            println!("{l}");
        }
    }
}

fn sizes_cmd(config: &Config, root: &Path) -> Result<(), Error> {
    if config.budgets.sizes.is_empty() {
        println!("no [budgets.sizes] configured");
        return Ok(());
    }
    let rows = budgets::check(&config.budgets.sizes, root);
    for r in &rows {
        println!("{}", r.line());
    }
    let over = rows
        .iter()
        .filter(|r| matches!(r.verdict, Verdict::Over { .. }))
        .count();
    if over > 0 {
        return Err(Error::Failed(format!("{over} over budget")));
    }
    Ok(())
}

fn pixels_cmd(matches: &ArgMatches) -> Result<(), Error> {
    let read =
        |p: &Path| std::fs::read(p).map_err(|e| Error::Config(format!("{}: {e}", p.display())));
    let path = |sub: &ArgMatches, k: &str| sub.get_one::<PathBuf>(k).cloned().unwrap_or_default();
    match matches.subcommand() {
        Some(("encode", sub)) => {
            let (input, output) = (path(sub, "input"), path(sub, "output"));
            let step = sub
                .get_one::<u8>("step")
                .copied()
                .unwrap_or(pixels::REF_STEP);
            let img = pixels::decode(&read(&input)?)?;
            std::fs::write(&output, pixels::encode(&img, step)?)
                .map_err(|e| Error::Config(format!("{}: {e}", output.display())))
        }
        Some(("compare", sub)) => {
            let (reference, actual, out) = (
                path(sub, "reference"),
                path(sub, "actual"),
                path(sub, "out"),
            );
            let name = actual
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("screen")
                .to_string();
            if !reference.is_file() {
                return Err(Error::Failed(format!(
                    "{name}: no reference at {}; generate it with shakedown update (or pixels encode)",
                    reference.display()
                )));
            }
            let (before, after) = (
                pixels::decode(&read(&reference)?)?,
                pixels::decode(&read(&actual)?)?,
            );
            let outcome = pixels::compare(&before, &after);
            if outcome.passes() {
                println!(
                    "ok {name} {:.4}% differ, worst block {:.1}%",
                    outcome.ratio * 100.0,
                    outcome.block * 100.0
                );
                return Ok(());
            }
            std::fs::create_dir_all(&out)
                .map_err(|e| Error::Environment(format!("{}: {e}", out.display())))?;
            let copy = |from: &Path, to: &str| -> Result<String, Error> {
                std::fs::copy(from, out.join(to)).map_err(|e| Error::Environment(e.to_string()))?;
                Ok(to.to_string())
            };
            let reason = if outcome.same_size {
                format!(
                    "{:.4}% of pixels differ (limit {:.1}%), worst block {:.1}% (limit {:.0}%)",
                    outcome.ratio * 100.0,
                    pixels::TOL_RATIO * 100.0,
                    outcome.block * 100.0,
                    pixels::TOL_BLOCK * 100.0
                )
            } else {
                format!(
                    "size changed: {}x{} to {}x{}",
                    before.w, before.h, after.w, after.h
                )
            };
            let diff = match &outcome.diff {
                Some(img) => {
                    let file = format!("{name}.diff.png");
                    std::fs::write(out.join(&file), pixels::encode(img, 1)?)
                        .map_err(|e| Error::Environment(e.to_string()))?;
                    Some(file)
                }
                None => None,
            };
            let failure = PixelFailure {
                id: name.clone(),
                reason: reason.clone(),
                reference: Some(copy(&reference, &format!("{name}.reference.png"))?),
                actual: Some(copy(&actual, &format!("{name}.actual.png"))?),
                diff,
            };
            std::fs::write(out.join("report.html"), report::pixels_html(&[failure]))
                .map_err(|e| Error::Environment(e.to_string()))?;
            println!("report: {}", out.join("report.html").display());
            Err(Error::Failed(format!("{name}: {reason}")))
        }
        _ => Err(Error::Config("unknown pixels command".into())),
    }
}

fn doctor_cmd() -> Result<(), Error> {
    let findings = doctor::run_all();
    for f in &findings {
        println!("{}", f.line());
    }
    let hard: Vec<&str> = findings
        .iter()
        .filter(|f| f.missing.is_some() && !f.optional)
        .map(|f| f.name)
        .collect();
    if hard.is_empty() {
        Ok(())
    } else {
        Err(Error::Environment(format!("missing: {}", hard.join(", "))))
    }
}
