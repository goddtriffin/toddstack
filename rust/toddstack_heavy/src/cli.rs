//! Argument parsing: `heavy [flags] [--] cmd args...` and `heavy status`.

use std::time::Duration;

use crate::error::Error;
use crate::sizes::parse_size;

pub const USAGE: &str = "usage: heavy [--cpu N] [--mem SIZE] [--vram SIZE] [--disk SIZE] [--timeout DURATION] [--] cmd [args...]\n       heavy status";

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
    Help,
    Version,
    Run { flags: Flags, cmd: Vec<String> },
}

/// The default maximum run time (from config: `HEAVY_TIMEOUT`, else this).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_hours(1);

/// `90s`, `10m`, `2h`, `1500ms`; a bare number is seconds.
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
        _ => Err(Error::Usage(format!("not a duration: {text:?} (try 90s, 10m, 2h)"))),
    }
}

impl Flags {
    /// `--timeout`, else `HEAVY_TIMEOUT`, else 60 minutes.
    #[must_use]
    pub fn timeout(&self) -> Duration {
        self.timeout.or_else(|| parse_duration(&std::env::var("HEAVY_TIMEOUT").ok()?).ok()).unwrap_or(DEFAULT_TIMEOUT)
    }
}

/// # Errors
/// Returns a usage error for unknown flags, bad values, or a missing command.
pub fn parse(args: &[String]) -> Result<Command, Error> {
    if args == ["status"] {
        return Ok(Command::Status);
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
    fn status_and_errors() {
        assert_eq!(parse(&a("status")).unwrap(), Command::Status);
        assert!(matches!(parse(&a("-- status")).unwrap(), Command::Run { .. }));
        assert!(parse(&a("--cpu 4")).is_err());
        assert!(parse(&a("--bogus 1 x")).is_err());
        assert!(parse(&a("--mem lots x")).is_err());
    }
}
