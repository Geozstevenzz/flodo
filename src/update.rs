//! Updating Flodo from npm.
//!
//! Flodo ships as the npm package `flodo`, so an update is `npm install -g`
//! of a newer version into the installation the app is running from. Windows
//! cannot replace an executable while it runs, so the app never installs over
//! itself: it writes a small helper script, starts it, and quits. The helper
//! waits for Flodo to exit, installs the exact version that was offered,
//! records the outcome in `update-result.json`, and starts Flodo again. The
//! new instance shows that outcome once and deletes the file.
//!
//! npm is run as `node npm-cli.js` when the npm launcher told us where those
//! are (`FLODO_NODE`, `FLODO_NPM_CLI`), because an app started from a
//! shortcut or Finder often has a PATH without npm on it.

use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
pub const RESULT_FILE: &str = "update-result.json";

const CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const AFTER_LAUNCH: Duration = Duration::from_secs(5);
/// Debug builds only: pretend this version is available and press the update
/// button a few seconds after launch. Used to test the restart end to end
/// against a throwaway npm prefix; release builds ignore it.
#[cfg(debug_assertions)]
const DEBUG_UPDATE_TO: &str = "FLODO_DEBUG_UPDATE_TO";

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// ------------------------------------------------------------------ versions

/// `(major, minor, patch)` of a stable release. Prereleases, build metadata
/// and anything else that is not three plain numbers give `None`.
pub fn stable(version: &str) -> Option<(u64, u64, u64)> {
    let parts: Vec<&str> = version.trim().split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let mut numbers = [0u64; 3];
    for (slot, part) in numbers.iter_mut().zip(&parts) {
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    Some((numbers[0], numbers[1], numbers[2]))
}

/// Whether `candidate` is a stable release newer than `current`. Never true
/// for a prerelease or for anything that does not parse.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    matches!((stable(candidate), stable(current)), (Some(c), Some(cur)) if c > cur)
}

/// The version out of `npm view flodo version --json`: a JSON string, or an
/// array when several versions match.
pub fn parse_view(stdout: &str) -> Result<String, String> {
    let text = stdout.trim();
    let version = match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::String(v)) => Some(v),
        Ok(serde_json::Value::Array(list)) => list
            .iter()
            .rev()
            .find_map(|v| v.as_str().map(str::to_string)),
        Ok(_) => None,
        Err(_) => Some(text.trim_matches('"').to_string()),
    };
    match version {
        Some(v) if !v.is_empty() && !v.contains(char::is_whitespace) => Ok(v),
        _ => Err("npm returned an unreadable version.".into()),
    }
}

// -------------------------------------------------------------- installation

/// Where a global npm install keeps Flodo, as path strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    /// `<prefix>/node_modules/flodo` on Windows, `<prefix>/lib/node_modules/flodo` elsewhere.
    pub package: String,
    /// What `npm install -g --prefix` needs to update this same installation.
    pub prefix: String,
}

/// Works out the npm package and prefix from the running executable's path,
/// which sits at `.../node_modules/flodo/native/<platform>/...`. Pure string
/// work, so both layouts can be tested on any platform.
pub fn layout(exe: &str, windows: bool) -> Option<Layout> {
    let sep = if windows { "\\" } else { "/" };
    let parts: Vec<&str> = exe.split(|c| c == '/' || (windows && c == '\\')).collect();
    let same = |a: &str, b: &str| {
        if windows {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    };
    // `native/<platform>/<executable>` must follow, so a folder that merely
    // happens to be called `native` is not mistaken for the package.
    let i = (0..parts.len().saturating_sub(4)).rev().find(|&i| {
        same(parts[i], "node_modules")
            && same(parts[i + 1], "flodo")
            && same(parts[i + 2], "native")
    })?;
    let prefix_end = if windows {
        i
    } else if i > 0 && parts[i - 1] == "lib" {
        i - 1
    } else {
        return None;
    };
    let join = |n: usize| {
        let joined = parts[..n].join(sep);
        if joined.is_empty() {
            sep.to_string()
        } else {
            joined
        }
    };
    if windows && prefix_end == 0 {
        return None;
    }
    let mut prefix = join(prefix_end);
    // `C:` alone means the current folder on C:, and `C:\` would end in a
    // backslash, which the helper's command-line quoting cannot carry.
    if windows && prefix.ends_with(':') {
        prefix.push_str(r"\.");
    }
    Some(Layout {
        package: join(i + 2),
        prefix,
    })
}

/// The global npm installation this process runs from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub exe: PathBuf,
    pub package: PathBuf,
    pub prefix: PathBuf,
}

impl Install {
    /// `None` when Flodo was not installed with `npm install -g`: a release
    /// download, `cargo run`, or a project-local `npm install flodo`.
    pub fn current() -> Option<Self> {
        let exe = plain_path(std::env::current_exe().ok()?.canonicalize().ok()?);
        let found = layout(exe.to_str()?, cfg!(windows))?;
        let package = PathBuf::from(found.package);
        let prefix = PathBuf::from(found.prefix);
        // A global install puts a `flodo` command next to the packages; a
        // project-local one does not, and `install -g` would litter it.
        let command = if cfg!(windows) {
            prefix.join("flodo.cmd")
        } else {
            prefix.join("bin").join("flodo")
        };
        (package.join("package.json").is_file() && command.exists()).then_some(Self {
            exe,
            package,
            prefix,
        })
    }

    /// What to start again: the app bundle on macOS, the executable elsewhere.
    #[cfg(target_os = "macos")]
    fn relaunch_app(&self) -> Option<PathBuf> {
        self.exe
            .ancestors()
            .find(|p| p.extension().is_some_and(|e| e == "app"))
            .map(Path::to_path_buf)
    }
}

/// `canonicalize` on Windows returns `\\?\C:\...`, which npm and PowerShell
/// handle badly. Drop the prefix again.
fn plain_path(path: PathBuf) -> PathBuf {
    let Some(text) = path.to_str() else {
        return path;
    };
    if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{unc}"))
    } else if let Some(local) = text.strip_prefix(r"\\?\") {
        PathBuf::from(local)
    } else {
        path
    }
}

// ----------------------------------------------------------------------- npm

/// How to run npm.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Npm {
    /// `node npm-cli.js`, independent of PATH.
    Node { node: PathBuf, cli: PathBuf },
    /// Whatever `npm` is on PATH.
    Path,
}

impl Npm {
    /// The launcher's `FLODO_NODE` and `FLODO_NPM_CLI` first, then the node
    /// that usually lives with a global prefix (nvm, Homebrew, nvm-windows),
    /// then PATH. macOS opens the app through LaunchServices, which drops the
    /// launcher's environment, so the second step is what finds npm there.
    pub fn find(prefix: Option<&Path>) -> Self {
        let from_env = std::env::var_os("FLODO_NODE")
            .zip(std::env::var_os("FLODO_NPM_CLI"))
            .map(|(node, cli)| (PathBuf::from(node), PathBuf::from(cli)));
        let beside_prefix = prefix.map(|prefix| {
            if cfg!(windows) {
                (
                    prefix.join("node.exe"),
                    prefix
                        .join("node_modules")
                        .join("npm")
                        .join("bin")
                        .join("npm-cli.js"),
                )
            } else {
                (
                    prefix.join("bin").join("node"),
                    prefix
                        .join("lib")
                        .join("node_modules")
                        .join("npm")
                        .join("bin")
                        .join("npm-cli.js"),
                )
            }
        });
        from_env
            .into_iter()
            .chain(beside_prefix)
            .find(|(node, cli)| node.is_file() && cli.is_file())
            .map_or(Npm::Path, |(node, cli)| Npm::Node { node, cli })
    }

    /// A hidden, prompt-free npm command. On Windows the PATH fallback goes
    /// through `cmd.exe` because `npm` there is a batch file.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = match self {
            Npm::Node { node, cli } => {
                let mut cmd = Command::new(node);
                cmd.arg(cli);
                cmd
            }
            Npm::Path if cfg!(windows) => {
                let mut cmd = Command::new("cmd.exe");
                cmd.args(["/d", "/c", "npm"]);
                cmd
            }
            Npm::Path => Command::new("npm"),
        };
        cmd.args(args)
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in QUIET {
            cmd.env(key, value);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        cmd
    }
}

/// Keeps npm from nagging, prompting or phoning anywhere but the registry.
const QUIET: [(&str, &str); 5] = [
    ("npm_config_update_notifier", "false"),
    ("npm_config_fund", "false"),
    ("npm_config_audit", "false"),
    ("npm_config_progress", "false"),
    ("npm_config_color", "false"),
];

struct Output {
    ok: bool,
    stdout: String,
    stderr: String,
}

/// Runs a command with a hard timeout, draining both pipes while it runs.
fn run(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            NPM_MISSING.to_string()
        } else {
            format!("Could not start npm: {e}")
        }
    })?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut buf);
            }
            let _ = tx.send(String::from_utf8_lossy(&buf).into_owned());
        });
        rx
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    "npm took too long to answer. Check your connection and try again.".into(),
                );
            }
        }
    };
    let collect =
        |rx: Receiver<String>| rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
    Ok(Output {
        ok: status.success(),
        stdout: collect(out),
        stderr: collect(err),
    })
}

const NPM_MISSING: &str = "npm was not found. Install Node.js, or update Flodo from a terminal.";

/// One readable line out of npm's error output.
fn explain(stderr: &str) -> String {
    if stderr.contains("is not recognized as an internal or external command") {
        return NPM_MISSING.into();
    }
    if stderr.contains("E404") {
        return "Flodo was not found on the npm registry.".into();
    }
    let offline = [
        "ENOTFOUND",
        "EAI_AGAIN",
        "ECONNREFUSED",
        "ECONNRESET",
        "ETIMEDOUT",
    ];
    if offline.iter().any(|code| stderr.contains(code)) {
        return "Could not reach the npm registry. Check your connection.".into();
    }
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.to_ascii_lowercase().starts_with("npm warn"));
    match line {
        Some(line) => {
            let line = line
                .strip_prefix("npm error ")
                .or_else(|| line.strip_prefix("npm ERR! "))
                .unwrap_or(line);
            let short: String = line.chars().take(100).collect();
            format!("npm failed: {short}")
        }
        None => "npm failed without saying why.".into(),
    }
}

/// Asks the registry for the latest version. `Ok(Some(v))` when `v` is a
/// newer stable release than this build.
pub fn check(npm: &Npm) -> Result<Option<String>, String> {
    let out = run(
        npm.command(&["view", "flodo", "version", "--json"]),
        CHECK_TIMEOUT,
    )?;
    if !out.ok {
        return Err(explain(&out.stderr));
    }
    let latest = parse_view(&out.stdout)?;
    Ok(is_newer(&latest, CURRENT).then_some(latest))
}

// -------------------------------------------------------------------- result

/// What the helper leaves behind for the next launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResult {
    pub ok: bool,
    pub from: String,
    pub to: String,
    pub message: String,
    pub finished_at: String,
}

impl UpdateResult {
    pub fn describe(&self) -> String {
        if self.ok {
            format!("Updated to {}.", self.to)
        } else {
            let reason = self.message.trim().trim_end_matches('.');
            format!("Update to {} failed: {reason}. See update.log.", self.to)
        }
    }
}

/// Reads and deletes the helper's result, so it is shown once.
pub fn take_result(state_dir: &Path) -> Option<UpdateResult> {
    let path = state_dir.join(RESULT_FILE);
    let raw = std::fs::read_to_string(&path).ok()?;
    let _ = std::fs::remove_file(&path);
    // Windows PowerShell likes to write a byte-order mark.
    serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok()
}

// -------------------------------------------------------------------- helper

/// Everything the helper script needs, already resolved.
struct Job<'a> {
    pid: u32,
    npm: &'a Npm,
    prefix: &'a Path,
    package: &'a Path,
    spec: String,
    from: &'a str,
    to: &'a str,
    exe: &'a Path,
    /// The macOS bundle to `open -a`; only the shell helper reads it.
    #[cfg_attr(windows, allow(dead_code))]
    app: Option<&'a Path>,
    work_dir: PathBuf,
    state_dir: &'a Path,
}

impl Job<'_> {
    fn node(&self) -> (String, String) {
        match self.npm {
            Npm::Node { node, cli } => (lossy(node), lossy(cli)),
            Npm::Path => (String::new(), String::new()),
        }
    }
}

fn lossy(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// A single-quoted PowerShell literal. PowerShell also treats the curly
/// single quotes as quote marks, so those are doubled too.
#[cfg(any(windows, test))]
fn ps_quote(value: &str) -> String {
    let mut out = String::from("'");
    for c in value.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

#[cfg(any(unix, test))]
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(any(windows, test))]
fn powershell_script(job: &Job) -> String {
    let (node, cli) = job.node();
    let vars = [
        ("Node", node),
        ("NpmCli", cli),
        ("Prefix", lossy(job.prefix)),
        ("Package", lossy(job.package)),
        ("Spec", job.spec.clone()),
        ("From", job.from.to_string()),
        ("To", job.to.to_string()),
        ("Exe", lossy(job.exe)),
        ("WorkDir", lossy(&job.work_dir)),
        ("StateDir", lossy(job.state_dir)),
    ];
    // The byte-order mark makes Windows PowerShell read the file as UTF-8
    // rather than the ANSI code page, so non-ASCII paths survive.
    let mut script = format!("\u{feff}$FlodoPid = {}\r\n", job.pid);
    for (name, value) in vars {
        script.push_str(&format!("${name} = {}\r\n", ps_quote(&value)));
    }
    script.push_str(
        &include_str!("update-helper.ps1")
            .replace("\r\n", "\n")
            .replace('\n', "\r\n"),
    );
    script
}

#[cfg(any(unix, test))]
fn shell_script(job: &Job) -> String {
    let (node, cli) = job.node();
    let vars = [
        ("NODE", node),
        ("NPM_CLI", cli),
        ("PREFIX", lossy(job.prefix)),
        ("PACKAGE", lossy(job.package)),
        ("SPEC", job.spec.clone()),
        ("FROM", job.from.to_string()),
        ("TO", job.to.to_string()),
        ("EXE", lossy(job.exe)),
        ("APP", job.app.map(lossy).unwrap_or_default()),
        ("WORK_DIR", lossy(&job.work_dir)),
        ("STATE_DIR", lossy(job.state_dir)),
    ];
    let mut script = format!("#!/bin/sh\nFLODO_PID={}\n", job.pid);
    for (name, value) in vars {
        script.push_str(&format!("{name}={}\n", sh_quote(&value)));
    }
    // A checkout with CRLF line endings must not break the shell.
    script.push_str(&include_str!("update-helper.sh").replace("\r\n", "\n"));
    script
}

/// Writes the helper script and starts it, detached from this process, so it
/// can install over Flodo once Flodo has quit. The caller closes the app.
pub fn start_helper(
    install: &Install,
    npm: &Npm,
    version: &str,
    state_dir: &Path,
) -> Result<(), String> {
    if stable(version).is_none() {
        return Err(format!("{version:?} is not a version Flodo can install."));
    }
    #[cfg(target_os = "macos")]
    let app = install.relaunch_app();
    #[cfg(not(target_os = "macos"))]
    let app: Option<PathBuf> = None;
    let job = Job {
        pid: std::process::id(),
        npm,
        prefix: &install.prefix,
        package: &install.package,
        spec: format!("flodo@{version}"),
        from: CURRENT,
        to: version,
        exe: &install.exe,
        app: app.as_deref(),
        work_dir: std::env::current_dir().unwrap_or_else(|_| std::env::temp_dir()),
        state_dir,
    };
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("flodo-update-{}-{nanos}", job.pid);
    #[cfg(windows)]
    let (script, text) = (
        std::env::temp_dir().join(format!("{name}.ps1")),
        powershell_script(&job),
    );
    #[cfg(unix)]
    let (script, text) = (
        std::env::temp_dir().join(format!("{name}.sh")),
        shell_script(&job),
    );
    std::fs::write(&script, text).map_err(|e| format!("Could not write the updater: {e}"))?;
    spawn_helper(&script).map_err(|e| {
        let _ = std::fs::remove_file(&script);
        format!("Could not start the updater: {e}")
    })
}

/// No console window, its own process group, and out of any job the app was
/// started in (a terminal's kill-on-close job would take the helper down with
/// Flodo). Breaking away is refused in some jobs, so it is tried first and
/// dropped if the spawn fails.
#[cfg(windows)]
fn spawn_helper(script: &Path) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    let command = |flags: u32| {
        let mut cmd = Command::new("powershell.exe");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(script)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(flags);
        #[cfg(debug_assertions)]
        cmd.env_remove(DEBUG_UPDATE_TO);
        cmd
    };
    let base = CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP;
    command(base | CREATE_BREAKAWAY_FROM_JOB)
        .spawn()
        .or_else(|_| command(base).spawn())
        .map(drop)
}

/// A new process group, so a terminal's hangup or Ctrl-C aimed at Flodo does
/// not reach the helper; the script also ignores SIGHUP.
#[cfg(unix)]
fn spawn_helper(script: &Path) -> std::io::Result<()> {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new("/bin/sh");
    cmd.arg(script)
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    #[cfg(debug_assertions)]
    cmd.env_remove(DEBUG_UPDATE_TO);
    cmd.spawn().map(drop)
}

// ---------------------------------------------------------------- app client

/// What the settings sheet shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Checking,
    UpToDate,
    Available(String),
    Failed(String),
    /// The user said yes; waiting for a sync in flight to finish first.
    Waiting(String),
    /// The helper is running and the app is closing.
    Restarting(String),
}

/// The app's side of updating: one quiet check after launch, one per press of
/// "Check for updates", and the hand-over to the helper.
pub struct Client {
    install: Option<Install>,
    /// False under `FLODO_DEMO` and in tests: nothing is checked or started.
    enabled: bool,
    results: Option<Receiver<Result<Option<String>, String>>>,
    due: Option<Instant>,
    status: Status,
    /// The last update's outcome, read once at launch.
    outcome: Option<UpdateResult>,
    #[cfg(debug_assertions)]
    auto_update: Option<Instant>,
}

impl Client {
    pub fn new(state_dir: &Path) -> Self {
        let install = Install::current();
        let mut client = Self {
            due: install.as_ref().map(|_| Instant::now() + AFTER_LAUNCH),
            install,
            enabled: true,
            results: None,
            status: Status::Idle,
            outcome: take_result(state_dir),
            #[cfg(debug_assertions)]
            auto_update: None,
        };
        #[cfg(debug_assertions)]
        if let Ok(version) = std::env::var(DEBUG_UPDATE_TO) {
            if client.install.is_some() && stable(&version).is_some() {
                client.due = None;
                client.status = Status::Available(version);
                client.auto_update = Some(Instant::now() + Duration::from_secs(3));
            }
        }
        client
    }

    /// Shows the controls but never runs npm.
    pub fn disabled() -> Self {
        Self {
            install: None,
            enabled: false,
            results: None,
            due: None,
            status: Status::Idle,
            outcome: None,
            #[cfg(debug_assertions)]
            auto_update: None,
        }
    }

    /// Whether the update button makes sense here.
    pub fn installed_by_npm(&self) -> bool {
        self.install.is_some() || !self.enabled
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    /// True once the user has said yes: no new sync should start.
    pub fn is_updating(&self) -> bool {
        matches!(self.status, Status::Waiting(_) | Status::Restarting(_))
    }

    pub fn waiting(&self) -> bool {
        matches!(self.status, Status::Waiting(_))
    }

    pub fn check_now(&mut self) {
        if self.enabled && self.install.is_some() && !self.is_updating() {
            self.outcome = None;
            self.due = Some(Instant::now());
        }
    }

    pub fn accept(&mut self) {
        if let Status::Available(version) = &self.status {
            self.outcome = None;
            self.status = Status::Waiting(version.clone());
        }
    }

    /// Collects a finished check and starts a due one, on its own thread.
    pub fn tick(&mut self, ctx: &eframe::egui::Context) {
        if let Some(rx) = &self.results {
            match rx.try_recv() {
                Ok(result) => {
                    self.results = None;
                    self.status = match result {
                        Ok(Some(version)) => Status::Available(version),
                        Ok(None) => Status::UpToDate,
                        Err(error) => Status::Failed(error),
                    };
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.results = None;
                    self.status = Status::Failed("The update check stopped unexpectedly.".into());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        #[cfg(debug_assertions)]
        if self.auto_update.is_some_and(|at| at <= Instant::now()) {
            self.auto_update = None;
            self.accept();
        } else if let Some(at) = self.auto_update {
            ctx.request_repaint_after(at.saturating_duration_since(Instant::now()));
        }
        let (Some(install), Some(due)) = (&self.install, self.due) else {
            return;
        };
        if !self.enabled || self.results.is_some() || self.is_updating() {
            return;
        }
        let now = Instant::now();
        if due > now {
            ctx.request_repaint_after(due - now);
            return;
        }
        self.due = None;
        let prefix = install.prefix.clone();
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("flodo-update-check".into())
            .spawn(move || {
                let _ = tx.send(check(&Npm::find(Some(&prefix))));
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.results = Some(rx);
            self.status = Status::Checking;
        } else {
            self.status = Status::Failed("Could not start the update check.".into());
        }
    }

    /// Starts the helper for the accepted version. On success the caller must
    /// close the app at once; the helper is waiting for it to exit.
    pub fn hand_over(&mut self, state_dir: &Path) -> Result<(), String> {
        let (Status::Waiting(version), Some(install)) = (&self.status, &self.install) else {
            return Err("No update is waiting.".into());
        };
        let version = version.clone();
        let npm = Npm::find(Some(&install.prefix));
        match start_helper(install, &npm, &version, state_dir) {
            Ok(()) => {
                self.status = Status::Restarting(version);
                Ok(())
            }
            Err(error) => {
                self.status = Status::Failed(error.clone());
                Err(error)
            }
        }
    }

    /// The status line, and whether it reports a problem. A failed update
    /// stays in view even when the same version is offered again.
    pub fn line(&self) -> (String, bool) {
        let outcome = self.outcome.as_ref();
        match (&self.status, outcome) {
            (Status::Checking, _) => ("Checking…".into(), false),
            (Status::Waiting(_), _) => ("Waiting for the sync to finish…".into(), false),
            (Status::Restarting(v), _) => (format!("Restarting to update to {v}…"), false),
            (_, Some(last)) if !last.ok => (last.describe(), true),
            (Status::Available(v), _) => (format!("Version {v} is available."), false),
            (_, Some(last)) => (last.describe(), false),
            (Status::Failed(error), None) => (error.clone(), true),
            (Status::UpToDate, None) => ("Up to date.".into(), false),
            (Status::Idle, None) => (String::new(), false),
        }
    }

    /// The button's text, and whether it can be pressed.
    pub fn button(&self) -> (String, bool) {
        match &self.status {
            Status::Available(v) => (format!("Update to {v} and restart"), true),
            Status::Checking | Status::Waiting(_) | Status::Restarting(_) => {
                ("Check for updates".into(), false)
            }
            _ => ("Check for updates".into(), self.enabled),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically_and_never_offer_prereleases() {
        assert!(is_newer("0.6.1", "0.6.0"));
        assert!(is_newer("0.10.0", "0.9.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.6.0", "0.6.0"));
        assert!(!is_newer("0.5.9", "0.6.0"));
        assert!(!is_newer("0.7.0-beta.1", "0.6.0"));
        assert!(!is_newer("0.7.0+build", "0.6.0"));
        assert!(!is_newer("garbage", "0.6.0"));
        assert!(!is_newer("", "0.6.0"));
        assert!(!is_newer("0.7", "0.6.0"));
        assert!(!is_newer("0.7.0.1", "0.6.0"));
        assert!(!is_newer("+1.0.0", "0.6.0"));
        assert!(!is_newer("1.+0.0", "0.6.0"));
        assert!(!is_newer("0.7.0", "not-a-version"));
        assert_eq!(stable(" 1.2.3\n"), Some((1, 2, 3)));
    }

    #[test]
    fn npm_view_output_is_parsed() {
        assert_eq!(parse_view("\"0.6.1\"\n").unwrap(), "0.6.1");
        assert_eq!(parse_view("[\"0.5.0\",\"0.6.1\"]").unwrap(), "0.6.1");
        assert_eq!(parse_view("0.6.1").unwrap(), "0.6.1");
        assert!(parse_view("").is_err());
        assert!(parse_view("{}").is_err());
        assert!(parse_view("npm error something").is_err());
    }

    #[test]
    fn windows_global_installs_are_found() {
        let found = layout(
            r"C:\Users\Ann Lee\AppData\Roaming\npm\node_modules\flodo\native\win32\flodo.exe",
            true,
        )
        .unwrap();
        assert_eq!(found.prefix, r"C:\Users\Ann Lee\AppData\Roaming\npm");
        assert_eq!(
            found.package,
            r"C:\Users\Ann Lee\AppData\Roaming\npm\node_modules\flodo"
        );
        // Case-insensitive, and forward slashes are separators too.
        let found = layout(
            r"D:/nvm4w/NodeJS/Node_Modules/Flodo/Native/win32/flodo.exe",
            true,
        )
        .unwrap();
        assert_eq!(found.prefix, r"D:\nvm4w\NodeJS");
        assert!(layout(r"C:\src\flodo\target\release\flodo.exe", true).is_none());
        assert!(layout(r"C:\x\node_modules\flodo\native", true).is_none());
        assert!(layout(r"node_modules\flodo\native\win32\flodo.exe", true).is_none());
        assert_eq!(
            layout(r"C:\node_modules\flodo\native\win32\flodo.exe", true)
                .unwrap()
                .prefix,
            r"C:\."
        );
    }

    #[test]
    fn unix_global_installs_are_found() {
        let found = layout(
            "/opt/homebrew/lib/node_modules/flodo/native/darwin/Flodo.app/Contents/MacOS/flodo",
            false,
        )
        .unwrap();
        assert_eq!(found.prefix, "/opt/homebrew");
        assert_eq!(found.package, "/opt/homebrew/lib/node_modules/flodo");
        let found = layout(
            "/home/ann/.nvm/versions/node/v22.1.0/lib/node_modules/flodo/native/linux/flodo",
            false,
        )
        .unwrap();
        assert_eq!(found.prefix, "/home/ann/.nvm/versions/node/v22.1.0");
        assert_eq!(
            layout("/lib/node_modules/flodo/native/linux/flodo", false)
                .unwrap()
                .prefix,
            "/"
        );
        // A project-local install has no `lib` above node_modules.
        assert!(layout("/home/ann/app/node_modules/flodo/native/linux/flodo", false).is_none());
        // Case matters here, and backslashes are ordinary characters.
        assert!(layout("/usr/lib/Node_Modules/flodo/native/linux/flodo", false).is_none());
        assert!(layout("/usr/local/bin/flodo", false).is_none());
    }

    #[test]
    fn verbatim_windows_paths_are_made_plain() {
        assert_eq!(
            plain_path(PathBuf::from(r"\\?\C:\npm\flodo.exe")),
            PathBuf::from(r"C:\npm\flodo.exe")
        );
        assert_eq!(
            plain_path(PathBuf::from(r"\\?\UNC\srv\share\flodo.exe")),
            PathBuf::from(r"\\srv\share\flodo.exe")
        );
        assert_eq!(
            plain_path(PathBuf::from("/usr/bin")),
            PathBuf::from("/usr/bin")
        );
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir()
                .join(format!("flodo-update-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn the_result_file_round_trips_and_is_shown_once() {
        let dir = TempDir::new("result");
        let result = UpdateResult {
            ok: false,
            from: "0.6.0".into(),
            to: "0.6.1".into(),
            message: "npm error EBUSY".into(),
            finished_at: "2026-11-23T10:00:00Z".into(),
        };
        let json = serde_json::to_string(&result).unwrap();
        // As Windows PowerShell may write it.
        std::fs::write(dir.0.join(RESULT_FILE), format!("\u{feff}{json}")).unwrap();
        assert_eq!(take_result(&dir.0), Some(result.clone()));
        assert!(!dir.0.join(RESULT_FILE).exists());
        assert_eq!(take_result(&dir.0), None);
        assert_eq!(
            result.describe(),
            "Update to 0.6.1 failed: npm error EBUSY. See update.log."
        );
        let ok = UpdateResult { ok: true, ..result };
        assert_eq!(ok.describe(), "Updated to 0.6.1.");

        // What the shell helper writes.
        std::fs::write(
            dir.0.join(RESULT_FILE),
            r#"{"ok":true,"from":"0.6.0","to":"0.6.1","message":"","finished_at":"2026-11-23T10:00:00Z"}"#,
        )
        .unwrap();
        assert!(take_result(&dir.0).unwrap().ok);

        // A garbled file is still removed, so it cannot linger.
        std::fs::write(dir.0.join(RESULT_FILE), "{").unwrap();
        assert_eq!(take_result(&dir.0), None);
        assert!(!dir.0.join(RESULT_FILE).exists());
    }

    #[test]
    fn npm_errors_become_one_line() {
        assert!(explain("npm error code E404\nnpm error 404 Not Found").contains("not found"));
        assert!(explain("npm error code ENOTFOUND\nnpm error network").contains("reach"));
        assert!(
            explain("'npm' is not recognized as an internal or external command,")
                .contains("not found")
        );
        assert_eq!(
            explain("npm warn config x\nnpm error code EPERM\n"),
            "npm failed: code EPERM"
        );
        assert_eq!(explain("\n"), "npm failed without saying why.");
    }

    #[test]
    fn quoting_keeps_paths_literal() {
        assert_eq!(ps_quote(r"C:\Ann's \u2019 $x"), r"'C:\Ann''s \u2019 $x'");
        assert_eq!(ps_quote("a\u{2019}b"), "'a\u{2019}\u{2019}b'");
        assert_eq!(sh_quote("/home/ann's $HOME"), r"'/home/ann'\''s $HOME'");
    }

    #[test]
    fn helper_scripts_carry_the_job() {
        let npm = Npm::Node {
            node: PathBuf::from("/n/node"),
            cli: PathBuf::from("/n/npm-cli.js"),
        };
        let job = Job {
            pid: 42,
            npm: &npm,
            prefix: Path::new("/p"),
            package: Path::new("/p/lib/node_modules/flodo"),
            spec: "flodo@0.6.1".into(),
            from: "0.6.0",
            to: "0.6.1",
            exe: Path::new("/p/lib/node_modules/flodo/native/linux/flodo"),
            app: None,
            work_dir: PathBuf::from("/home/ann"),
            state_dir: Path::new("/state"),
        };
        let sh = shell_script(&job);
        assert!(sh.starts_with("#!/bin/sh\nFLODO_PID=42\n"));
        assert!(sh.contains("SPEC='flodo@0.6.1'\n"));
        assert!(sh.contains("APP=''\n"));
        assert!(sh.contains("update-result.json"));
        let ps = powershell_script(&job);
        assert!(ps.starts_with("\u{feff}$FlodoPid = 42\r\n"));
        assert!(ps.contains("$NpmCli = '/n/npm-cli.js'\r\n"));
        assert!(ps.contains("update-result.json"));
        assert!(!ps.replace("\r\n", "").contains('\n'));
    }
}
