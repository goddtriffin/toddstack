//! `shakedown.toml`: parse and validate. Sections later phases use are accepted and reported as unused.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::Error;

/// The sections this version reads. Everything else in the design is accepted but ignored.
#[derive(Debug, Deserialize)]
pub struct Config {
    /// The tool version the repo expects.
    pub shakedown: String,
    /// Project name.
    pub project: String,
    /// The local gate.
    #[serde(default)]
    pub check: Check,
    /// Budgets.
    #[serde(default)]
    pub budgets: Budgets,
    /// The local server `ui` draws against.
    pub server: Option<Server>,
    /// Which devices and themes.
    #[serde(default)]
    pub devices: Devices,
    /// Seeded local accounts.
    pub players: Option<Players>,
    /// Where the approved references live.
    #[serde(default)]
    pub ui: Ui,
    /// Layout rule opt-outs.
    #[serde(default)]
    pub layout: Layout,
    /// The screens.
    #[serde(default)]
    pub screen: Vec<Screen>,
    /// The sweep's routes and groups.
    pub sweep: Option<Sweep>,
    /// The production-image smoke test.
    pub smoke: Option<crate::smoke::Smoke>,
    /// Sections owned by later phases.
    #[serde(flatten)]
    pub unused: BTreeMap<String, toml::Value>,
}

/// `[server]`: the local server that `ui` draws against.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    /// Starts the server and stays in the foreground. `{port}` is a free port, `{db}` a per-checkout database name.
    /// `SHAKEDOWN_PORT`, `SHAKEDOWN_DB`, `SHAKEDOWN_BASE_URL` and `SHAKEDOWN_NOW` are also set.
    pub start: String,
    /// A path that answers once the server is up.
    #[serde(default = "default_ready")]
    pub ready: String,
    /// `fake`: every `/api` call is answered from fixtures. `seeded`: the real server and database.
    #[serde(default)]
    pub world: World,
    /// Runs once the server is ready (seeded worlds). Same placeholders as `start`.
    pub seed: Option<String>,
    /// Drops the throwaway database `{db}` (idempotent: a missing one is not an error). Run after the server stops on
    /// every way a run can end, and for each leftover `list_dbs` names. Needed whenever `start` creates the database.
    pub drop_db: Option<String>,
    /// Prints the names of this machine's throwaway databases, one per line. At startup, the ones named
    /// `shakedown_<project>_<hash>_p<pid>` whose process is dead are dropped with `drop_db`: leftovers of a run
    /// that was killed. A run alive (its pid is running) is never touched.
    pub list_dbs: Option<String>,
    /// The fixed clock, RFC 3339 (given to the server and frozen in the page).
    #[serde(default = "default_now")]
    pub now: String,
    /// Paths starting with this are the API: in a fake world every call must be faked.
    #[serde(default = "default_api")]
    pub api: String,
    /// Seconds to wait for the server to be ready.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
}

fn default_ready() -> String {
    "/".into()
}
fn default_now() -> String {
    "2026-01-15T12:00:00Z".into()
}
fn default_api() -> String {
    "/api/".into()
}
const fn default_timeout() -> u64 {
    120
}

/// The two world modes.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum World {
    /// Every `/api` call answered from fixtures; an unfaked call fails the screen.
    #[default]
    Fake,
    /// The real local server and its database.
    Seeded,
}

/// `[devices]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Devices {
    /// Devices with committed references.
    #[serde(default = "default_ui_devices")]
    pub ui: Vec<String>,
    /// Devices the sweep covers: `"all"` (default) or a list of names.
    pub sweep: Option<DeviceSet>,
    /// `light` and `dark`.
    #[serde(default = "default_themes")]
    pub themes: Vec<String>,
}

impl Default for Devices {
    fn default() -> Self {
        Self {
            ui: default_ui_devices(),
            sweep: None,
            themes: default_themes(),
        }
    }
}

/// `"all"`, or named devices.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum DeviceSet {
    /// Only `"all"` is valid.
    Word(String),
    /// Named devices.
    List(Vec<String>),
}

impl Devices {
    /// The devices the sweep draws, by name.
    pub fn sweep_names(&self) -> Vec<String> {
        match &self.sweep {
            Some(DeviceSet::List(l)) => l.clone(),
            _ => crate::devices::all().into_iter().map(|d| d.name).collect(),
        }
    }
}

/// `[sweep]`: what `shakedown sweep` draws on every device and theme.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sweep {
    /// Pages: a path, or a table to set the player, fixtures and expected console messages.
    #[serde(default)]
    pub routes: Vec<SweepRoute>,
    /// Pages every frame of which is its own player (one lobby or game seen from several accounts).
    #[serde(default)]
    pub groups: Vec<SweepGroup>,
}

/// One swept page.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum SweepRoute {
    /// A path, as a guest.
    Path(String),
    /// A path with options.
    Page(SweepPage),
}

/// A swept page with options.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepPage {
    /// The path to open.
    pub path: String,
    /// Its name in the pack (default: made from the path).
    pub name: Option<String>,
    /// `guest` (default) or one of `[players].accounts`.
    #[serde(default = "default_player")]
    pub player: String,
    /// Whether the page counts against the player's `[players].parallel` turns (default true). Set `false` for a
    /// page that opens none of the connections the server caps per person (accounts-service: only its friends page
    /// opens an event stream), so its shots never wait for a turn.
    #[serde(default = "default_turn")]
    pub turn: bool,
    /// Fixtures, fake worlds only (as in a screen).
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
    /// Console messages that are expected, with the reason.
    #[serde(default)]
    pub allow_console: Vec<AllowConsole>,
}

/// A swept page shown to several signed-in players.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SweepGroup {
    /// Its name in the pack.
    pub name: String,
    /// The path to open.
    pub path: String,
    /// How many accounts take turns (frame `i` is `accounts[i mod players]`).
    pub players: usize,
    /// Fixtures, fake worlds only.
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
    /// Console messages that are expected, with the reason.
    #[serde(default)]
    pub allow_console: Vec<AllowConsole>,
}

fn default_ui_devices() -> Vec<String> {
    vec!["phone".into(), "tablet".into(), "desktop".into()]
}
fn default_themes() -> Vec<String> {
    vec!["light".into(), "dark".into()]
}

/// `[players]`: seeded local accounts, never production.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Players {
    /// Account names; `guest` is always available and signs nothing in.
    pub accounts: Vec<String>,
    /// A path on the server that signs the browser in; `{player}` is the account.
    pub signin: String,
    /// How many shots one signed-in player is in at once (default 3): servers cap a person's open connections.
    #[serde(default = "default_parallel")]
    pub parallel: usize,
}

const fn default_parallel() -> usize {
    3
}

/// `[ui]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ui {
    /// The references directory, from the repo root; web references are in its `web/`.
    #[serde(default = "default_refs")]
    pub refs: String,
}

impl Default for Ui {
    fn default() -> Self {
        Self {
            refs: default_refs(),
        }
    }
}

fn default_refs() -> String {
    "ui/refs".into()
}

/// `[layout]`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// Opt-outs by CSS selector. Every one needs a reason.
    #[serde(default)]
    pub ignore: Vec<LayoutIgnore>,
}

/// One opt-out.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutIgnore {
    /// The rule to switch off.
    pub rule: String,
    /// A CSS selector.
    #[serde(rename = "match")]
    pub selector: String,
    /// Why this is fine.
    pub why: String,
}

/// `[[screen]]`: one page state, drawn on every `ui` device and theme.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Screen {
    /// Unique; lower case letters, digits and `-`.
    pub name: String,
    /// The path to open.
    pub path: String,
    /// `guest` (default) or one of `[players].accounts`.
    #[serde(default = "default_player")]
    pub player: String,
    /// Whether the page counts against the player's `[players].parallel` turns (default true). Set `false` for a
    /// page that opens none of the connections the server caps per person (accounts-service: only its friends page
    /// opens an event stream), so its shots never wait for a turn.
    #[serde(default = "default_turn")]
    pub turn: bool,
    /// `"METHOD /path"` (or with `?query`) to the fixture that answers it.
    #[serde(default)]
    pub routes: BTreeMap<String, Route>,
    /// What to do before the shot.
    #[serde(default)]
    pub act: Vec<Action>,
    /// Only these devices (default: `[devices].ui`).
    pub devices: Option<Vec<String>>,
    /// Console messages that are expected, with the reason.
    #[serde(default)]
    pub allow_console: Vec<AllowConsole>,
}

const fn default_turn() -> bool {
    true
}

fn default_player() -> String {
    "guest".into()
}

/// A fixture: a path, or a path with a status.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Route {
    /// A JSON file, from the repo root.
    File(String),
    /// A JSON file answered with a status.
    Detailed {
        /// The fixture.
        file: String,
        /// HTTP status (default 200).
        status: Option<u16>,
    },
}

/// A console message the screen expects.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowConsole {
    /// Part of the message.
    pub contains: String,
    /// Why it is fine.
    pub why: String,
}

/// The small fixed action list (design section 4).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Click the first element matching a selector.
    Click(String),
    /// Focus a field and type.
    Type(TypeText),
    /// Press a key (`Enter`, `Tab`, `Escape`).
    Press(String),
    /// Wait until the page's text contains this.
    WaitText(String),
    /// Wait until nothing matches this selector.
    WaitGone(String),
    /// Scroll a selector into view, or by pixels.
    Scroll(ScrollTo),
}

/// `type = { selector, text }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypeText {
    /// The field.
    pub selector: String,
    /// What to type.
    pub text: String,
}

/// Where `scroll` goes.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ScrollTo {
    /// Into view.
    Selector(String),
    /// Down by pixels.
    Pixels(i64),
}

/// `[check]`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    /// Shell steps.
    #[serde(default)]
    pub steps: Vec<Step>,
    /// Tool-run steps that join the same graph (`budgets.sizes`).
    #[serde(default)]
    pub builtin: Vec<String>,
}

/// One shell step.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Unique name.
    pub name: String,
    /// The command, run with `sh -c` in the directory holding `shakedown.toml`.
    pub run: String,
    /// Steps that must pass first.
    #[serde(default)]
    pub after: Vec<String>,
    /// Runs only when named by `--only`.
    #[serde(default)]
    pub manual: bool,
}

/// `[budgets]`.
#[derive(Debug, Default, Deserialize)]
pub struct Budgets {
    /// Exact byte ceilings per artifact, keyed by label.
    #[serde(default)]
    pub sizes: BTreeMap<String, SizeBudget>,
}

/// One artifact's ceilings. `file` may hold one `*` in its last component (hashed names).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SizeBudget {
    /// Path from the repo root.
    pub file: String,
    /// Raw bytes ceiling.
    pub raw: Option<u64>,
    /// Gzip level 9 ceiling.
    pub gzip: Option<u64>,
    /// Brotli 11 ceiling.
    pub brotli: Option<u64>,
    /// Measure the file after `strip -S -x` (static libraries).
    #[serde(default)]
    pub strip: bool,
    /// Ceiling for the raw (or stripped) size.
    pub max: Option<u64>,
}

impl Config {
    /// Reads and validates the file and the files it includes.
    ///
    /// # Errors
    ///
    /// Unreadable file, bad TOML, a version mismatch, or a step graph that is not valid.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
        Self::parse_in(
            &text,
            env!("CARGO_PKG_VERSION"),
            dir.unwrap_or(Path::new(".")),
        )
    }

    /// Parses text with no includes, with the running tool version passed in so tests can vary it.
    ///
    /// # Errors
    ///
    /// See [`Config::load`].
    #[cfg(test)]
    pub fn parse(text: &str, tool_version: &str) -> Result<Self, Error> {
        Self::parse_in(text, tool_version, Path::new("."))
    }

    /// Like [`Config::parse`], resolving `include` patterns from `dir`.
    ///
    /// # Errors
    ///
    /// See [`Config::load`].
    pub fn parse_in(text: &str, tool_version: &str, dir: &Path) -> Result<Self, Error> {
        let mut table: toml::Table =
            toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        merge_includes(&mut table, dir)?;
        let config: Self = toml::Value::Table(table)
            .try_into()
            .map_err(|e: toml::de::Error| Error::Config(e.to_string()))?;
        if config.shakedown != tool_version {
            return Err(Error::Config(format!(
                "shakedown.toml expects shakedown {}, this is {tool_version}; run: cargo install shakedown@{} --locked",
                config.shakedown, config.shakedown
            )));
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), Error> {
        let mut names = BTreeSet::new();
        for step in &self.check.steps {
            if step.name.is_empty() || step.run.trim().is_empty() {
                return Err(Error::Config("every step needs a name and a run".into()));
            }
            if !names.insert(step.name.as_str()) {
                return Err(Error::Config(format!(
                    "duplicate step name {:?}",
                    step.name
                )));
            }
        }
        for step in &self.check.steps {
            for dep in &step.after {
                if !names.contains(dep.as_str()) {
                    return Err(Error::Config(format!(
                        "step {:?} runs after unknown step {dep:?}",
                        step.name
                    )));
                }
            }
        }
        for b in &self.check.builtin {
            if b != "budgets.sizes" && b != "ui.web" {
                return Err(Error::Config(format!(
                    "unknown builtin step {b:?} (this version has: budgets.sizes, ui.web)"
                )));
            }
        }
        self.validate_ui()?;
        if let Some(s) = &self.smoke {
            s.validate(self.sweep.is_some())?;
        }
        for (label, size) in &self.budgets.sizes {
            if size.strip && size.max.is_none() {
                return Err(Error::Config(format!("budget {label:?}: strip needs max")));
            }
            if size.raw.is_none()
                && size.gzip.is_none()
                && size.brotli.is_none()
                && size.max.is_none()
            {
                return Err(Error::Config(format!("budget {label:?} sets no ceiling")));
            }
        }
        crate::steps::order(&self.check.steps)?;
        Ok(())
    }

    fn validate_ui(&self) -> Result<(), Error> {
        let bad = |m: String| Err(Error::Config(m));
        crate::devices::pick(&self.devices.ui)?;
        if self.devices.themes.is_empty()
            || self
                .devices
                .themes
                .iter()
                .any(|t| t != "light" && t != "dark")
        {
            return bad("[devices].themes must be \"light\", \"dark\" or both".into());
        }
        let mut names = BTreeSet::new();
        for s in &self.screen {
            let ok = !s.name.is_empty()
                && s.name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            if !ok {
                return bad(format!(
                    "screen name {:?}: use lower case letters, digits and -",
                    s.name
                ));
            }
            if !names.insert(s.name.as_str()) {
                return bad(format!("duplicate screen name {:?}", s.name));
            }
            if !s.path.starts_with('/') {
                return bad(format!("screen {:?}: path must start with /", s.name));
            }
            if let Some(d) = &s.devices {
                crate::devices::pick(d)?;
            }
            if s.player != "guest" {
                match &self.players {
                    Some(p) if p.accounts.contains(&s.player) => {}
                    _ => {
                        return bad(format!(
                            "screen {:?}: player {:?} is not in [players].accounts",
                            s.name, s.player
                        ));
                    }
                }
            }
            for key in s.routes.keys() {
                if key
                    .split_once(' ')
                    .is_none_or(|(m, p)| m.is_empty() || !p.starts_with('/'))
                {
                    return bad(format!(
                        "screen {:?}: route {key:?} must look like \"GET /api/v1/me\"",
                        s.name
                    ));
                }
            }
            for c in &s.allow_console {
                if c.why.trim().is_empty() {
                    return bad(format!("screen {:?}: allow_console needs a why", s.name));
                }
            }
        }
        for ig in &self.layout.ignore {
            if !crate::layout::RULES.contains(&ig.rule.as_str()) {
                return bad(format!(
                    "[layout].ignore: unknown rule {:?} (rules: {})",
                    ig.rule,
                    crate::layout::RULES.join(", ")
                ));
            }
            if ig.why.trim().is_empty() {
                return bad(format!(
                    "[layout].ignore {:?}: every ignore needs a why",
                    ig.rule
                ));
            }
        }
        self.validate_sweep()?;
        if let Some(p) = &self.players
            && p.parallel == 0
        {
            return bad("[players].parallel must be at least 1".into());
        }
        if let Some(p) = &self.players
            && !p.signin.contains("{player}")
        {
            return bad("[players].signin must contain {player}".into());
        }
        if self.check.builtin.iter().any(|b| b == "ui.web") {
            self.require_ui()?;
        }
        Ok(())
    }

    fn validate_sweep(&self) -> Result<(), Error> {
        let bad = |m: String| Err(Error::Config(m));
        match &self.devices.sweep {
            Some(DeviceSet::Word(w)) if w != "all" => {
                return bad(format!(
                    "[devices].sweep {w:?}: use \"all\" or a list of device names"
                ));
            }
            Some(DeviceSet::List(l)) => {
                if l.is_empty() {
                    return bad("[devices].sweep is empty".into());
                }
                crate::devices::pick(l)?;
            }
            _ => {}
        }
        let Some(sweep) = &self.sweep else {
            return Ok(());
        };
        let mut names = BTreeSet::new();
        let known = |player: &str| {
            player == "guest"
                || self
                    .players
                    .as_ref()
                    .is_some_and(|p| p.accounts.iter().any(|a| a == player))
        };
        for r in &sweep.routes {
            let (path, name, player, allow) = match r {
                SweepRoute::Path(p) => (p.as_str(), None, "guest", &[][..]),
                SweepRoute::Page(p) => (
                    p.path.as_str(),
                    p.name.as_deref(),
                    p.player.as_str(),
                    p.allow_console.as_slice(),
                ),
            };
            if !path.starts_with('/') {
                return bad(format!("[sweep] route {path:?}: path must start with /"));
            }
            if !known(player) {
                return bad(format!(
                    "[sweep] route {path:?}: player {player:?} is not in [players].accounts"
                ));
            }
            if allow.iter().any(|c| c.why.trim().is_empty()) {
                return bad(format!("[sweep] route {path:?}: allow_console needs a why"));
            }
            let name = crate::sweep::page_name(name, path);
            if !names.insert(name.clone()) {
                return bad(format!(
                    "[sweep] two pages are named {name:?}: give one a name"
                ));
            }
        }
        for g in &sweep.groups {
            if !g.path.starts_with('/') {
                return bad(format!(
                    "[sweep] group {:?}: path must start with /",
                    g.name
                ));
            }
            let accounts = self.players.as_ref().map_or(0, |p| p.accounts.len());
            if g.players == 0 || g.players > accounts {
                return bad(format!(
                    "[sweep] group {:?}: players = {} but [players].accounts has {accounts} (every frame needs its own account, so a group cannot ask for more than there are)",
                    g.name, g.players
                ));
            }
            let name = crate::sweep::page_name(Some(&g.name), &g.path);
            if !names.insert(name.clone()) {
                return bad(format!("[sweep] two pages are named {name:?}"));
            }
        }
        Ok(())
    }

    /// `sweep` without `--base` needs a server.
    ///
    /// # Errors
    ///
    /// There is no `[server]`.
    pub fn require_server(&self) -> Result<(), Error> {
        if self.server.is_none() {
            return Err(Error::Config(
                "sweep needs a [server] section in shakedown.toml (or --base URL of a running local server)".into(),
            ));
        }
        Ok(())
    }

    /// `ui` needs a server and at least one screen.
    ///
    /// # Errors
    ///
    /// Either is missing.
    pub fn require_ui(&self) -> Result<(), Error> {
        if self.server.is_none() {
            return Err(Error::Config(
                "ui needs a [server] section in shakedown.toml".into(),
            ));
        }
        if self.screen.is_empty() {
            return Err(Error::Config(
                "ui needs at least one [[screen]] in shakedown.toml".into(),
            ));
        }
        Ok(())
    }

    /// Names of top-level sections this version does not read, sorted.
    pub fn unused_sections(&self) -> Vec<String> {
        self.unused.keys().cloned().collect()
    }
}

/// Appends the `[[screen]]` entries of every included file. Included files hold screens and nothing else.
fn merge_includes(table: &mut toml::Table, dir: &Path) -> Result<(), Error> {
    let patterns: Vec<String> = match table.get("include").cloned() {
        None => return Ok(()),
        Some(v) => v
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
            .ok_or_else(|| Error::Config("include must be a list of paths".into()))?,
    };
    table.remove("include");
    let mut screens: Vec<toml::Value> = table
        .remove("screen")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();
    for pattern in &patterns {
        let files = expand(dir, pattern)?;
        if files.is_empty() {
            return Err(Error::Config(format!(
                "include {pattern:?} matches no file"
            )));
        }
        for file in files {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| Error::Config(format!("{}: {e}", file.display())))?;
            let inc: toml::Table = toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", file.display())))?;
            for (key, value) in inc {
                if key != "screen" {
                    return Err(Error::Config(format!(
                        "{}: included files hold [[screen]] entries only, not {key:?}",
                        file.display()
                    )));
                }
                if let Some(a) = value.as_array() {
                    screens.extend(a.iter().cloned());
                }
            }
        }
    }
    table.insert("screen".into(), toml::Value::Array(screens));
    Ok(())
}

/// A path pattern with at most one `*` in its last component, matching files, sorted.
fn expand(dir: &Path, pattern: &str) -> Result<Vec<PathBuf>, Error> {
    let full = dir.join(pattern);
    let name = full
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::Config(format!("include {pattern:?}: not a file pattern")))?
        .to_string();
    if full
        .parent()
        .is_some_and(|p| p.to_string_lossy().contains('*'))
    {
        return Err(Error::Config(format!(
            "include {pattern:?}: `*` is allowed in the file name only"
        )));
    }
    let Some((pre, post)) = name.split_once('*') else {
        return Ok(vec![full]);
    };
    let parent = full.parent().unwrap_or(dir);
    let mut found: Vec<PathBuf> = std::fs::read_dir(parent)
        .map_err(|e| Error::Config(format!("include {pattern:?}: {e}")))?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.len() >= pre.len() + post.len() && n.starts_with(pre) && n.ends_with(post)
                })
        })
        .collect();
    found.sort();
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = r#"
shakedown = "0.4.2"
project = "p"
[check]
steps = [
  { name = "a", run = "true" },
  { name = "b", run = "true", after = ["a"] },
]
builtin = ["budgets.sizes"]
[budgets.sizes]
"x gzip" = { file = "a.js", gzip = 10 }
[simulators]
ios = []
"#;

    #[test]
    fn parses_and_reports_unused() {
        let c = Config::parse(OK, "0.4.2").unwrap();
        assert_eq!(c.check.steps.len(), 2);
        assert_eq!(c.unused_sections(), vec!["simulators"]);
    }

    #[test]
    fn version_mismatch_names_install_command() {
        let e = Config::parse(OK, "0.3.0").unwrap_err().to_string();
        assert!(e.contains("cargo install shakedown@0.4.2 --locked"), "{e}");
    }

    #[test]
    fn rejects_bad_graphs() {
        let dup = OK.replace(r#"name = "b""#, r#"name = "a""#);
        assert!(Config::parse(&dup, "0.4.2").is_err());
        let unknown = OK.replace(r#"["a"]"#, r#"["zzz"]"#);
        assert!(Config::parse(&unknown, "0.4.2").is_err());
        let cycle = OK.replace(
            r#"{ name = "a", run = "true" }"#,
            r#"{ name = "a", run = "true", after = ["b"] }"#,
        );
        assert!(
            Config::parse(&cycle, "0.4.2")
                .unwrap_err()
                .to_string()
                .contains("cycle")
        );
    }

    #[test]
    fn strip_needs_max() {
        let bad = OK.replace(
            "{ file = \"a.js\", gzip = 10 }",
            "{ file = \"a.js\", strip = true, raw = 10 }",
        );
        assert!(
            Config::parse(&bad, "0.4.2")
                .unwrap_err()
                .to_string()
                .contains("strip needs max")
        );
    }

    #[test]
    fn rejects_typos_in_known_sections() {
        let bad = OK.replace("gzip = 10", "gzp = 10");
        assert!(Config::parse(&bad, "0.4.2").is_err());
    }

    const UI: &str = r#"
shakedown = "0.4.2"
project = "p"
[server]
start = "serve {port}"
[players]
accounts = ["ann"]
signin = "/dev?as={player}"
[layout]
ignore = [{ rule = "clipped-text", match = ".m", why = "scrolls" }]
[[screen]]
name = "home"
path = "/"
[[screen]]
name = "menu"
path = "/"
player = "ann"
routes = { "GET /api/me" = "me.json", "GET /api/x" = { file = "x.json", status = 404 } }
act = [{ click = "button" }, { type = { selector = "input", text = "hi" } }, { press = "Enter" }, { wait_text = "ok" }, { wait_gone = ".spin" }, { scroll = "footer" }, { scroll = 200 }]
"#;

    #[test]
    fn parses_screens_actions_and_defaults() {
        let c = Config::parse(UI, "0.4.2").unwrap();
        assert_eq!(c.screen.len(), 2);
        assert_eq!(c.screen[0].player, "guest");
        assert_eq!(c.screen[1].act.len(), 7);
        assert_eq!(c.devices.ui, ["phone", "tablet", "desktop"]);
        assert_eq!(c.devices.themes, ["light", "dark"]);
        let s = c.server.unwrap();
        assert_eq!(
            (s.world, s.ready.as_str(), s.api.as_str()),
            (World::Fake, "/", "/api/")
        );
    }

    #[test]
    fn rejects_bad_ui_config() {
        let bad = |from: &str, to: &str| {
            Config::parse(&UI.replace(from, to), "0.4.2")
                .unwrap_err()
                .to_string()
        };
        assert!(bad("name = \"menu\"", "name = \"home\"").contains("duplicate"));
        assert!(bad("name = \"menu\"", "name = \"Menu\"").contains("lower case"));
        assert!(bad("player = \"ann\"", "player = \"zed\"").contains("not in [players]"));
        assert!(bad("why = \"scrolls\"", "why = \" \"").contains("needs a why"));
        assert!(bad("rule = \"clipped-text\"", "rule = \"nope\"").contains("unknown rule"));
        assert!(bad("\"GET /api/me\"", "\"api/me\"").contains("must look like"));
        assert!(bad("{ click = \"button\" }", "{ hover = \"button\" }").contains("hover"));
        let devices = format!("{UI}\n[devices]\nui = [\"nokia\"]\n");
        assert!(
            Config::parse(&devices, "0.4.2")
                .unwrap_err()
                .to_string()
                .contains("unknown device")
        );
    }

    #[test]
    fn smoke_section_parses_and_checks_its_paths() {
        let ok = format!(
            "{OK}\n[smoke]\nup = \"make smoke_up PORT={{port}}\"\ndown = \"make smoke_down\"\nchecks = [{{ path = \"/\", contains = \"x\" }}]\n"
        );
        let c = Config::parse(&ok, "0.4.2").unwrap();
        let s = c.smoke.unwrap();
        assert_eq!(
            (s.timeout, s.ready.as_str(), s.checks[0].status),
            (30, "/", 200)
        );
        let bad = ok.replace("path = \"/\"", "path = \"x\"");
        assert!(
            Config::parse(&bad, "0.4.2")
                .unwrap_err()
                .to_string()
                .contains("must start with /")
        );
        let typo = ok.replace("down =", "dwon = \"x\"\ndown =");
        assert!(Config::parse(&typo, "0.4.2").is_err());
    }

    #[test]
    fn includes_add_screens_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("shakedown-include-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("screens")).unwrap();
        std::fs::write(
            dir.join("screens/b.toml"),
            "[[screen]]\nname = \"b\"\npath = \"/b\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("screens/a.toml"),
            "[[screen]]\nname = \"a\"\npath = \"/a\"\n",
        )
        .unwrap();
        let main = "shakedown = \"0.4.2\"\nproject = \"p\"\ninclude = [\"screens/*.toml\"]\n[server]\nstart = \"x\"\n[[screen]]\nname = \"home\"\npath = \"/\"\n";
        let c = Config::parse_in(main, "0.4.2", &dir).unwrap();
        let names: Vec<_> = c.screen.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            ["home", "a", "b"],
            "own screens first, then each file in name order"
        );
        std::fs::write(dir.join("screens/c.toml"), "project = \"x\"\n").unwrap();
        let e = Config::parse_in(main, "0.4.2", &dir)
            .unwrap_err()
            .to_string();
        assert!(e.contains("screen"), "{e}");
        let none = main.replace("screens/*.toml", "nowhere/*.toml");
        assert!(Config::parse_in(&none, "0.4.2", &dir).is_err());
        let dup = format!("{main}[[screen]]\nname = \"a\"\npath = \"/\"\n");
        std::fs::remove_file(dir.join("screens/c.toml")).unwrap();
        assert!(
            Config::parse_in(&dup, "0.4.2", &dir)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
