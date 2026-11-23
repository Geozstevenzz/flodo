//! Keeps the data identical across computers through a private git remote.
//!
//! Flodo shells out to the `git` on this machine and keeps its own clone in
//! `<state dir>/sync/repo`. Authentication is whatever that git already has
//! (a credential helper or an SSH key); Flodo stores no token.
//!
//! The merge base is `refs/remotes/origin/main` in that clone: the last
//! remote state this computer has written into its own files. Fetches land in
//! `refs/flodo/remote/*` instead, and the base only moves once the merged
//! result is on disk here. A cycle that fails halfway therefore leaves the
//! base where it was, and the next cycle cannot mistake the remote's changes
//! for local deletions.
//!
//! Every function takes explicit paths rather than reading `FLODO_STATE_DIR`,
//! so tests can run in parallel.

use crate::calendar::{self, CALENDAR_FILE};
use crate::merge::{self, Changes};
use crate::model::Store;
use crate::store::{self, TODOS_FILE};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant, SystemTime};

const BRANCH: &str = "refs/heads/main";
const BASE_REF: &str = "refs/remotes/origin/main";
const FETCHED_REF: &str = "refs/flodo/remote/main";
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// A lock older than this belongs to a process that died mid-sync.
const LOCK_STALE: Duration = Duration::from_secs(5 * 60);
const ATTEMPTS: usize = 3;

pub fn sync_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("sync")
}

/// What a finished cycle did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Outcome {
    pub todos: Changes,
    pub events: Changes,
    pub pushed: bool,
}

impl Outcome {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        let todos = self.todos;
        if todos.total() > 0 {
            let mut bits = Vec::new();
            for (n, word) in [
                (todos.added, "new"),
                (todos.updated, "changed"),
                (todos.removed, "removed"),
            ] {
                if n > 0 {
                    bits.push(format!("{n} {word}"));
                }
            }
            parts.push(format!(
                "{} to-do{}",
                bits.join(", "),
                if todos.total() == 1 { "" } else { "s" }
            ));
        }
        let events = self.events.total();
        if events > 0 {
            parts.push(format!(
                "{events} calendar change{}",
                if events == 1 { "" } else { "s" }
            ));
        }
        let received = (!parts.is_empty()).then(|| format!("Received {}.", parts.join("; ")));
        let sent = self.pushed.then(|| "Sent your changes.".to_string());
        match (received, sent) {
            (None, None) => "Already up to date.".into(),
            (Some(one), None) | (None, Some(one)) => one,
            (Some(received), Some(sent)) => format!("{received} {sent}"),
        }
    }
}

/// The last result, kept on disk so `flodo sync status` can report what the
/// app's background sync did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastResult {
    pub at: i64,
    pub ok: bool,
    pub message: String,
}

pub fn last_result(state_dir: &Path) -> Option<LastResult> {
    let raw = std::fs::read_to_string(sync_dir(state_dir).join("status.json")).ok()?;
    serde_json::from_str(&raw).ok()
}

fn record_result(state_dir: &Path, result: &Result<Outcome, String>) {
    let last = LastResult {
        at: crate::model::now_secs(),
        ok: result.is_ok(),
        message: match result {
            Ok(outcome) => outcome.describe(),
            Err(error) => error.clone(),
        },
    };
    if let Ok(json) = serde_json::to_string_pretty(&last) {
        let _ = store::write_atomic(&sync_dir(state_dir).join("status.json"), &json);
    }
}

/// Runs one full sync: fetch, merge into the local files, push. Retries when
/// another computer pushed first or a local file changed underneath it.
pub fn run(state_dir: &Path, remote: &str) -> Result<Outcome, String> {
    let result = run_with(state_dir, remote, &mut || {});
    record_result(state_dir, &result);
    result
}

/// `before_push` lets tests have another computer push first.
fn run_with(
    state_dir: &Path,
    remote: &str,
    before_push: &mut dyn FnMut(),
) -> Result<Outcome, String> {
    let remote = remote.trim();
    if remote.is_empty() {
        return Err("No sync remote is set.".into());
    }
    if remote.starts_with('-') {
        return Err("That doesn't look like a git URL.".into());
    }
    let _lock = Lock::acquire(&sync_dir(state_dir))?;
    let mut total = Outcome::default();
    for _ in 0..ATTEMPTS {
        match cycle(state_dir, remote, &mut total, before_push) {
            Ok(Step::Done) => return Ok(total),
            Ok(Step::Retry) => continue,
            Err(error) => return Err(error),
        }
    }
    Err("Another computer kept syncing at the same moment. Try again shortly.".into())
}

enum Step {
    Done,
    Retry,
}

/// A file as it was read, so the write can confirm nothing changed since.
struct Live {
    path: PathBuf,
    raw: Option<String>,
}

impl Live {
    fn read(path: PathBuf) -> Result<Self, String> {
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => Some(raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Could not read {}: {e}", path.display())),
        };
        Ok(Self { path, raw })
    }

    fn unchanged(&self) -> bool {
        Live::read(self.path.clone()).is_ok_and(|now| now.raw == self.raw)
    }
}

fn cycle(
    state_dir: &Path,
    remote: &str,
    total: &mut Outcome,
    before_push: &mut dyn FnMut(),
) -> Result<Step, String> {
    let sync = sync_dir(state_dir);
    let repo = sync.join("repo");
    let git = Git::new(&repo);
    git.ensure_clone(remote)?;

    // The base is read before fetching: it is the last state both sides
    // agreed on, and fetching must not move it.
    let base_commit = git.commit_at(BASE_REF)?;
    git.fetch()?;
    let remote_commit = git.commit_at(FETCHED_REF)?;
    // A remote whose branch vanished (or a new, empty repository at the same
    // URL) must not read as "everything was deleted": merge as a union.
    let base_commit = base_commit.filter(|_| remote_commit.is_some());

    let read_store = |commit: &Option<String>, strict: bool| -> Result<Store, String> {
        match commit.as_deref().map(|c| git.file_at(c, TODOS_FILE)).transpose()? {
            Some(Some(raw)) => serde_json::from_str(&raw).or_else(|e| {
                if strict {
                    Err(format!(
                        "The {TODOS_FILE} on the sync remote is not valid ({e}). Fix it there first."
                    ))
                } else {
                    Ok(Store::default())
                }
            }),
            _ => Ok(Store::default()),
        }
    };
    let read_agenda = |commit: &Option<String>, strict: bool| -> Result<Value, String> {
        match commit
            .as_deref()
            .map(|c| git.file_at(c, CALENDAR_FILE))
            .transpose()?
        {
            Some(Some(raw)) => match calendar::parse_agenda(&raw) {
                Some(agenda) => Ok(agenda),
                None if strict => Err(format!(
                    "The {CALENDAR_FILE} on the sync remote is not valid. Fix it there first."
                )),
                None => Ok(calendar::empty_agenda()),
            },
            _ => Ok(calendar::empty_agenda()),
        }
    };
    // An unreadable base only costs deletions (the merge becomes a union);
    // an unreadable remote would be written over, so it stops the sync.
    let base_store = read_store(&base_commit, false)?;
    let remote_store = read_store(&remote_commit, true)?;
    let base_agenda = read_agenda(&base_commit, false)?;
    let remote_agenda = read_agenda(&remote_commit, true)?;

    let live_todos = Live::read(state_dir.join(TODOS_FILE))?;
    let live_calendar = Live::read(state_dir.join(CALENDAR_FILE))?;
    let local_store: Store = match &live_todos.raw {
        None => Store::default(),
        Some(raw) => serde_json::from_str(raw).map_err(|e| {
            format!(
                "Your {TODOS_FILE} is not valid ({e}). Nothing was synced; fix or move it first."
            )
        })?,
    };
    let local_agenda = match &live_calendar.raw {
        None => calendar::empty_agenda(),
        Some(raw) => calendar::parse_agenda(raw).ok_or_else(|| {
            format!(
                "Your {CALENDAR_FILE} could not be read. Nothing was synced; fix or move it first."
            )
        })?,
    };

    let merged_store = merge::merge_store(&base_store, &local_store, &remote_store);
    let merged_agenda = merge::merge_agenda(&base_agenda, &local_agenda, &remote_agenda);
    if calendar::parse_agenda(&merged_agenda.to_string()).is_none() {
        return Err(
            "Merging the calendars produced an invalid calendar. Nothing was changed.".into(),
        );
    }

    let todos_changed = to_value(&merged_store) != to_value(&local_store);
    let calendar_changed = merged_agenda != local_agenda;
    if todos_changed || calendar_changed {
        // The app or the CLI may have saved while we fetched; start over
        // rather than write over that save.
        if !live_todos.unchanged() || !live_calendar.unchanged() {
            return Ok(Step::Retry);
        }
        if todos_changed {
            replace_live(
                &sync,
                &live_todos,
                "last-local-todos.json",
                &pretty(&merged_store)?,
            )?;
        }
        if calendar_changed {
            replace_live(
                &sync,
                &live_calendar,
                "last-local-calendar.json",
                &pretty(&merged_agenda)?,
            )?;
        }
        total
            .todos
            .add(merge::todo_changes(&local_store, &merged_store));
        total
            .events
            .add(merge::event_changes(&local_agenda, &merged_agenda));
    }

    // This computer's files now include everything the remote had.
    match &remote_commit {
        Some(commit) => git.set_ref(BASE_REF, commit)?,
        None => git.delete_ref(BASE_REF)?,
    }

    // Build the commit from the remote's tree, so files someone else keeps in
    // the repository (a README, say) survive.
    std::fs::write(
        repo.join(TODOS_FILE),
        pretty(&merge::for_repo(&merged_store))?,
    )
    .map_err(|e| format!("Could not write the sync copy: {e}"))?;
    std::fs::write(repo.join(CALENDAR_FILE), pretty(&merged_agenda)?)
        .map_err(|e| format!("Could not write the sync copy: {e}"))?;
    let tree = git.tree_with(remote_commit.as_deref(), &[TODOS_FILE, CALENDAR_FILE])?;
    if let Some(commit) = &remote_commit {
        if git.tree_of(commit)? == tree {
            git.check_out(commit)?;
            return Ok(Step::Done);
        }
    }
    let commit = git.commit(&tree, remote_commit.as_deref())?;
    before_push();
    match git.push(&commit)? {
        Pushed::Yes => {
            git.set_ref(BASE_REF, &commit)?;
            git.check_out(&commit)?;
            total.pushed = true;
            Ok(Step::Done)
        }
        Pushed::Rejected => Ok(Step::Retry),
    }
}

fn to_value<T: Serialize>(value: &T) -> Option<Value> {
    serde_json::to_value(value).ok()
}

fn pretty<T: Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

/// Keeps the pre-merge file as a single rolling backup, then swaps in the
/// merged one atomically.
fn replace_live(sync: &Path, live: &Live, backup: &str, merged: &str) -> Result<(), String> {
    if let Some(raw) = &live.raw {
        store::write_atomic(&sync.join(backup), raw)
            .map_err(|e| format!("Could not back up {}: {e}", live.path.display()))?;
    }
    store::write_atomic(&live.path, merged)
        .map_err(|e| format!("Could not write {}: {e}", live.path.display()))
}

/// Stops the CLI and the app's background sync running two cycles at once.
struct Lock(PathBuf);

impl Lock {
    fn acquire(dir: &Path) -> Result<Self, String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("Could not create {}: {e}", dir.display()))?;
        let path = dir.join("lock");
        for _ in 0..2 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(Self(path));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .ok()
                        .and_then(|t| SystemTime::now().duration_since(t).ok())
                        .is_some_and(|age| age > LOCK_STALE);
                    if !stale {
                        return Err("Another sync is already running.".into());
                    }
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => return Err(format!("Could not create {}: {e}", path.display())),
            }
        }
        Err("Another sync is already running.".into())
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The computer's name, for commit messages and the committer address.
fn hostname() -> String {
    let name = ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty()))
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .filter(|v| !v.trim().is_empty())
        })
        .unwrap_or_else(|| "computer".into());
    let clean: String = name
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.')
        .collect();
    if clean.is_empty() {
        "computer".into()
    } else {
        clean
    }
}

/// Removes `user:password@` from anything URL-shaped, so a token typed into
/// the remote never reaches a status line.
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(ix) = rest.find("://") {
        let (head, tail) = rest.split_at(ix + 3);
        out.push_str(head);
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(tail.len());
        match tail[..end].rfind('@') {
            Some(at) => rest = &tail[at + 1..],
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

struct GitOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

enum Pushed {
    Yes,
    Rejected,
}

struct Git<'a> {
    dir: &'a Path,
    host: String,
}

impl<'a> Git<'a> {
    fn new(dir: &'a Path) -> Self {
        Self {
            dir,
            host: hostname(),
        }
    }

    /// Runs git with no terminal, no credential prompt and a hard timeout.
    /// A Windows GUI process has no console, so without CREATE_NO_WINDOW
    /// every call would flash one up.
    fn run_with(&self, args: &[&str], index: Option<&Path>) -> Result<GitOut, String> {
        let mut cmd = Command::new("git");
        cmd.args(["-c", "core.autocrlf=false", "-c", "core.quotepath=false"])
            .args(args)
            .current_dir(self.dir)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(index) = index {
            cmd.env("GIT_INDEX_FILE", index);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "Git is not installed, or is not on PATH.".to_string()
            } else {
                format!("Could not start git: {e}")
            }
        })?;
        // Drain both pipes while waiting, so a chatty git can't fill one and
        // block forever.
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
                Ok(None) if started.elapsed() < GIT_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "Git took too long to answer. Check your connection and try again.".into(),
                    );
                }
            }
        };
        // A helper git started (a credential cache daemon, say) can inherit
        // the pipes and keep them open after git exits, so don't wait on EOF
        // for long.
        let collect =
            |rx: Receiver<String>| rx.recv_timeout(Duration::from_secs(5)).unwrap_or_default();
        Ok(GitOut {
            ok: status.success(),
            stdout: collect(out),
            stderr: collect(err),
        })
    }

    fn run(&self, args: &[&str]) -> Result<GitOut, String> {
        self.run_with(args, None)
    }

    /// Runs a command that must succeed, returning its trimmed output.
    fn ok(&self, args: &[&str]) -> Result<String, String> {
        let out = self.run(args)?;
        if out.ok {
            Ok(out.stdout.trim().to_string())
        } else {
            Err(explain(&out.stderr))
        }
    }

    fn ensure_clone(&self, remote: &str) -> Result<(), String> {
        if !self.dir.join(".git").exists() {
            std::fs::create_dir_all(self.dir)
                .map_err(|e| format!("Could not create {}: {e}", self.dir.display()))?;
            self.ok(&["init", "-q"])?;
        }
        let current = self.run(&["config", "--get", "remote.origin.url"])?;
        if !current.ok {
            self.ok(&["remote", "add", "origin", remote])?;
        } else if current.stdout.trim() != remote {
            self.ok(&["remote", "set-url", "origin", remote])?;
            // A different repository: what we agreed with the old one says
            // nothing about this one.
            self.delete_ref(BASE_REF)?;
            self.delete_ref(FETCHED_REF)?;
        }
        // Fetches only ever go to refs/flodo/remote, never to the base, even
        // if someone runs `git fetch` in this clone by hand.
        let _ = self.run(&["config", "--unset-all", "remote.origin.fetch"]);
        Ok(())
    }

    fn fetch(&self) -> Result<(), String> {
        self.ok(&[
            "fetch",
            "-q",
            "--prune",
            "--no-tags",
            "origin",
            "+refs/heads/*:refs/flodo/remote/*",
        ])
        .map(|_| ())
    }

    fn commit_at(&self, reference: &str) -> Result<Option<String>, String> {
        let spec = format!("{reference}^{{commit}}");
        let out = self.run(&["rev-parse", "-q", "--verify", &spec])?;
        Ok(out.ok.then(|| out.stdout.trim().to_string()))
    }

    /// A file's contents at a commit, or `None` if the commit lacks it.
    fn file_at(&self, commit: &str, file: &str) -> Result<Option<String>, String> {
        let spec = format!("{commit}:{file}");
        if !self.run(&["cat-file", "-e", &spec])?.ok {
            return Ok(None);
        }
        self.ok(&["cat-file", "blob", &spec]).map(Some)
    }

    fn set_ref(&self, reference: &str, commit: &str) -> Result<(), String> {
        self.ok(&["update-ref", reference, commit]).map(|_| ())
    }

    fn delete_ref(&self, reference: &str) -> Result<(), String> {
        if self.commit_at(reference)?.is_some() {
            self.ok(&["update-ref", "-d", reference])?;
        }
        Ok(())
    }

    fn tree_of(&self, commit: &str) -> Result<String, String> {
        self.ok(&["rev-parse", &format!("{commit}^{{tree}}")])
    }

    /// The tree of `parent` with `files` replaced from the working directory.
    /// Uses a private index file so nothing a person does in the clone can
    /// leak into the commit.
    fn tree_with(&self, parent: Option<&str>, files: &[&str]) -> Result<String, String> {
        let index = self.dir.join(".git").join("flodo-index");
        let _ = std::fs::remove_file(&index);
        let run = |args: &[&str]| -> Result<String, String> {
            let out = self.run_with(args, Some(&index))?;
            if out.ok {
                Ok(out.stdout.trim().to_string())
            } else {
                Err(explain(&out.stderr))
            }
        };
        match parent {
            Some(parent) => run(&["read-tree", parent])?,
            None => run(&["read-tree", "--empty"])?,
        };
        for file in files {
            let blob = self.ok(&["hash-object", "-w", "--no-filters", "--", file])?;
            run(&[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{blob},{file}"),
            ])?;
        }
        let tree = run(&["write-tree"]);
        let _ = std::fs::remove_file(&index);
        tree
    }

    fn commit(&self, tree: &str, parent: Option<&str>) -> Result<String, String> {
        let email = format!("user.email=flodo@{}", self.host);
        let message = format!("Sync from {}", self.host);
        let mut args = vec![
            "-c",
            "user.name=Flodo",
            "-c",
            &email,
            "-c",
            "commit.gpgsign=false",
            "commit-tree",
            tree,
            "-m",
            &message,
        ];
        if let Some(parent) = parent {
            args.extend(["-p", parent]);
        }
        self.ok(&args)
    }

    fn push(&self, commit: &str) -> Result<Pushed, String> {
        let spec = format!("{commit}:{BRANCH}");
        let out = self.run(&["push", "-q", "--porcelain", "origin", &spec])?;
        if out.ok {
            return Ok(Pushed::Yes);
        }
        let text = format!("{}\n{}", out.stdout, out.stderr);
        if [
            "[rejected]",
            "non-fast-forward",
            "fetch first",
            "stale info",
        ]
        .iter()
        .any(|s| text.contains(s))
        {
            Ok(Pushed::Rejected)
        } else {
            Err(explain(&out.stderr))
        }
    }

    /// Points the clone's own branch, index and files at `commit`, so the
    /// clone reads sensibly if someone looks inside it.
    fn check_out(&self, commit: &str) -> Result<(), String> {
        self.set_ref(BRANCH, commit)?;
        self.ok(&["symbolic-ref", "HEAD", BRANCH])?;
        self.ok(&["reset", "-q", "--hard", commit]).map(|_| ())
    }
}

/// Turns git's stderr into one line a person can act on.
fn explain(stderr: &str) -> String {
    let lower = stderr.to_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));
    if any(&[
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "permission denied (publickey",
        "host key verification failed",
        "access denied",
        "http basic: access denied",
    ]) {
        return "Git could not sign in to the remote. Run `git ls-remote <url>` in a terminal once to sign in."
            .into();
    }
    if any(&[
        "repository not found",
        "does not appear to be a git repository",
        "not found",
    ]) {
        return "The remote repository was not found. Check the URL.".into();
    }
    if any(&[
        "could not resolve host",
        "unable to access",
        "connection timed out",
        "failed to connect",
        "connection refused",
        "network is unreachable",
    ]) {
        return "Could not reach the remote. Check your connection.".into();
    }
    let line = stderr
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("unknown error");
    redact(&format!("Git failed: {line}"))
}

// ------------------------------------------------------------- app worker

struct Job {
    state_dir: PathBuf,
    remote: String,
}

/// What the settings sheet shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    Syncing,
    Synced(chrono::DateTime<chrono::Local>, String),
    Failed(String),
}

/// The app's side of sync: a background thread that runs cycles, and the
/// schedule that decides when. No async runtime; the thread blocks on git.
pub struct Client {
    jobs: Option<Sender<Job>>,
    results: Option<Receiver<Result<Outcome, String>>>,
    running: bool,
    due: Option<Instant>,
    pub status: Status,
}

/// How often to sync while the app is open.
const INTERVAL: Duration = Duration::from_secs(120);
/// How long after a save to sync it, so a burst of edits goes as one commit.
const AFTER_SAVE: Duration = Duration::from_secs(10);
const AFTER_LAUNCH: Duration = Duration::from_secs(3);

impl Default for Client {
    fn default() -> Self {
        Self {
            jobs: None,
            results: None,
            running: false,
            due: Some(Instant::now() + AFTER_LAUNCH),
            status: Status::Idle,
        }
    }
}

impl Client {
    pub fn is_running(&self) -> bool {
        self.running
    }

    pub fn saved(&mut self) {
        let soon = Instant::now() + AFTER_SAVE;
        self.due = Some(self.due.map_or(soon, |due| due.min(soon)));
    }

    pub fn sync_now(&mut self) {
        self.due = Some(Instant::now());
    }

    /// Collects a finished cycle and starts a due one. Returns true when a
    /// cycle has just finished, so the caller can look at the files at once.
    pub fn tick(
        &mut self,
        ctx: &eframe::egui::Context,
        state_dir: &Path,
        remote: Option<&str>,
        dirty: bool,
    ) -> bool {
        let mut finished = false;
        if let Some(rx) = &self.results {
            match rx.try_recv() {
                Ok(result) => {
                    self.finish(result);
                    finished = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.finish(Err("Sync stopped unexpectedly.".into()));
                    self.jobs = None;
                    self.results = None;
                    finished = true;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let Some(remote) = remote else {
            return finished;
        };
        if self.running {
            return finished;
        }
        let now = Instant::now();
        match self.due {
            Some(due) if due <= now && !dirty => self.start(ctx, state_dir, remote),
            Some(due) if due > now => ctx.request_repaint_after(due - now),
            _ => {}
        }
        finished
    }

    fn finish(&mut self, result: Result<Outcome, String>) {
        self.running = false;
        self.due = Some(Instant::now() + INTERVAL);
        self.status = match result {
            Ok(outcome) => Status::Synced(chrono::Local::now(), outcome.describe()),
            Err(error) => Status::Failed(error),
        };
    }

    fn start(&mut self, ctx: &eframe::egui::Context, state_dir: &Path, remote: &str) {
        if self.jobs.is_none() {
            let (job_tx, job_rx) = mpsc::channel::<Job>();
            let (result_tx, result_rx) = mpsc::channel();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("flodo-sync".into())
                .spawn(move || {
                    // Ends when the app drops its sender on quit; a cycle in
                    // flight is simply abandoned with the process.
                    while let Ok(job) = job_rx.recv() {
                        let result = run(&job.state_dir, &job.remote);
                        if result_tx.send(result).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                });
            if spawned.is_err() {
                self.finish(Err("Could not start the sync thread.".into()));
                return;
            }
            self.jobs = Some(job_tx);
            self.results = Some(result_rx);
        }
        let job = Job {
            state_dir: state_dir.to_path_buf(),
            remote: remote.to_string(),
        };
        if self.jobs.as_ref().is_some_and(|tx| tx.send(job).is_ok()) {
            self.running = true;
            self.due = None;
            self.status = Status::Syncing;
        } else {
            self.jobs = None;
            self.results = None;
            self.finish(Err("Sync stopped unexpectedly.".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_strips_credentials_from_urls() {
        assert_eq!(
            redact("fatal: unable to access 'https://me:tok3n@gitlab.com/me/flodo.git/'"),
            "fatal: unable to access 'https://gitlab.com/me/flodo.git/'"
        );
        assert_eq!(
            redact("git@gitlab.com:me/flodo.git"),
            "git@gitlab.com:me/flodo.git"
        );
        assert_eq!(
            redact("ssh://git@host/x and https://a@b/c"),
            "ssh://host/x and https://b/c"
        );
    }

    #[test]
    fn git_errors_become_one_actionable_line() {
        assert!(explain("fatal: Authentication failed for 'https://x/'").contains("sign in"));
        assert!(explain("remote: Repository not found.\nfatal: ...").contains("not found"));
        assert!(
            explain("fatal: unable to access 'https://x/': Could not resolve host: x")
                .contains("reach")
        );
        assert_eq!(
            explain("\n  something odd\nmore"),
            "Git failed: something odd"
        );
    }

    #[test]
    fn outcomes_describe_themselves() {
        assert_eq!(Outcome::default().describe(), "Already up to date.");
        let outcome = Outcome {
            todos: Changes {
                added: 2,
                removed: 0,
                updated: 1,
            },
            events: Changes {
                added: 1,
                ..Default::default()
            },
            pushed: true,
        };
        assert_eq!(
            outcome.describe(),
            "Received 2 new, 1 changed to-dos; 1 calendar change. Sent your changes."
        );
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let nanos = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let p = std::env::temp_dir()
                .join(format!("flodo-sync-{tag}-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn load(dir: &Path) -> Store {
        store::load_strict_at(&dir.join(TODOS_FILE)).unwrap()
    }

    fn save(dir: &Path, s: &Store) {
        store::write_atomic(
            &dir.join(TODOS_FILE),
            &serde_json::to_string_pretty(s).unwrap(),
        )
        .unwrap();
    }

    fn titles(dir: &Path) -> Vec<String> {
        load(dir).todos.into_iter().map(|t| t.title).collect()
    }

    /// Two state directories stand in for two computers, and a local bare
    /// repository for the GitLab project.
    #[test]
    fn two_computers_converge_through_a_bare_remote() {
        if !git_available() {
            eprintln!("git is not on PATH; skipping the sync integration test");
            return;
        }
        let root = TempDir::new("e2e");
        let bare = root.0.join("remote.git");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "--bare"])
            .current_dir(&bare)
            .status()
            .unwrap()
            .success());
        let url = bare.to_string_lossy().into_owned();
        let (a, b) = (root.0.join("a"), root.0.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();

        // A starts the empty remote.
        let mut s = Store::default();
        let first = s.add("from A");
        let second = s.add("second");
        s.get_mut(first).unwrap().expanded = true;
        save(&a, &s);
        let out = run(&a, &url).unwrap();
        assert!(out.pushed);
        assert_eq!(load(&a).todos.len(), 2, "A's own list is untouched");
        assert!(
            load(&a).get(first).unwrap().expanded,
            "expanded stays local"
        );

        // B has nothing yet and receives everything, unexpanded.
        let out = run(&b, &url).unwrap();
        assert_eq!(out.todos.added, 2);
        assert!(!out.pushed);
        assert_eq!(titles(&b), titles(&a));
        assert!(!load(&b).get(first).unwrap().expanded);
        assert!(last_result(&b).unwrap().ok);

        // Each edits a different task; both sync; they converge.
        let mut sa = load(&a);
        sa.get_mut(first).unwrap().title = "edited on A".into();
        save(&a, &sa);
        let mut sb = load(&b);
        sb.toggle(second);
        let added_on_b = sb.add("added on B");
        save(&b, &sb);
        run(&a, &url).unwrap();
        run(&b, &url).unwrap();
        run(&a, &url).unwrap();
        let (sa, sb) = (load(&a), load(&b));
        assert_eq!(
            serde_json::to_value(merge::for_repo(&sa)).unwrap(),
            serde_json::to_value(merge::for_repo(&sb)).unwrap()
        );
        assert_eq!(sa.get(first).unwrap().title, "edited on A");
        assert!(sa.get(second).unwrap().done);
        assert_eq!(sa.todos[0].id, added_on_b, "arrives where B put it");

        // B deletes; A sees the delete.
        let mut sb = load(&b);
        sb.remove(second);
        save(&b, &sb);
        run(&b, &url).unwrap();
        let out = run(&a, &url).unwrap();
        assert_eq!(out.todos.removed, 1);
        assert!(load(&a).get(second).is_none());
        assert!(sync_dir(&a).join("last-local-todos.json").exists());

        // A's push loses a race with B, is rejected, and is retried.
        let mut sa = load(&a);
        sa.add("raced on A");
        save(&a, &sa);
        let mut sb = load(&b);
        sb.add("raced on B");
        save(&b, &sb);
        let mut raced = false;
        let out = run_with(&a, &url, &mut || {
            if !raced {
                raced = true;
                run(&b, &url).unwrap();
            }
        })
        .unwrap();
        assert!(raced && out.pushed);
        run(&b, &url).unwrap();
        for dir in [&a, &b] {
            let t = titles(dir);
            assert!(t.contains(&"raced on A".to_string()), "{t:?}");
            assert!(t.contains(&"raced on B".to_string()), "{t:?}");
        }

        // A corrupt local file stops the sync and is left exactly as it was.
        std::fs::write(b.join(TODOS_FILE), "{ not json").unwrap();
        let err = run(&b, &url).unwrap_err();
        assert!(err.contains("not valid"), "{err}");
        assert_eq!(
            std::fs::read_to_string(b.join(TODOS_FILE)).unwrap(),
            "{ not json"
        );
        assert!(!last_result(&b).unwrap().ok);
    }

    #[test]
    fn calendars_sync_and_colliding_event_ids_are_both_kept() {
        if !git_available() {
            return;
        }
        let root = TempDir::new("cal");
        let bare = root.0.join("remote.git");
        std::fs::create_dir_all(&bare).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q", "--bare"])
            .current_dir(&bare)
            .status()
            .unwrap()
            .success());
        let url = bare.to_string_lossy().into_owned();
        let (a, b) = (root.0.join("a"), root.0.join("b"));
        let event = |title: &str| {
            format!(
                r#"{{"version":1,"events":[{{"id":1,"title":"{title}","date":"2026-11-20","start":"09:00:00","end":"10:00:00","kind":"meeting","location":""}}]}}"#
            )
        };
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join(CALENDAR_FILE), event("dentist")).unwrap();
        std::fs::write(b.join(CALENDAR_FILE), event("standup")).unwrap();
        run(&a, &url).unwrap();
        run(&b, &url).unwrap();
        run(&a, &url).unwrap();
        for dir in [&a, &b] {
            let raw = std::fs::read_to_string(dir.join(CALENDAR_FILE)).unwrap();
            assert!(calendar::parse_agenda(&raw).is_some());
            assert!(raw.contains("dentist") && raw.contains("standup"), "{raw}");
        }
        assert!(
            !a.join(TODOS_FILE).exists(),
            "no empty task file is invented"
        );
    }

    #[test]
    fn a_held_lock_refuses_a_second_sync_and_is_released_on_drop() {
        let dir = std::env::temp_dir().join(format!(
            "flodo-lock-{}-{}",
            std::process::id(),
            crate::model::now_millis()
        ));
        let first = Lock::acquire(&dir).unwrap();
        assert!(Lock::acquire(&dir).is_err());
        drop(first);
        assert!(Lock::acquire(&dir).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
