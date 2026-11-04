//! Read-only adapter for the active Windows Classic Outlook session.

use chrono::{NaiveDate, NaiveDateTime};
use eframe::egui;
use serde::Deserialize;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Deserialize)]
pub struct OutlookEvent {
    pub id: String,
    pub title: String,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub location: String,
    pub all_day: bool,
    pub is_meeting: bool,
}

#[derive(Deserialize)]
#[cfg(any(windows, test))]
struct Snapshot {
    events: Vec<OutlookEvent>,
    error: Option<String>,
}

pub struct Outlook {
    pub events: Vec<OutlookEvent>,
    pub status: String,
    receiver: Option<Receiver<Result<Vec<OutlookEvent>, String>>>,
    pub last_attempt: Option<Instant>,
}

impl Default for Outlook {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            status: if cfg!(windows) {
                "Open Classic Outlook, then refresh to read your calendar."
            } else {
                "Outlook sync requires Classic Outlook on Windows."
            }
            .into(),
            receiver: None,
            last_attempt: None,
        }
    }
}

impl Outlook {
    pub fn is_loading(&self) -> bool {
        self.receiver.is_some()
    }

    pub fn refresh(&mut self, from: NaiveDate, until: NaiveDate) {
        if self.is_loading() {
            return;
        }
        self.last_attempt = Some(Instant::now());
        self.status = "Reading Outlook...".into();
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        std::thread::spawn(move || {
            let _ = sender.send(read_calendar(from, until));
        });
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        let result = self.receiver.as_ref().and_then(|rx| match rx.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => {
                Some(Err("Outlook refresh stopped. Try again.".into()))
            }
        });
        if let Some(result) = result {
            self.receiver = None;
            match result {
                Ok(events) => {
                    self.events = events;
                    self.status = format!(
                        "Outlook refreshed at {}",
                        chrono::Local::now().format("%H:%M")
                    );
                }
                Err(error) => {
                    self.status = if self.events.is_empty() {
                        error
                    } else {
                        format!("{error} Showing the last successful refresh.")
                    };
                }
            }
        }
        if self.is_loading() {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }
}

#[cfg(any(windows, test))]
fn parse_snapshot(raw: &[u8]) -> Result<Vec<OutlookEvent>, String> {
    let snapshot: Snapshot = serde_json::from_slice(raw)
        .map_err(|_| "Outlook returned an unreadable calendar. Try refreshing again.".to_owned())?;
    if let Some(error) = snapshot.error {
        return Err(error);
    }
    let mut events = snapshot.events;
    if events
        .iter()
        .any(|event| event.end <= event.start || event.id.is_empty())
    {
        return Err("Outlook returned invalid appointment times. Try refreshing again.".into());
    }
    events.sort_by(|a, b| (a.start, &a.id).cmp(&(b.start, &b.id)));
    events.dedup_by(|a, b| a.id == b.id);
    Ok(events)
}

#[cfg(not(windows))]
fn read_calendar(_from: NaiveDate, _until: NaiveDate) -> Result<Vec<OutlookEvent>, String> {
    Err("Outlook sync requires Classic Outlook on Windows.".into())
}

#[cfg(windows)]
fn read_calendar(from: NaiveDate, until: NaiveDate) -> Result<Vec<OutlookEvent>, String> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};

    let mut child = Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-WindowStyle",
            "Hidden",
            "-Command",
        ])
        .arg(include_str!("outlook-calendar.ps1"))
        .env("FLODO_CALENDAR_FROM", from.to_string())
        .env("FLODO_CALENDAR_UNTIL", until.to_string())
        .creation_flags(0x08000000) // CREATE_NO_WINDOW, including debug builds.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| {
            "Could not start the Outlook reader. Windows PowerShell is required.".to_owned()
        })?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("Could not read the Outlook response.".into());
    };
    // Drain the pipe while waiting: a busy calendar can exceed the pipe buffer.
    let reader = std::thread::spawn(move || {
        let mut raw = Vec::new();
        stdout.read_to_end(&mut raw).map(|_| raw)
    });
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let raw = reader
                    .join()
                    .ok()
                    .and_then(Result::ok)
                    .ok_or_else(|| "Could not read the Outlook response.".to_owned())?;
                if !status.success() {
                    return Err("Outlook reader failed. Open Classic Outlook, then refresh.".into());
                }
                return parse_snapshot(&raw);
            }
            Ok(None) if started.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(50));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = reader.join();
                return Err("Outlook took too long to respond. Try refreshing again.".into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_outlook_rejects_failed_or_malformed_snapshots() {
        assert!(parse_snapshot(br#"{"events":[],"error":"Unavailable"}"#).is_err());
        assert!(parse_snapshot(b"not json").is_err());
        assert!(parse_snapshot(br#"{"events":[],"error":null}"#)
            .unwrap()
            .is_empty());
    }
}
