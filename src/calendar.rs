//! A local agenda, kept separate from the task list and closed on every launch.

use crate::{store, theme::Palette};
use chrono::{Datelike, Days, Local, NaiveDate, NaiveTime};
use eframe::egui::{self, RichText};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    #[default]
    Meeting,
    Appointment,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::Meeting => "Meeting",
            Self::Appointment => "Appointment",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Event {
    id: u64,
    title: String,
    date: NaiveDate,
    start: NaiveTime,
    end: NaiveTime,
    kind: Kind,
    #[serde(default)]
    location: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Agenda {
    version: u32,
    events: Vec<Event>,
}

impl Default for Agenda {
    fn default() -> Self {
        Self {
            version: 1,
            events: Vec::new(),
        }
    }
}

struct Editor {
    id: Option<u64>,
    title: String,
    date: String,
    start: String,
    end: String,
    kind: Kind,
    location: String,
    confirm_delete: bool,
    error: Option<String>,
}

impl Editor {
    fn new(date: NaiveDate) -> Self {
        Self {
            id: None,
            title: String::new(),
            date: date.to_string(),
            start: "09:00".into(),
            end: "09:30".into(),
            kind: Kind::Meeting,
            location: String::new(),
            confirm_delete: false,
            error: None,
        }
    }

    fn from_event(event: &Event) -> Self {
        Self {
            id: Some(event.id),
            title: event.title.clone(),
            date: event.date.to_string(),
            start: event.start.format("%H:%M").to_string(),
            end: event.end.format("%H:%M").to_string(),
            kind: event.kind,
            location: event.location.clone(),
            confirm_delete: false,
            error: None,
        }
    }

    fn event(&self, id: u64) -> Result<Event, String> {
        if self.title.trim().is_empty() {
            return Err("Give this event a title.".into());
        }
        let date = NaiveDate::parse_from_str(self.date.trim(), "%Y-%m-%d")
            .map_err(|_| "Use a valid date: YYYY-MM-DD.".to_string())?;
        let start = NaiveTime::parse_from_str(self.start.trim(), "%H:%M")
            .map_err(|_| "Use a start time between 00:00 and 23:59.".to_string())?;
        let end = NaiveTime::parse_from_str(self.end.trim(), "%H:%M")
            .map_err(|_| "Use an end time between 00:00 and 23:59.".to_string())?;
        if end <= start {
            return Err("End time must be after start time on the same day.".into());
        }
        Ok(Event {
            id,
            title: self.title.trim().into(),
            date,
            start,
            end,
            kind: self.kind,
            location: self.location.trim().into(),
        })
    }
}

pub struct Calendar {
    pub open: bool,
    agenda: Agenda,
    editor: Option<Editor>,
    path: PathBuf,
    /// A failed read must never turn into an empty file on the next save.
    writable: bool,
    disk_raw: Option<String>,
    error: Option<String>,
    outlook: crate::outlook::Outlook,
    outlook_refreshed: Option<Instant>,
    month: NaiveDate,
    selected_date: NaiveDate,
    outlook_range: Option<(NaiveDate, NaiveDate)>,
}

impl Default for Calendar {
    fn default() -> Self {
        Self {
            open: false,
            agenda: Agenda::default(),
            editor: None,
            path: store::state_dir().join("calendar.json"),
            writable: true,
            disk_raw: None,
            error: None,
            outlook: crate::outlook::Outlook::default(),
            outlook_refreshed: None,
            month: month_start(Local::now().date_naive()),
            selected_date: Local::now().date_naive(),
            outlook_range: None,
        }
    }
}

impl Calendar {
    pub fn load() -> Self {
        let mut calendar = Self::default();
        calendar.reload();
        calendar
    }

    fn reload(&mut self) {
        match std::fs::read_to_string(&self.path) {
            Ok(raw) => match serde_json::from_str::<Agenda>(&raw) {
                Ok(agenda) if valid_agenda(&agenda) => {
                    self.agenda = agenda;
                    self.disk_raw = Some(raw);
                    self.writable = true;
                    self.error = None;
                }
                _ => {
                    self.writable = false;
                    self.error = Some(
                        "Couldn't read your calendar. The file has been left untouched.".into(),
                    );
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.agenda = Agenda::default();
                self.disk_raw = None;
                self.writable = true;
                self.error = None;
            }
            Err(error) => {
                self.writable = false;
                self.error = Some(format!("Couldn't open your calendar: {error}"));
            }
        }
    }

    fn persist(&mut self, agenda: Agenda) -> Result<(), String> {
        if !self.writable {
            return Err("Reload your calendar before making changes.".into());
        }
        let current = match std::fs::read_to_string(&self.path) {
            Ok(raw) => Some(raw),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Couldn't read your calendar: {error}")),
        };
        if current != self.disk_raw {
            self.writable = false;
            return Err("Your calendar changed elsewhere. Reload it before saving.".into());
        }
        let raw = serde_json::to_string_pretty(&agenda).map_err(|error| error.to_string())?;
        store::write_atomic(&self.path, &raw)
            .map_err(|error| format!("Couldn't save your calendar: {error}"))?;
        self.agenda = agenda;
        self.disk_raw = Some(raw);
        self.error = None;
        Ok(())
    }

    pub fn show(&mut self, ctx: &egui::Context, p: &Palette) {
        self.outlook.poll(ctx);
        let range = outlook_range(self.month);
        if self.open
            && std::env::var_os("FLODO_DEMO").is_none()
            && !self.outlook.is_loading()
            && (self.outlook_range != Some(range)
                || self
                    .outlook_refreshed
                    .is_none_or(|last| last.elapsed() >= Duration::from_secs(300)))
        {
            if self.outlook_range != Some(range) {
                self.outlook.events.clear();
            }
            self.outlook.refresh(range.0, range.1);
            self.outlook_range = Some(range);
            self.outlook_refreshed = Some(Instant::now());
        }
        let screen = ctx.content_rect();
        let width = (screen.width() - 48.0).clamp(180.0, 320.0);
        let height = (screen.height() - 48.0).max(80.0);
        let slide = ctx.animate_bool_with_time(
            egui::Id::new("calendar-slide"),
            self.open,
            ctx.global_style().animation_time.min(0.2),
        );
        let right = screen.right() - 9.0;
        let left = right - width * slide;
        let top = screen.top() + 38.0;

        if slide > 0.0 {
            egui::Area::new(egui::Id::new("calendar-drawer"))
                .fixed_pos(egui::pos2(left, top))
                .order(egui::Order::Foreground)
                .constrain(false)
                .fade_in(false)
                .show(ctx, |ui| {
                    ui.set_clip_rect(screen.shrink(8.0));
                    egui::Frame::NONE
                        .fill(p.bg)
                        .stroke(egui::Stroke::new(1.0, p.border))
                        .corner_radius(8)
                        .inner_margin(12)
                        .show(ui, |ui| {
                            ui.set_width(width - 26.0);
                            ui.set_height(height - 26.0);
                            ui.visuals_mut().override_text_color = Some(p.text);
                            ui.horizontal(|ui| {
                                ui.heading(RichText::new("Calendar").size(18.0));
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.button("Close").clicked() {
                                            self.open = false;
                                        }
                                    },
                                );
                            });
                            ui.add_space(6.0);
                            egui::ScrollArea::vertical()
                                .id_salt("calendar-scroll")
                                .max_height((height - 64.0).max(30.0))
                                .auto_shrink([false, false])
                                .show(ui, |ui| self.contents(ui, p));
                        });
                });
        }

        // The tab travels with the drawer, like a handle on its leading edge.
        egui::Area::new(egui::Id::new("calendar-tab"))
            .fixed_pos(egui::pos2(left - 27.0, top + 42.0))
            .order(egui::Order::Foreground)
            .fade_in(false)
            .show(ctx, |ui| {
                let label = if self.open { ">" } else { "<" };
                let button = egui::Button::new(
                    RichText::new(format!("{label}\nCal"))
                        .size(10.0)
                        .color(p.accent),
                )
                .fill(p.surface)
                .stroke(egui::Stroke::new(1.0, p.border))
                .corner_radius(5)
                .min_size(egui::vec2(27.0, 56.0));
                let response = ui.add(button);
                crate::ui::hint(
                    ctx,
                    &response,
                    crate::ui::Action::new(if self.open {
                        "Close calendar"
                    } else {
                        "Open calendar"
                    }),
                );
                if response.clicked() {
                    self.open = !self.open;
                }
            });
        if self.open {
            // Dates and the current meeting must change even without input.
            ctx.request_repaint_after(Duration::from_secs(30));
        }
    }

    fn contents(&mut self, ui: &mut egui::Ui, p: &Palette) {
        self.month_grid(ui, p);
        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.writable && self.editor.is_none(),
                    egui::Button::new("+ Add event"),
                )
                .clicked()
            {
                self.editor = Some(Editor::new(self.selected_date));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Today").clicked() {
                    self.selected_date = Local::now().date_naive();
                    self.month = month_start(self.selected_date);
                }
            });
        });
        if let Some(error) = &self.error {
            ui.label(RichText::new(error).color(p.danger));
            if ui.button("Reload calendar").clicked() {
                self.reload();
            }
            ui.add_space(8.0);
        }
        if self.editor.is_some() {
            self.editor_ui(ui, p);
            ui.separator();
        }

        let today = Local::now().date_naive();
        let events: Vec<_> = ordered_events(&self.agenda.events)
            .into_iter()
            .filter(|event| in_week(event.date, self.selected_date))
            .collect();
        let mut upcoming_heading = false;
        for offset in 0..=7 {
            let Some(date) = self.selected_date.checked_add_days(Days::new(offset)) else {
                continue;
            };
            let mut rows: Vec<_> = events
                .iter()
                .filter(|event| event.date == date)
                .map(|event| (event.start, AgendaRow::Local(event)))
                .collect();
            let midnight = date.and_hms_opt(0, 0, 0).unwrap();
            if let Some(next) = date.succ_opt() {
                let next_midnight = next.and_hms_opt(0, 0, 0).unwrap();
                rows.extend(
                    self.outlook
                        .events
                        .iter()
                        .filter(|event| event.start < next_midnight && event.end > midnight)
                        .map(|event| (event.start.max(midnight).time(), AgendaRow::Outlook(event))),
                );
            }
            rows.sort_by_key(|(start, _)| *start);
            let count = rows.len();
            if count == 0 && offset > 0 {
                continue;
            }
            if offset > 0 && !upcoming_heading {
                ui.add_space(12.0);
                ui.label(RichText::new("Next 7 days").size(12.0).color(p.muted));
                upcoming_heading = true;
            }
            ui.add_space(10.0);
            let prefix = if date == today { "Today · " } else { "" };
            ui.label(
                RichText::new(format!("{prefix}{}", date.format("%A, %-d %b")))
                    .size(12.0)
                    .color(p.text),
            );
            for (_, row) in rows {
                match row {
                    AgendaRow::Local(event) => {
                        if event_row(ui, p, event) {
                            self.editor = Some(Editor::from_event(event));
                        }
                    }
                    AgendaRow::Outlook(event) => outlook_row(ui, p, event, date),
                }
            }
            if count == 0 {
                ui.label(
                    RichText::new("No events on this day.")
                        .size(12.0)
                        .color(p.muted),
                );
            }
        }

        ui.add_space(12.0);
        ui.separator();
        if ui
            .add_enabled(
                !self.outlook.is_loading(),
                egui::Button::new("Refresh Outlook"),
            )
            .clicked()
        {
            let range = outlook_range(self.month);
            self.outlook.refresh(range.0, range.1);
            self.outlook_range = Some(range);
            self.outlook_refreshed = Some(Instant::now());
        }
        ui.label(
            RichText::new(&self.outlook.status)
                .size(11.0)
                .color(p.muted),
        );
    }

    fn has_events(&self, date: NaiveDate) -> bool {
        self.agenda.events.iter().any(|event| event.date == date)
            || self
                .outlook
                .events
                .iter()
                .any(|event| outlook_on_date(event, date))
    }

    fn month_grid(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            if ui.small_button("<").clicked() {
                if let Some(month) = shift_month(self.month, -1) {
                    self.month = month;
                    self.selected_date = month;
                }
            }
            let width = (ui.available_width() - 30.0).max(60.0);
            ui.add_sized(
                [width, 24.0],
                egui::Label::new(
                    RichText::new(self.month.format("%B %Y").to_string())
                        .size(14.0)
                        .strong(),
                )
                .truncate(),
            );
            if ui.small_button(">").clicked() {
                if let Some(month) = shift_month(self.month, 1) {
                    self.month = month;
                    self.selected_date = month;
                }
            }
        });
        ui.add_space(5.0);
        let days = calendar_days(self.month);
        let row_height = 27.0;
        let width = ui.available_width();
        let cell_width = width / 7.0;
        let rows = days.len() / 7;
        let (rect, _) = ui.allocate_exact_size(
            egui::vec2(width, 18.0 + rows as f32 * row_height),
            egui::Sense::hover(),
        );
        let font = egui::FontId::new(
            12.0,
            ui.style().text_styles[&egui::TextStyle::Body]
                .family
                .clone(),
        );
        for (column, day) in ["Mo", "Tu", "We", "Th", "Fr", "Sa", "Su"]
            .iter()
            .enumerate()
        {
            let center = rect.min + egui::vec2((column as f32 + 0.5) * cell_width, 8.0);
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                day,
                egui::FontId::new(10.0, font.family.clone()),
                p.muted,
            );
        }
        let today = Local::now().date_naive();
        for (index, date) in days.into_iter().enumerate() {
            let cell = egui::Rect::from_min_size(
                rect.min
                    + egui::vec2(
                        (index % 7) as f32 * cell_width,
                        18.0 + (index / 7) as f32 * row_height,
                    ),
                egui::vec2(cell_width, row_height),
            );
            let response = ui.interact(
                cell,
                egui::Id::new(("calendar-day", date)),
                egui::Sense::click(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::WidgetType::Button,
                    true,
                    date.format("%A, %-d %B %Y").to_string(),
                )
            });
            let center = cell.center() - egui::vec2(0.0, 2.0);
            let has_events = self.has_events(date);
            let selected = date == self.selected_date;
            if selected || response.hovered() {
                ui.painter().circle_filled(center, 10.5, p.surface_hover);
            }
            if selected || date == today {
                ui.painter().circle_stroke(
                    center,
                    10.5,
                    egui::Stroke::new(1.0, if selected { p.muted } else { p.border }),
                );
            }
            let color = if has_events {
                p.danger
            } else if date.month() != self.month.month() {
                p.muted.gamma_multiply(0.4)
            } else {
                p.text
            };
            ui.painter().text(
                center,
                egui::Align2::CENTER_CENTER,
                date.day().to_string(),
                font.clone(),
                color,
            );
            if has_events {
                ui.painter()
                    .circle_filled(center + egui::vec2(0.0, 12.5), 1.5, p.danger);
            }
            if response.clicked() {
                self.selected_date = date;
                self.month = month_start(date);
            }
            crate::ui::hint(
                ui.ctx(),
                &response,
                crate::ui::Action::new(&date.format("%A, %-d %B %Y").to_string()),
            );
        }
    }

    fn editor_ui(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let Some(editor) = self.editor.as_mut() else {
            return;
        };
        ui.label(
            RichText::new(if editor.id.is_some() {
                "Edit event"
            } else {
                "New event"
            })
            .strong(),
        );
        ui.add(
            egui::TextEdit::singleline(&mut editor.title)
                .id(egui::Id::new("calendar-title"))
                .hint_text("Title")
                .desired_width(f32::INFINITY),
        );
        egui::ComboBox::from_id_salt("calendar-kind")
            .selected_text(editor.kind.label())
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut editor.kind, Kind::Meeting, "Meeting");
                ui.selectable_value(&mut editor.kind, Kind::Appointment, "Appointment");
            });
        ui.label(RichText::new("Date (YYYY-MM-DD)").small().color(p.muted));
        ui.add(egui::TextEdit::singleline(&mut editor.date).desired_width(f32::INFINITY));
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("Start (24h)").small().color(p.muted));
                ui.add(egui::TextEdit::singleline(&mut editor.start).desired_width(76.0));
            });
            ui.vertical(|ui| {
                ui.label(RichText::new("End (24h)").small().color(p.muted));
                ui.add(egui::TextEdit::singleline(&mut editor.end).desired_width(76.0));
            });
        });
        ui.add(
            egui::TextEdit::singleline(&mut editor.location)
                .hint_text("Location or meeting link (optional)")
                .desired_width(f32::INFINITY),
        );
        if let Some(error) = &editor.error {
            ui.label(RichText::new(error).small().color(p.danger));
        }
        let mut save = false;
        let mut cancel = false;
        let mut delete = false;
        ui.horizontal(|ui| {
            save = ui
                .add_enabled(self.writable, egui::Button::new("Save"))
                .clicked();
            cancel = ui.button("Cancel").clicked();
            if editor.id.is_some()
                && ui
                    .add_enabled(self.writable, egui::Button::new("Delete"))
                    .clicked()
            {
                editor.confirm_delete = true;
            }
        });
        if editor.confirm_delete {
            ui.label(RichText::new("Delete this event?").color(p.danger));
            ui.horizontal(|ui| {
                delete = ui.button("Delete event").clicked();
                if ui.button("Keep it").clicked() {
                    editor.confirm_delete = false;
                }
            });
        }
        if cancel {
            self.editor = None;
        } else if save || delete {
            let mut agenda = self.agenda.clone();
            let result = if delete {
                agenda.events.retain(|event| Some(event.id) != editor.id);
                Ok(())
            } else {
                let next = agenda
                    .events
                    .iter()
                    .map(|event| event.id)
                    .max()
                    .unwrap_or(0)
                    .checked_add(1);
                match editor.id.or(next) {
                    None => Err("No more event IDs are available.".into()),
                    Some(id) => editor.event(id).map(|event| {
                        agenda.events.retain(|old| old.id != id);
                        agenda.events.push(event);
                    }),
                }
            };
            match result.and_then(|()| self.persist(agenda)) {
                Ok(()) => self.editor = None,
                Err(error) => {
                    self.error = Some(error.clone());
                    if let Some(editor) = self.editor.as_mut() {
                        editor.error = Some(error);
                    }
                }
            }
        }
    }
}

enum AgendaRow<'a> {
    Local(&'a Event),
    Outlook(&'a crate::outlook::OutlookEvent),
}

fn month_start(date: NaiveDate) -> NaiveDate {
    date.with_day(1).unwrap()
}

fn shift_month(month: NaiveDate, offset: i32) -> Option<NaiveDate> {
    let index = month.year() * 12 + month.month0() as i32 + offset;
    NaiveDate::from_ymd_opt(index.div_euclid(12), index.rem_euclid(12) as u32 + 1, 1)
}

fn calendar_days(month: NaiveDate) -> Vec<NaiveDate> {
    let first = month_start(month);
    let start = first
        .checked_sub_days(Days::new(first.weekday().num_days_from_monday() as u64))
        .unwrap_or(first);
    let end = shift_month(first, 1).unwrap_or(NaiveDate::MAX);
    let count = end.signed_duration_since(start).num_days().div_euclid(7)
        + i64::from(end.signed_duration_since(start).num_days() % 7 != 0);
    (0..count * 7)
        .filter_map(|offset| start.checked_add_days(Days::new(offset as u64)))
        .collect()
}

fn outlook_range(month: NaiveDate) -> (NaiveDate, NaiveDate) {
    let first = month_start(month);
    let start = first
        .checked_sub_days(Days::new(first.weekday().num_days_from_monday() as u64))
        .unwrap_or(first);
    let end = shift_month(first, 1)
        .and_then(|next| next.checked_add_days(Days::new(7)))
        .unwrap_or(NaiveDate::MAX);
    (start, end)
}

fn outlook_on_date(event: &crate::outlook::OutlookEvent, date: NaiveDate) -> bool {
    date.succ_opt().is_some_and(|next| {
        event.start < next.and_hms_opt(0, 0, 0).unwrap()
            && event.end > date.and_hms_opt(0, 0, 0).unwrap()
    })
}

fn outlook_row(
    ui: &mut egui::Ui,
    p: &Palette,
    event: &crate::outlook::OutlookEvent,
    date: NaiveDate,
) {
    ui.push_id(&event.id, |ui| {
        egui::Frame::NONE
            .inner_margin(egui::Margin::symmetric(0, 5))
            .show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(45.0);
                        let start = if event.all_day {
                            "All day".into()
                        } else if event.start.date() < date {
                            "00:00".into()
                        } else {
                            event.start.format("%H:%M").to_string()
                        };
                        ui.label(RichText::new(start).size(12.0).color(p.accent));
                        if !event.all_day {
                            let end = if event.end.date() > date {
                                "24:00".into()
                            } else {
                                event.end.format("%H:%M").to_string()
                            };
                            ui.label(RichText::new(end).size(11.0).color(p.muted));
                        }
                    });
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        ui.label(RichText::new(&event.title).color(p.text));
                        let kind = if event.is_meeting {
                            "Meeting"
                        } else {
                            "Appointment"
                        };
                        let details = if event.location.is_empty() {
                            format!("Outlook · {kind}")
                        } else {
                            format!("Outlook · {kind} · {}", event.location)
                        };
                        ui.label(RichText::new(details).size(11.0).color(p.muted));
                    });
                });
            });
    });
}

fn valid_agenda(agenda: &Agenda) -> bool {
    let mut ids = std::collections::HashSet::new();
    agenda.version == 1
        && agenda.events.iter().all(|event| {
            !event.title.trim().is_empty() && event.end > event.start && ids.insert(event.id)
        })
}

fn in_week(date: NaiveDate, today: NaiveDate) -> bool {
    (0..=7).contains(&date.signed_duration_since(today).num_days())
}

fn ordered_events(events: &[Event]) -> Vec<&Event> {
    let mut ordered: Vec<_> = events.iter().collect();
    ordered.sort_by_key(|event| (event.date, event.start, event.id));
    ordered
}

fn event_row(ui: &mut egui::Ui, p: &Palette, event: &Event) -> bool {
    let response = egui::Frame::NONE
        .inner_margin(egui::Margin::symmetric(0, 5))
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(45.0);
                    ui.label(
                        RichText::new(event.start.format("%H:%M").to_string())
                            .size(12.0)
                            .color(p.accent),
                    );
                    ui.label(
                        RichText::new(event.end.format("%H:%M").to_string())
                            .size(11.0)
                            .color(p.muted),
                    );
                });
                ui.vertical(|ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new(&event.title).color(p.text));
                    let details = if event.location.is_empty() {
                        event.kind.label().to_string()
                    } else {
                        format!("{} · {}", event.kind.label(), event.location)
                    };
                    ui.label(RichText::new(details).size(11.0).color(p.muted));
                });
            });
        })
        .response;
    ui.interact(
        response.rect,
        response.id.with(event.id),
        egui::Sense::click(),
    )
    .on_hover_cursor(egui::CursorIcon::PointingHand)
    .clicked()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn calendar_month_grid_covers_week_boundaries_and_leap_day() {
        let days = calendar_days(date("2026-11-01"));
        assert_eq!(days.len(), 42);
        assert_eq!(days[0], date("2026-10-26"));
        assert_eq!(*days.last().unwrap(), date("2026-12-06"));
        assert!(calendar_days(date("2024-02-01")).contains(&date("2024-02-29")));
        assert_eq!(shift_month(date("2026-12-01"), 1), Some(date("2027-01-01")));
        assert_eq!(
            shift_month(date("2027-01-01"), -1),
            Some(date("2026-12-01"))
        );
        assert_eq!(
            outlook_range(date("2026-11-01")),
            (date("2026-10-26"), date("2026-12-08"))
        );
    }

    #[test]
    fn calendar_marks_local_and_spanning_outlook_events_and_selects_a_day() {
        let mut calendar = Calendar {
            month: date("2026-11-01"),
            selected_date: date("2026-11-04"),
            ..Calendar::default()
        };
        let mut editor = Editor::new(date("2026-11-12"));
        editor.title = "Appointment".into();
        calendar.agenda.events.push(editor.event(1).unwrap());
        calendar.outlook.events.push(crate::outlook::OutlookEvent {
            id: "overnight".into(),
            title: "Overnight".into(),
            start: date("2026-11-14").and_hms_opt(22, 0, 0).unwrap(),
            end: date("2026-11-16").and_hms_opt(0, 0, 0).unwrap(),
            location: String::new(),
            all_day: false,
            is_meeting: true,
        });
        for day in ["2026-11-12", "2026-11-14", "2026-11-15"] {
            assert!(calendar.has_events(date(day)));
        }
        assert!(!calendar.has_events(date("2026-11-16")));
        let ctx = egui::Context::default();
        let p = Palette::new(crate::theme::Accent::Blue, true);
        let raw = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(260.0, 480.0),
            )),
            events,
            ..Default::default()
        };
        for _ in 0..2 {
            let _ = ctx.run_ui(raw(vec![]), |ui| calendar.month_grid(ui, &p));
        }
        let selected = date("2026-12-03");
        let position = ctx
            .read_response(egui::Id::new(("calendar-day", selected)))
            .unwrap()
            .rect
            .center();
        for pressed in [true, false] {
            let _ = ctx.run_ui(
                raw(vec![
                    egui::Event::PointerMoved(position),
                    egui::Event::PointerButton {
                        pos: position,
                        button: egui::PointerButton::Primary,
                        pressed,
                        modifiers: egui::Modifiers::NONE,
                    },
                ]),
                |ui| calendar.month_grid(ui, &p),
            );
        }
        assert_eq!(calendar.selected_date, selected);
        assert_eq!(calendar.month, date("2026-12-01"));
    }

    #[test]
    fn calendar_week_includes_today_and_seventh_day_across_years() {
        let today = date("2026-12-28");
        assert!(in_week(today, today));
        assert!(in_week(date("2027-01-04"), today));
        assert!(!in_week(date("2027-01-05"), today));
        assert!(!in_week(date("2026-12-27"), today));
    }

    #[test]
    fn calendar_editor_rejects_invalid_dates_and_reversed_times() {
        let mut editor = Editor::new(date("2026-11-04"));
        editor.title = "Review".into();
        editor.date = "2026-02-30".into();
        assert!(editor.event(1).is_err());
        editor.date = "2026-11-04".into();
        editor.end = "08:59".into();
        assert!(editor.event(1).is_err());
        editor.end = "09:00".into();
        assert!(editor.event(1).is_err());
        editor.end = "09:30".into();
        assert!(editor.event(1).is_ok());
    }

    #[test]
    fn calendar_events_sort_by_date_then_time_without_mutating_storage() {
        let mut editor = Editor::new(date("2026-11-04"));
        editor.title = "Meeting".into();
        let early = editor.event(1).unwrap();
        editor.start = "10:00".into();
        editor.end = "11:00".into();
        let late = editor.event(2).unwrap();
        editor.date = "2026-11-05".into();
        let tomorrow = editor.event(3).unwrap();
        let events = vec![tomorrow, late, early];
        assert_eq!(
            ordered_events(&events)
                .iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(events[0].id, 3);
        let agenda = Agenda {
            events,
            ..Agenda::default()
        };
        let json = serde_json::to_string(&agenda).unwrap();
        assert!(valid_agenda(&serde_json::from_str(&json).unwrap()));
    }

    #[test]
    fn calendar_save_refuses_external_changes_and_preserves_existing_bytes() {
        let path = std::env::temp_dir().join(format!(
            "flodo-calendar-{}-{}.json",
            std::process::id(),
            Local::now().timestamp_nanos_opt().unwrap()
        ));
        let mut calendar = Calendar {
            path: path.clone(),
            ..Calendar::default()
        };
        calendar.persist(Agenda::default()).unwrap();
        std::fs::write(&path, "external changes").unwrap();
        assert!(calendar.persist(Agenda::default()).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "external changes");
        calendar.reload();
        assert!(!calendar.writable);
        assert!(!calendar.open);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn calendar_drawer_renders_small_windows_and_keeps_typing_in_its_editor() {
        for (width, height, font_size) in [
            (260.0, 180.0, 13.0),
            (340.0, 480.0, 13.0),
            (620.0, 700.0, 20.0),
        ] {
            let ctx = egui::Context::default();
            ctx.all_styles_mut(|style| {
                style.animation_time = 0.0;
                style.override_font_id = Some(egui::FontId::proportional(font_size));
            });
            let p = Palette::new(crate::theme::Accent::Blue, false);
            let mut calendar = Calendar {
                outlook_refreshed: Some(Instant::now()),
                outlook_range: Some(outlook_range(month_start(Local::now().date_naive()))),
                ..Calendar::default()
            };
            let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, height));
            let raw = || egui::RawInput {
                screen_rect: Some(screen),
                ..Default::default()
            };
            for _ in 0..2 {
                let _ = ctx.run_ui(raw(), |_ui| calendar.show(&ctx, &p));
            }
            assert!(!calendar.open);
            calendar.open = true;
            let today = Local::now().date_naive();
            let mut editor = Editor::new(today);
            editor.title = "A meeting title that should wrap inside the drawer".into();
            calendar.agenda.events.push(editor.event(1).unwrap());
            calendar.outlook.events.push(crate::outlook::OutlookEvent {
                id: "headless-outlook".into(),
                title: "Outlook appointment".into(),
                start: today.and_hms_opt(10, 0, 0).unwrap(),
                end: today.and_hms_opt(11, 0, 0).unwrap(),
                location: "Room one".into(),
                all_day: false,
                is_meeting: false,
            });
            for _ in 0..3 {
                let output = ctx.run_ui(raw(), |_ui| calendar.show(&ctx, &p));
                assert!(!output.shapes.is_empty());
                assert!(output
                    .shapes
                    .iter()
                    .all(|shape| shape.clip_rect.is_finite()));
            }
            calendar.editor = Some(Editor::new(today));
            for _ in 0..2 {
                let _ = ctx.run_ui(raw(), |_ui| calendar.show(&ctx, &p));
            }
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("calendar-title")));
            let mut typing = raw();
            typing
                .events
                .push(egui::Event::Text("Appointment draft".into()));
            let _ = ctx.run_ui(typing, |_ui| calendar.show(&ctx, &p));
            assert_eq!(calendar.editor.as_ref().unwrap().title, "Appointment draft");
            assert!(ctx.egui_wants_keyboard_input());
            assert_eq!(
                calendar.agenda.events.len(),
                1,
                "rendering and typing must not save a new event"
            );
        }
    }
}
