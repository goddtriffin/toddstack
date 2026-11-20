//! `heavy history`: how well heavy used the machine, and whether idle time was ever heavy's fault.
//!
//! The headline numbers are the average CPU while jobs waited (it should be near full) and "idle while waiting":
//! sample intervals where a job waited although the machine had room (CPU under 75%, memory under 85%; load is
//! ignored, since on macOS it also counts threads blocked in I/O). Those minutes are the ones heavy can be blamed
//! for; the blocking reasons recorded in them say why it waited anyway.

use std::collections::BTreeMap;
use std::fmt::Write;

use jiff::Timestamp;

use crate::log::Event;

pub const IDLE_CPU_PCT: u64 = 75;
pub const IDLE_MEM_PCT: u64 = 85;
pub const BUCKET_MS: i64 = 300_000;

/// Tie-break for equal idle time: a specific check outranks `queue` (a consequence of another waiter's block), which
/// outranks `startable` (nothing blocked).
fn reason_rank(name: &str) -> usize {
    ["ramp", "cpu", "mem", "load1", "load5", "vram", "queue", "startable"].iter().position(|r| *r == name).unwrap_or(7)
}

/// Samples are 30 s apart while jobs exist; a longer gap means no job existed, so it counts for nothing.
pub const MAX_GAP_MS: i64 = 120_000;
/// The text timeline shows at most this many buckets (24 h); `--html` shows them all.
const TEXT_BUCKETS: usize = 288;
const ROW: usize = 72;

#[derive(Debug, Clone, PartialEq)]
pub struct RepoRow {
    pub repo: String,
    pub jobs: usize,
    pub wait_ms: u64,
    pub cpu_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    pub start_ms: i64,
    pub cpu: Option<f64>,
    pub running: Option<u32>,
    pub waiting: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub since_ms: i64,
    pub until_ms: i64,
    pub jobs: usize,
    pub wait_total_ms: u64,
    pub wait_avg_ms: u64,
    pub wait_p95_ms: u64,
    /// Wall time during which at least one job waited.
    pub waiting_span_ms: u64,
    /// Machine CPU % averaged over that wall time (time-weighted); `None` when no job waited.
    pub wait_cpu_avg: Option<u64>,
    /// Part of that wall time with room on the machine.
    pub idle_ms: u64,
    /// Blocking reasons during the idle part, by time (a reason counts once per interval).
    pub idle_reasons: Vec<(String, u64)>,
    pub repos: Vec<RepoRow>,
    pub buckets: Vec<Bucket>,
}

struct Sample {
    ts: i64,
    cpu_pct: u64,
    mem_pct: u64,
    running: u32,
    waiting: u32,
    blocked: BTreeMap<String, u32>,
}

/// Nearest-rank 95th percentile.
fn p95(sorted: &[u64]) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * 95).div_ceil(100).max(1);
    sorted.get(rank - 1).copied().unwrap_or(0)
}

#[must_use]
pub fn compute(events: &[Event], since_ms: i64, until_ms: i64) -> Report {
    let mut waits: Vec<u64> = Vec::new();
    let mut repos: BTreeMap<String, RepoRow> = BTreeMap::new();
    let mut samples: Vec<Sample> = Vec::new();
    for e in events {
        let ts = e.ts().as_millisecond();
        if ts < since_ms || ts > until_ms {
            continue;
        }
        match e {
            Event::Started { job, wait_ms, .. } => {
                waits.push(*wait_ms);
                let row = repos.entry(job.repo.clone().unwrap_or_else(|| "(no repo)".into())).or_insert_with_key(|k| RepoRow { repo: k.clone(), jobs: 0, wait_ms: 0, cpu_ms: 0 });
                row.jobs += 1;
                row.wait_ms += wait_ms;
            }
            Event::Finished { job, cpu_ms, .. } => {
                let row = repos.entry(job.repo.clone().unwrap_or_else(|| "(no repo)".into())).or_insert_with_key(|k| RepoRow { repo: k.clone(), jobs: 0, wait_ms: 0, cpu_ms: 0 });
                row.cpu_ms += cpu_ms;
            }
            Event::Sample { cpu_pct, mem_pct, running, waiting, blocked, .. } => {
                samples.push(Sample { ts, cpu_pct: *cpu_pct, mem_pct: *mem_pct, running: *running, waiting: *waiting, blocked: blocked.clone() });
            }
            Event::Queued { .. } => {}
        }
    }
    samples.sort_by_key(|s| s.ts);
    waits.sort_unstable();
    let wait_total_ms: u64 = waits.iter().sum();
    let jobs = waits.len();

    let (mut waiting_span_ms, mut idle_ms, mut cpu_ms) = (0u64, 0u64, 0u64);
    let mut reasons: BTreeMap<String, u64> = BTreeMap::new();
    for pair in samples.windows(2) {
        let (a, b) = (&pair[0], &pair[1]);
        let dt = b.ts - a.ts;
        if dt <= 0 || dt > MAX_GAP_MS || a.waiting == 0 {
            continue;
        }
        let dt = u64::try_from(dt).unwrap_or(0);
        waiting_span_ms += dt;
        cpu_ms += a.cpu_pct.min(100) * dt;
        if a.cpu_pct < IDLE_CPU_PCT && a.mem_pct < IDLE_MEM_PCT {
            idle_ms += dt;
            if a.blocked.is_empty() {
                // Nothing blocked: the waiter could have started and had not yet woken up.
                *reasons.entry("startable".into()).or_default() += dt;
            }
            for k in a.blocked.keys() {
                *reasons.entry(k.clone()).or_default() += dt;
            }
        }
    }
    let mut idle_reasons: Vec<(String, u64)> = reasons.into_iter().collect();
    idle_reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| reason_rank(&a.0).cmp(&reason_rank(&b.0))).then_with(|| a.0.cmp(&b.0)));

    let mut repo_rows: Vec<RepoRow> = repos.into_values().collect();
    repo_rows.sort_by(|a, b| b.cpu_ms.cmp(&a.cpu_ms).then_with(|| b.wait_ms.cmp(&a.wait_ms)).then_with(|| a.repo.cmp(&b.repo)));

    Report {
        since_ms,
        until_ms,
        jobs,
        wait_total_ms,
        wait_avg_ms: if jobs == 0 { 0 } else { wait_total_ms / u64::try_from(jobs).unwrap_or(1) },
        wait_p95_ms: p95(&waits),
        waiting_span_ms,
        wait_cpu_avg: (waiting_span_ms > 0).then(|| cpu_ms / waiting_span_ms),
        idle_ms,
        idle_reasons,
        repos: repo_rows,
        buckets: buckets(&samples, since_ms, until_ms),
    }
}

fn buckets(samples: &[Sample], since_ms: i64, until_ms: i64) -> Vec<Bucket> {
    let Some(first) = samples.first() else {
        return Vec::new();
    };
    let start = first.ts.max(since_ms).div_euclid(BUCKET_MS) * BUCKET_MS;
    let n = usize::try_from((until_ms - start).div_euclid(BUCKET_MS) + 1).unwrap_or(0);
    let mut acc: Vec<(u64, u64, u32, u32)> = vec![(0, 0, 0, 0); n];
    let mut seen = vec![false; n];
    for s in samples {
        let Ok(i) = usize::try_from((s.ts - start).div_euclid(BUCKET_MS)) else {
            continue;
        };
        let Some(slot) = acc.get_mut(i) else { continue };
        slot.0 += s.cpu_pct;
        slot.1 += 1;
        slot.2 = slot.2.max(s.running);
        slot.3 = slot.3.max(s.waiting);
        seen[i] = true;
    }
    acc.iter()
        .enumerate()
        .map(|(i, (sum, count, running, waiting))| {
            let start_ms = start + i64::try_from(i).unwrap_or(0) * BUCKET_MS;
            if seen[i] {
                let cpu = Some(f(*sum) / f((*count).max(1)));
                Bucket { start_ms, cpu, running: Some(*running), waiting: Some(*waiting) }
            } else {
                Bucket { start_ms, cpu: None, running: None, waiting: None }
            }
        })
        .collect()
}

/// `450ms`, `12s`, `3m07s`, `1h02m`.
#[must_use]
pub fn fmt_dur(ms: u64) -> String {
    let s = ms / 1000;
    if ms < 1000 {
        format!("{ms}ms")
    } else if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    }
}

fn pct(part: u64, whole: u64) -> String {
    if whole == 0 {
        return "0%".into();
    }
    format!("{}.{}%", part * 100 / whole, (part * 1000 / whole) % 10)
}

fn hours(ms: u64) -> String {
    format!("{}.{:02}", ms / 3_600_000, (ms % 3_600_000) * 100 / 3_600_000)
}

fn stamp(ms: i64) -> String {
    Timestamp::from_millisecond(ms).map_or_default(|t| t.strftime("%m-%d %H:%M").to_string())
}

const LEVELS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One character per value; `None` (no sample) is a dot.
#[must_use]
pub fn spark(values: &[Option<f64>], max: f64) -> String {
    let max = max.max(f64::EPSILON);
    values
        .iter()
        .map(|v| match v {
            None => '·',
            Some(x) => {
                let level = format!("{:.0}", (x / max * 7.0).clamp(0.0, 7.0)).parse::<usize>().unwrap_or(0);
                LEVELS[level.min(7)]
            }
        })
        .collect()
}

#[must_use]
pub fn render_text(r: &Report) -> String {
    let mut o = String::new();
    let _ = writeln!(o, "heavy history, {} to {} UTC", stamp(r.since_ms), stamp(r.until_ms));
    let _ = writeln!(o, "jobs {}, wait total {}, avg {}, p95 {}", r.jobs, fmt_dur(r.wait_total_ms), fmt_dur(r.wait_avg_ms), fmt_dur(r.wait_p95_ms));
    if let Some(avg) = r.wait_cpu_avg {
        let _ = writeln!(o, "avg cpu while jobs waited: {avg}%");
    }
    let _ = writeln!(
        o,
        "idle while waiting (a job waited, cpu < {IDLE_CPU_PCT}%, mem < {IDLE_MEM_PCT}%, load ignored): {} of {} waiting time ({})",
        fmt_dur(r.idle_ms),
        fmt_dur(r.waiting_span_ms),
        pct(r.idle_ms, r.waiting_span_ms)
    );
    if r.idle_ms > 0 {
        let top: Vec<String> = r.idle_reasons.iter().take(5).map(|(k, ms)| format!("{k} {}", fmt_dur(*ms))).collect();
        let _ = writeln!(o, "  top blockers in those minutes: {}", top.join(", "));
    }
    let _ = writeln!(o, "\n{:<24} {:>5} {:>10} {:>8} {:>9}", "repo", "jobs", "wait", "avg wait", "cpu-hours");
    for row in &r.repos {
        let avg = if row.jobs == 0 { 0 } else { row.wait_ms / u64::try_from(row.jobs).unwrap_or(1) };
        let _ = writeln!(o, "{:<24} {:>5} {:>10} {:>8} {:>9}", row.repo.chars().take(24).collect::<String>(), row.jobs, fmt_dur(row.wait_ms), fmt_dur(avg), hours(row.cpu_ms));
    }
    if r.buckets.is_empty() {
        let _ = writeln!(o, "\nno machine samples in this window");
        return o;
    }
    let shown = &r.buckets[r.buckets.len().saturating_sub(TEXT_BUCKETS)..];
    let note = if shown.len() < r.buckets.len() { format!(", last {} of {} (use --html for all)", shown.len(), r.buckets.len()) } else { String::new() };
    let _ = writeln!(o, "\ntimeline, 5-minute buckets{note}; cpu 0-100%, running and waiting scaled to their max");
    let max_run = shown.iter().filter_map(|b| b.running).max().unwrap_or(1).max(1);
    let max_wait = shown.iter().filter_map(|b| b.waiting).max().unwrap_or(1).max(1);
    for chunk in shown.chunks(ROW) {
        let _ = writeln!(o, "{} UTC", stamp(chunk[0].start_ms));
        let cpu: Vec<_> = chunk.iter().map(|b| b.cpu).collect();
        let run: Vec<_> = chunk.iter().map(|b| b.running.map(f64::from)).collect();
        let wait: Vec<_> = chunk.iter().map(|b| b.waiting.map(f64::from)).collect();
        let _ = writeln!(o, "  cpu     {}", spark(&cpu, 100.0));
        let _ = writeln!(o, "  running {}  (max {max_run})", spark(&run, f64::from(max_run)));
        let _ = writeln!(o, "  waiting {}  (max {max_wait})", spark(&wait, f64::from(max_wait)));
    }
    o
}

#[expect(clippy::cast_precision_loss)]
fn f(x: u64) -> f64 {
    x as f64
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

const W: f64 = 900.0;
const H: f64 = 140.0;

/// A polyline path over the buckets; a missing bucket lifts the pen.
fn path(values: &[Option<f64>], max: f64) -> String {
    let n = values.len().max(2) - 1;
    let max = max.max(f64::EPSILON);
    let mut d = String::new();
    let mut pen_down = false;
    for (i, v) in values.iter().enumerate() {
        match v {
            Some(x) => {
                let px = W * f(i as u64) / f(n as u64);
                let py = H - (x / max).clamp(0.0, 1.0) * (H - 4.0) - 2.0;
                let _ = write!(d, "{}{px:.1} {py:.1} ", if pen_down { "L" } else { "M" });
                pen_down = true;
            }
            None => pen_down = false,
        }
    }
    d
}

fn chart(title: &str, lines: &[(&str, &str, String)], max_label: &str, first: &str, last: &str, guide: Option<f64>) -> String {
    let mut s = format!(
        "<section><h2>{}</h2><svg viewBox=\"0 0 {W} {}\" role=\"img\" aria-label=\"{}\"><rect class=\"frame\" width=\"{W}\" height=\"{H}\"/>",
        esc(title),
        H + 18.0,
        esc(title)
    );
    if let Some(g) = guide {
        let y = H - g * (H - 4.0) - 2.0;
        let _ = write!(s, "<line class=\"guide\" x1=\"0\" x2=\"{W}\" y1=\"{y:.1}\" y2=\"{y:.1}\"/>");
    }
    for (cls, _, d) in lines {
        let _ = write!(s, "<path class=\"{cls}\" d=\"{d}\"/>");
    }
    let _ = write!(
        s,
        "<text x=\"4\" y=\"12\" class=\"lbl\">{}</text><text x=\"2\" y=\"{}\" class=\"lbl\">{}</text><text x=\"{}\" y=\"{}\" class=\"lbl\" text-anchor=\"end\">{}</text></svg>",
        esc(max_label),
        H + 14.0,
        esc(first),
        W - 2.0,
        H + 14.0,
        esc(last)
    );
    let legend: Vec<String> = lines.iter().map(|(cls, name, _)| format!("<span class=\"key {cls}\">{}</span>", esc(name))).collect();
    let _ = write!(s, "<p class=\"legend\">{}</p></section>", legend.join(" "));
    s
}

/// One self-contained HTML page: inline SVG, no scripts, light and dark.
#[must_use]
pub fn render_html(r: &Report) -> String {
    let cpu: Vec<_> = r.buckets.iter().map(|b| b.cpu).collect();
    let run: Vec<_> = r.buckets.iter().map(|b| b.running.map(f64::from)).collect();
    let wait: Vec<_> = r.buckets.iter().map(|b| b.waiting.map(f64::from)).collect();
    let max_n = r.buckets.iter().flat_map(|b| [b.running, b.waiting]).flatten().max().unwrap_or(1).max(1);
    let (first, last) = (r.buckets.first().map_or_default(|b| stamp(b.start_ms)), r.buckets.last().map_or_default(|b| stamp(b.start_ms + BUCKET_MS)));
    let mut h = String::from(concat!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>heavy history</title><style>",
        ":root{--bg:#fff;--fg:#1b1f24;--mut:#667;--line:#d7dbe0;--cpu:#2563eb;--run:#16a34a;--wait:#d97706;--bad:#dc2626}",
        "@media(prefers-color-scheme:dark){:root{--bg:#14171a;--fg:#e6e8ea;--mut:#9aa3ad;--line:#2d333a;--cpu:#60a5fa;--run:#4ade80;--wait:#fbbf24;--bad:#f87171}}",
        "body{margin:0 auto;max-width:960px;padding:16px;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,sans-serif}",
        "h1{font-size:1.4rem;margin:0 0 4px}h2{font-size:1.05rem;margin:22px 0 6px}.mut,.lbl,.legend{color:var(--mut)}.lbl{fill:var(--mut);font-size:11px}",
        "svg{width:100%;height:auto;display:block}.frame{fill:none;stroke:var(--line)}.guide{stroke:var(--bad);stroke-dasharray:4 4;opacity:.6}",
        "path{fill:none;stroke-width:1.6;stroke-linejoin:round}path.cpu{stroke:var(--cpu)}path.run{stroke:var(--run)}path.wait{stroke:var(--wait)}",
        ".key::before{content:'';display:inline-block;width:14px;height:3px;margin-right:5px;vertical-align:middle;background:currentColor}.key.cpu::before{background:var(--cpu)}.key.run::before{background:var(--run)}.key.wait::before{background:var(--wait)}",
        ".tiles{display:grid;grid-template-columns:repeat(auto-fit,minmax(150px,1fr));gap:10px}.tile{border:1px solid var(--line);border-radius:8px;padding:8px 12px}.tile b{display:block;font-size:1.3rem}",
        "table{border-collapse:collapse;width:100%}td,th{padding:4px 8px;border-bottom:1px solid var(--line);text-align:right}td:first-child,th:first-child{text-align:left}",
        ".bar{fill:var(--bad)}.barbg{fill:var(--line)}",
        "</style></head><body>"
    ));
    let _ = write!(h, "<h1>heavy history</h1><p class=\"mut\">{} to {} UTC</p>", stamp(r.since_ms), stamp(r.until_ms));
    let _ = write!(
        h,
        "<div class=\"tiles\"><div class=\"tile\"><b>{}</b>jobs</div><div class=\"tile\"><b>{}</b>wait total</div><div class=\"tile\"><b>{}</b>avg wait</div><div class=\"tile\"><b>{}</b>p95 wait</div><div class=\"tile\"><b>{}</b>avg cpu while jobs waited</div><div class=\"tile\"><b>{}</b>idle while waiting ({} of {})</div></div>",
        r.jobs,
        fmt_dur(r.wait_total_ms),
        fmt_dur(r.wait_avg_ms),
        fmt_dur(r.wait_p95_ms),
        r.wait_cpu_avg.map_or_else(|| "-".to_string(), |c| format!("{c}%")),
        pct(r.idle_ms, r.waiting_span_ms),
        fmt_dur(r.idle_ms),
        fmt_dur(r.waiting_span_ms)
    );
    let _ = write!(
        h,
        "<p class=\"mut\">Idle while waiting: a job waited although cpu &lt; {IDLE_CPU_PCT}%, memory &lt; {IDLE_MEM_PCT}% (load ignored: on macOS it counts threads waiting on I/O). Only these minutes can be heavy's fault.</p>"
    );
    h.push_str(&chart("CPU used by the whole machine (5-minute buckets)", &[("cpu", "cpu %", path(&cpu, 100.0))], "100%", &first, &last, Some(f(IDLE_CPU_PCT) / 100.0)));
    h.push_str(&chart(
        "Jobs running and waiting",
        &[("run", "running", path(&run, f64::from(max_n))), ("wait", "waiting", path(&wait, f64::from(max_n)))],
        &format!("max {max_n}"),
        &first,
        &last,
        None,
    ));
    h.push_str("<section><h2>Blockers while idle</h2>");
    if r.idle_reasons.is_empty() {
        h.push_str("<p class=\"mut\">None: heavy never waited with room on the machine in this window.</p>");
    } else {
        let top = r.idle_reasons.first().map_or(1, |(_, ms)| *ms).max(1);
        let _ = write!(h, "<svg viewBox=\"0 0 {W} {}\" role=\"img\" aria-label=\"blocking reasons during idle minutes\">", r.idle_reasons.len() * 24 + 4);
        for (i, (k, ms)) in r.idle_reasons.iter().enumerate() {
            let y = i * 24 + 2;
            let w = 600.0 * f(*ms) / f(top);
            let _ = write!(
                h,
                "<text x=\"0\" y=\"{}\" class=\"lbl\">{}</text><rect class=\"barbg\" x=\"90\" y=\"{y}\" width=\"600\" height=\"16\"/><rect class=\"bar\" x=\"90\" y=\"{y}\" width=\"{w:.1}\" height=\"16\"/><text x=\"700\" y=\"{}\" class=\"lbl\">{}</text>",
                y + 13,
                esc(k),
                y + 13,
                fmt_dur(*ms)
            );
        }
        h.push_str("</svg>");
    }
    h.push_str("</section><section><h2>Per repo</h2><table><tr><th>repo</th><th>jobs</th><th>wait</th><th>avg wait</th><th>cpu-hours</th></tr>");
    for row in &r.repos {
        let avg = if row.jobs == 0 { 0 } else { row.wait_ms / u64::try_from(row.jobs).unwrap_or(1) };
        let _ = write!(h, "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>", esc(&row.repo), row.jobs, fmt_dur(row.wait_ms), fmt_dur(avg), hours(row.cpu_ms));
    }
    h.push_str("</table></section></body></html>\n");
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admit::Block;
    use crate::log::{JobInfo, ReasonTime};

    const MIN: i64 = 60_000;

    fn sample(ts: i64, cpu: u64, mem: u64, load1: f64, waiting: u32, blocked: &[(&str, u32)]) -> Event {
        Event::Sample {
            ts: Timestamp::from_millisecond(ts).unwrap(),
            cpu_pct: cpu,
            mem_pct: mem,
            load1,
            load5: load1,
            ncpu: 8,
            running: 1,
            waiting,
            blocked: blocked.iter().map(|(k, n)| ((*k).to_string(), *n)).collect(),
        }
    }

    fn job(repo: &str) -> JobInfo {
        JobInfo { id: "j".into(), pid: 1, cwd: "/".into(), repo: Some(repo.into()), command: "cargo test".into(), cpu: 4, mem: 1 }
    }

    #[test]
    fn idle_while_waiting_counts_only_intervals_with_room() {
        // Intervals start at their sample: [0,30s) busy cpu, [30s,60s) idle (ramp), [60s,90s) idle (queue+cpu),
        // [90s,120s) nobody waiting, [120s,150s) mem high, [150s,180s) load high but cpu idle (still idle: load is
        // ignored), gap of 10 min, then one more.
        let s = [
            sample(0, 80, 50, 1.0, 1, &[("cpu", 1)]),
            sample(30_000, 30, 50, 1.0, 1, &[("ramp", 1)]),
            sample(60_000, 30, 50, 1.0, 2, &[("queue", 1), ("cpu", 1)]),
            sample(90_000, 10, 10, 0.1, 0, &[]),
            sample(120_000, 10, 90, 0.1, 1, &[("mem", 1)]),
            sample(150_000, 10, 10, 16.0, 1, &[("load1", 1)]),
            sample(180_000, 10, 10, 1.0, 1, &[]),
            sample(180_000 + 10 * MIN, 10, 10, 1.0, 1, &[]),
        ];
        let r = compute(&s, 0, 180_000 + 10 * MIN);
        // Waiting intervals: 0,30,60,120,150 (30 s each) and 180 -> gap of 10 min is skipped.
        assert_eq!(r.waiting_span_ms, 150_000);
        // Idle: the ones at 30 s, 60 s and 150 s.
        assert_eq!(r.idle_ms, 90_000);
        let want = [("ramp", 30_000), ("cpu", 30_000), ("load1", 30_000), ("queue", 30_000)].map(|(k, ms)| (k.to_string(), ms));
        assert_eq!(r.idle_reasons, want);
        assert_eq!(pct(r.idle_ms, r.waiting_span_ms), "60.0%");
        // CPU over the waiting intervals: (80 + 30 + 30 + 10 + 10) / 5.
        assert_eq!(r.wait_cpu_avg, Some(32));
        assert!(render_text(&r).contains("avg cpu while jobs waited: 32%"));
    }

    #[test]
    fn seventy_percent_cpu_while_waiting_is_underused() {
        let s = [sample(0, 74, 40, 30.0, 1, &[("cpu", 1)]), sample(30_000, 75, 40, 30.0, 1, &[("cpu", 1)]), sample(60_000, 5, 5, 0.1, 0, &[])];
        let r = compute(&s, 0, 60_000);
        assert_eq!((r.idle_ms, r.waiting_span_ms), (30_000, 60_000));
        assert_eq!(compute(&[sample(0, 5, 5, 0.1, 0, &[])], 0, 1).wait_cpu_avg, None);
    }

    #[test]
    fn a_tie_ranks_a_specific_reason_before_queue() {
        let s = [sample(0, 5, 5, 0.1, 2, &[("queue", 1), ("ramp", 1)]), sample(30_000, 5, 5, 0.1, 0, &[])];
        let r = compute(&s, 0, 60_000);
        assert_eq!(r.idle_reasons, vec![("ramp".to_string(), 30_000), ("queue".to_string(), 30_000)]);
    }

    #[test]
    fn idle_with_nothing_blocking_is_startable() {
        let s = [sample(0, 5, 5, 0.1, 1, &[]), sample(30_000, 5, 5, 0.1, 0, &[])];
        let r = compute(&s, 0, 60_000);
        assert_eq!((r.idle_ms, r.idle_reasons), (30_000, vec![("startable".to_string(), 30_000)]));
    }

    #[test]
    fn totals_percentile_and_repos() {
        let t = |ms: i64| Timestamp::from_millisecond(ms).unwrap();
        let mut ev = Vec::new();
        for (i, w) in [100u64, 200, 300, 400, 5000].iter().enumerate() {
            let repo = if i < 4 { "a" } else { "b" };
            ev.push(Event::Started { ts: t(10), job: job(repo), wait_ms: *w, reasons: vec![ReasonTime { reason: Block::Cpu, ms: *w }] });
            ev.push(Event::Finished { ts: t(20), job: job(repo), exit: 0, wall_ms: 1, cpu_ms: 3_600_000, peak_rss: 1 });
        }
        let r = compute(&ev, 0, 100);
        assert_eq!((r.jobs, r.wait_total_ms, r.wait_avg_ms, r.wait_p95_ms), (5, 6000, 1200, 5000));
        assert_eq!(r.repos.len(), 2);
        assert_eq!((r.repos[0].repo.as_str(), r.repos[0].jobs, r.repos[0].wait_ms, r.repos[0].cpu_ms), ("a", 4, 1000, 4 * 3_600_000));
        assert!(render_text(&r).contains("a "));
        assert_eq!(hours(5_400_000), "1.50");
        // Events outside the window are ignored.
        assert_eq!(compute(&ev, 11, 100).jobs, 0);
    }

    #[test]
    fn buckets_and_sparks() {
        let s = [sample(0, 100, 5, 0.1, 0, &[]), sample(30_000, 0, 5, 0.1, 2, &[]), sample(2 * BUCKET_MS, 50, 5, 0.1, 0, &[])];
        let r = compute(&s, 0, 3 * BUCKET_MS);
        assert_eq!(r.buckets.len(), 4);
        assert_eq!(r.buckets[0].cpu, Some(50.0));
        assert_eq!(r.buckets[0].waiting, Some(2));
        assert_eq!(r.buckets[1].cpu, None);
        assert_eq!(spark(&[Some(0.0), Some(50.0), Some(100.0), None], 100.0), "▁▅█·");
        let html = render_html(&r);
        assert!(html.contains("<svg") && !html.contains("<script"));
        let text = render_text(&r);
        assert!(text.contains("cpu") && text.contains("waiting"));
    }

    #[test]
    fn durations_format() {
        assert_eq!((fmt_dur(450).as_str(), fmt_dur(12_000).as_str(), fmt_dur(187_000).as_str(), fmt_dur(3_720_000).as_str()), ("450ms", "12s", "3m07s", "1h02m"));
    }
}
