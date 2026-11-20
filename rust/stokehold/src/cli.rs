//! Argument parsing: `stokehold [flags] [--] cmd args...` and `stokehold status`.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::Error;
use crate::sizes::parse_size;

pub const USAGE: &str = "usage: stokehold [--cpu N] [--mem SIZE] [--vram SIZE] [--disk SIZE] [--timeout DURATION] [--] cmd [args...]\n       stokehold status\n       stokehold history [--since 2h|1d|7d] [--html PATH]";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    pub cpu: Option<u32>,
    pub mem: Option<u64>,
    pub vram: Option<u64>,
    pub disk: Option<u64>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Status,
    /// `stokehold history`: how well heavy used the machine.
    History {
        since: Duration,
        html: Option<PathBuf>,
    },
    Help,
    Version,
    Run {
        flags: Flags,
        cmd: Vec<String>,
    },
}

/// The default maximum run time (from config: `STOKEHOLD_TIMEOUT`, else this).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_hours(1);

/// The default window of `stokehold history`.
pub const DEFAULT_SINCE: Duration = Duration::from_hours(24);

/// `90s`, `10m`, `2h`, `1d`, `1500ms`; a bare number is seconds.
///
/// # Errors
/// Returns a usage error when the text is not a duration.
pub fn parse_duration(text: &str) -> Result<Duration, Error> {
    let t = text.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num.parse().map_err(|_| Error::Usage(format!("not a duration: {text:?} (try 90s, 10m, 2h)")))?;
    match unit {
        "" | "s" => Ok(Duration::from_secs(n)),
        "ms" => Ok(Duration::from_millis(n)),
        "m" => Ok(Duration::from_secs(n * 60)),
        "h" => Ok(Duration::from_secs(n * 3600)),
        "d" => Ok(Duration::from_secs(n * 86_400)),
        _ => Err(Error::Usage(format!("not a duration: {text:?} (try 90s, 10m, 2h)"))),
    }
}

impl Flags {
    /// `--timeout`, else `STOKEHOLD_TIMEOUT`, else 60 minutes.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout.or_else(|| parse_duration(&crate::state::env_var("STOKEHOLD_TIMEOUT")?.into_string().ok()?).ok()).unwrap_or(DEFAULT_TIMEOUT)
    }
}

fn parse_history(args: &[String]) -> Result<Command, Error> {
    let (mut since, mut html) = (DEFAULT_SINCE, None);
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let (name, inline) = arg.split_once('=').map_or((arg.as_str(), None), |(n, v)| (n, Some(v.to_string())));
        let mut value = || inline.clone().or_else(|| it.next().cloned()).ok_or_else(|| Error::Usage(format!("{name} needs a value")));
        match name {
            "--since" => since = parse_duration(&value()?)?,
            "--html" => html = Some(PathBuf::from(value()?)),
            _ => {
                return Err(Error::Usage(format!("stokehold history: unknown argument {arg:?}\n{USAGE}")));
            }
        }
    }
    Ok(Command::History { since, html })
}

/// # Errors
/// Returns a usage error for unknown flags, bad values, or a missing command.
pub fn parse(args: &[String]) -> Result<Command, Error> {
    // `--status` is an alias: older docs and sessions use it.
    if args == ["status"] || args == ["--status"] {
        return Ok(Command::Status);
    }
    if args.first().is_some_and(|a| a == "history") {
        return parse_history(&args[1..]);
    }
    let mut flags = Flags::default();
    let mut i = 0;
    while let Some(arg) = args.get(i) {
        i += 1;
        if arg == "--" {
            break;
        }
        if arg == "--help" || arg == "-h" {
            return Ok(Command::Help);
        }
        if arg == "--version" || arg == "-V" {
            return Ok(Command::Version);
        }
        let Some(flag) = arg.strip_prefix("--") else {
            i -= 1;
            break;
        };
        let (name, inline) = flag.split_once('=').map_or((flag, None), |(n, v)| (n, Some(v.to_string())));
        let value = if let Some(v) = inline {
            v
        } else {
            i += 1;
            args.get(i - 1).cloned().ok_or_else(|| Error::Usage(format!("--{name} needs a value")))?
        };
        let num = |v: &str| v.parse::<u32>().map_err(|_| Error::Usage(format!("--{name}: not a number: {v:?}")));
        match name {
            "cpu" => flags.cpu = Some(num(&value)?),
            "mem" => flags.mem = Some(parse_size(&value)?),
            "vram" => flags.vram = Some(parse_size(&value)?),
            "disk" => flags.disk = Some(parse_size(&value)?),
            "timeout" => flags.timeout = Some(parse_duration(&value)?),
            _ => return Err(Error::Usage(format!("unknown flag --{name}"))),
        }
    }
    let cmd = args.get(i..).unwrap_or_default().to_vec();
    if cmd.is_empty() {
        return Err(Error::Usage(USAGE.to_string()));
    }
    Ok(Command::Run { flags, cmd })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn parses_flags_and_command() {
        let Command::Run { flags, cmd } = parse(&a("--cpu 8 --mem=2G cargo test --release")).unwrap() else { panic!() };
        assert_eq!((flags.cpu, flags.mem), (Some(8), Some(2 << 30)));
        assert_eq!(cmd, a("cargo test --release"));
    }

    #[test]
    fn double_dash_ends_flags() {
        let Command::Run { flags, cmd } = parse(&a("-- --cpu 3")).unwrap() else { panic!() };
        assert_eq!(flags, Flags::default());
        assert_eq!(cmd, a("--cpu 3"));
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("250ms").unwrap(), Duration::from_millis(250));
        assert!(parse_duration("soon").is_err());
        let Command::Run { flags, .. } = parse(&a("--timeout 5m x")).unwrap() else { panic!() };
        assert_eq!(flags.timeout(), Duration::from_secs(300));
    }

    #[test]
    fn history_since_and_html() {
        assert_eq!(parse(&a("history")).unwrap(), Command::History { since: DEFAULT_SINCE, html: None });
        assert_eq!(parse(&a("history --since 2h")).unwrap(), Command::History { since: Duration::from_secs(7200), html: None });
        assert_eq!(parse(&a("history --since=1d --html /tmp/x.html")).unwrap(), Command::History { since: Duration::from_hours(24), html: Some("/tmp/x.html".into()) });
        let Command::History { since, .. } = parse(&a("history --since 7d")).unwrap() else { panic!() };
        assert_eq!(since, Duration::from_hours(168));
        assert!(parse(&a("history --since soon")).is_err());
        assert!(parse(&a("history --since")).is_err());
        assert!(parse(&a("history --bogus")).is_err());
    }

    #[test]
    fn status_and_errors() {
        assert_eq!(parse(&a("status")).unwrap(), Command::Status);
        assert_eq!(parse(&a("--status")).unwrap(), Command::Status);
        assert!(matches!(parse(&a("-- status")).unwrap(), Command::Run { .. }));
        assert!(parse(&a("--cpu 4")).is_err());
        assert!(parse(&a("--bogus 1 x")).is_err());
        assert!(parse(&a("--mem lots x")).is_err());
    }
}
