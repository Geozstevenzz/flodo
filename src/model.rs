//! The entire data model. Pure logic, no IO — everything here is unit-tested.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

pub const STORE_VERSION: u32 = 1;

fn default_version() -> u32 {
    STORE_VERSION
}

/// Seconds since the Unix epoch. Avoids a `chrono` dependency.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    #[default]
    None,
    Low,
    Medium,
    High,
}

impl Importance {
    pub const ALL: [Self; 4] = [Self::None, Self::Low, Self::Medium, Self::High];

    pub fn label(self) -> &'static str {
        match self {
            Self::None => "No flag",
            Self::Low => "Low importance",
            Self::Medium => "Medium importance",
            Self::High => "High importance",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomFlag {
    pub id: u64,
    pub name: String,
    pub color: [u8; 3],
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Todo {
    pub id: u64,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub done: bool,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default)]
    pub importance: Importance,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flag_ids: Vec<u64>,
    /// Catches keys written by a newer version so we round-trip instead of
    /// destroying them.
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Todo {
    pub fn new(id: u64, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            body: String::new(),
            done: false,
            created_at: now_secs(),
            completed_at: None,
            expanded: false,
            importance: Importance::None,
            flag_ids: Vec::new(),
            extra: Map::new(),
        }
    }

    pub fn has_body(&self) -> bool {
        !self.body.trim().is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Store {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub todos: Vec<Todo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<CustomFlag>,
    #[serde(flatten, default, skip_serializing_if = "Map::is_empty")]
    pub extra: Map<String, Value>,
}

impl Default for Store {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            todos: Vec::new(),
            flags: Vec::new(),
            extra: Map::new(),
        }
    }
}

impl Store {
    fn flag_name(&self, name: &str, editing: Option<u64>) -> Result<String, &'static str> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Enter a flag name.");
        }
        if name.chars().count() > 40 || name.chars().any(char::is_control) {
            return Err("Use a single-line name of at most 40 characters.");
        }
        if self
            .flags
            .iter()
            .any(|flag| Some(flag.id) != editing && flag.name.to_lowercase() == name.to_lowercase())
        {
            return Err("A flag with that name already exists.");
        }
        Ok(name.to_owned())
    }

    pub fn create_flag(&mut self, name: &str, color: [u8; 3]) -> Result<u64, &'static str> {
        let name = self.flag_name(name, None)?;
        let id = (now_millis() << 12).max(
            self.flags
                .iter()
                .map(|flag| flag.id)
                .max()
                .unwrap_or(0)
                .saturating_add(1),
        );
        self.flags.push(CustomFlag {
            id,
            name,
            color,
            extra: Map::new(),
        });
        Ok(id)
    }

    pub fn update_flag(&mut self, id: u64, name: &str, color: [u8; 3]) -> Result<(), &'static str> {
        let name = self.flag_name(name, Some(id))?;
        let flag = self
            .flags
            .iter_mut()
            .find(|flag| flag.id == id)
            .ok_or("This flag no longer exists.")?;
        flag.name = name;
        flag.color = color;
        Ok(())
    }

    pub fn delete_flag(&mut self, id: u64) {
        self.flags.retain(|flag| flag.id != id);
        for todo in &mut self.todos {
            todo.flag_ids.retain(|flag_id| *flag_id != id);
        }
    }

    pub fn set_flag(&mut self, todo_id: u64, flag_id: u64, assigned: bool) {
        if !self.flags.iter().any(|flag| flag.id == flag_id) {
            return;
        }
        if let Some(todo) = self.get_mut(todo_id) {
            todo.flag_ids.retain(|id| *id != flag_id);
            if assigned {
                todo.flag_ids.push(flag_id);
            }
        }
    }

    /// Monotonic and unique: timestamp-derived so ids stay meaningful across
    /// sessions, but always at least `max_existing + 1` so a clock that jumps
    /// backwards can never produce a collision.
    fn next_id(&self) -> u64 {
        let from_clock = now_millis() << 12;
        let from_data = self.todos.iter().map(|t| t.id).max().map_or(0, |m| m + 1);
        from_clock.max(from_data)
    }

    /// Inserts at the top, matching where the composer sits: you type, press
    /// Enter, and the new todo appears on the line directly below.
    pub fn add(&mut self, title: impl Into<String>) -> u64 {
        let id = self.next_id();
        self.todos.insert(0, Todo::new(id, title));
        id
    }

    pub fn index_of(&self, id: u64) -> Option<usize> {
        self.todos.iter().position(|t| t.id == id)
    }

    pub fn get(&self, id: u64) -> Option<&Todo> {
        self.todos.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Todo> {
        self.todos.iter_mut().find(|t| t.id == id)
    }

    pub fn toggle(&mut self, id: u64) {
        if let Some(t) = self.get_mut(id) {
            t.done = !t.done;
            t.completed_at = if t.done { Some(now_secs()) } else { None };
        }
    }

    /// Returns the removed todo and where it was, so undo can put it back.
    pub fn remove(&mut self, id: u64) -> Option<(usize, Todo)> {
        let ix = self.index_of(id)?;
        Some((ix, self.todos.remove(ix)))
    }

    pub fn restore(&mut self, index: usize, mut todo: Todo) {
        // A flag may have been deleted while this todo was in the undo stack.
        todo.flag_ids
            .retain(|id| self.flags.iter().any(|flag| flag.id == *id));
        let ix = index.min(self.todos.len());
        self.todos.insert(ix, todo);
    }

    /// Puts back several removed todos. Each index is where that todo sat
    /// before *any* of them were removed, so they go back lowest first: every
    /// earlier insert is then already in place when a later index is counted.
    pub fn restore_all(&mut self, mut removed: Vec<(usize, Todo)>) {
        removed.sort_by_key(|(ix, _)| *ix);
        for (ix, todo) in removed {
            self.restore(ix, todo);
        }
    }

    /// Adds several todos at once, keeping their order: the first item ends
    /// up on top, the way a pasted list reads. Returns the new ids.
    pub fn add_all(&mut self, items: &[PastedItem]) -> Vec<u64> {
        let mut ids = Vec::with_capacity(items.len());
        for item in items.iter().rev() {
            let id = self.add(item.title.clone());
            if item.done {
                self.toggle(id);
            }
            ids.push(id);
        }
        ids.reverse();
        ids
    }

    /// Removes every completed todo, returning each with the index it had so
    /// the whole sweep can be undone in one go.
    pub fn clear_completed(&mut self) -> Vec<(usize, Todo)> {
        let mut removed = Vec::new();
        let mut kept = Vec::with_capacity(self.todos.len());
        for (ix, todo) in std::mem::take(&mut self.todos).into_iter().enumerate() {
            if todo.done {
                removed.push((ix, todo));
            } else {
                kept.push(todo);
            }
        }
        self.todos = kept;
        removed
    }

    pub fn completed_count(&self) -> usize {
        self.todos.iter().filter(|t| t.done).count()
    }

    /// Moves a todo to an absolute position. Out-of-range targets clamp rather
    /// than panic, so callers can be sloppy.
    pub fn move_to(&mut self, id: u64, to: usize) {
        let Some(from) = self.index_of(id) else {
            return;
        };
        let to = to.min(self.todos.len().saturating_sub(1));
        if from == to {
            return;
        }
        let t = self.todos.remove(from);
        self.todos.insert(to, t);
    }

    pub fn move_up(&mut self, id: u64) {
        if let Some(ix) = self.index_of(id) {
            if ix > 0 {
                self.todos.swap(ix, ix - 1);
            }
        }
    }

    pub fn move_down(&mut self, id: u64) {
        if let Some(ix) = self.index_of(id) {
            if ix + 1 < self.todos.len() {
                self.todos.swap(ix, ix + 1);
            }
        }
    }

    pub fn any_completed(&self) -> bool {
        self.todos.iter().any(|t| t.done)
    }

    /// Ids in display order, honouring the hide-completed toggle.
    pub fn visible_ids(&self, hide_completed: bool) -> Vec<u64> {
        self.todos
            .iter()
            .filter(|t| !(hide_completed && t.done))
            .map(|t| t.id)
            .collect()
    }
}

/// One line of a pasted list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedItem {
    pub title: String,
    pub done: bool,
}

/// Turns pasted text into to-dos, one per non-blank line.
///
/// A list copied from anywhere — a markdown checklist, a bulleted note, a
/// numbered email — arrives with its markers still on. Those are stripped, and
/// a ticked checkbox (`- [x]`) comes in already done.
pub fn parse_paste(text: &str) -> Vec<PastedItem> {
    text.lines()
        .filter_map(|line| {
            let mut rest = line.trim();
            // A bullet: `-`, `*`, `+` or `•`, then a space.
            for bullet in ["- ", "* ", "+ ", "• "] {
                if let Some(r) = rest.strip_prefix(bullet) {
                    rest = r.trim_start();
                    break;
                }
            }
            // Or a number: `1.` or `1)`, then a space.
            let digits = rest.chars().take_while(char::is_ascii_digit).count();
            if digits > 0 {
                let after = &rest[digits..];
                if let Some(r) = after
                    .strip_prefix(". ")
                    .or_else(|| after.strip_prefix(") "))
                {
                    rest = r.trim_start();
                }
            }
            let mut done = false;
            for (mark, is_done) in [("[ ] ", false), ("[x] ", true), ("[X] ", true)] {
                if let Some(r) = rest.strip_prefix(mark) {
                    rest = r.trim_start();
                    done = is_done;
                    break;
                }
            }
            let title = rest.trim();
            (!title.is_empty()).then(|| PastedItem {
                title: title.to_string(),
                done,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn custom_flags_survive_legacy_roundtrip_and_cli_style_mutations() {
        let mut store: Store = serde_json::from_str(r#"{"version":1,"todos":[{"id":7,"title":"Keep me","body":"notes","future":true}],"futureStore":42}"#).unwrap();
        assert!(store.flags.is_empty());
        assert!(store.todos[0].flag_ids.is_empty());
        let id = store.create_flag("  Work  ", [1, 2, 3]).unwrap();
        store.set_flag(7, id, true);
        store.set_flag(7, id, true);
        store.todos[0].importance = Importance::High;
        store.toggle(7);
        let restored: Store =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(restored.flags[0].name, "Work");
        assert_eq!(restored.flags[0].color, [1, 2, 3]);
        assert_eq!(restored.todos[0].flag_ids, vec![id]);
        assert_eq!(restored.todos[0].importance, Importance::High);
        assert_eq!(restored.todos[0].body, "notes");
        assert_eq!(restored.todos[0].extra["future"], true);
        assert_eq!(restored.extra["futureStore"], 42);
    }

    #[test]
    fn custom_flags_validate_names_and_clear_assignments_on_delete_and_undo() {
        let mut store = Store::default();
        let a = store.add("a");
        let b = store.add("b");
        assert!(store.create_flag("  ", [0; 3]).is_err());
        assert!(store.create_flag("two\nlines", [0; 3]).is_err());
        assert!(store.create_flag(&"x".repeat(41), [0; 3]).is_err());
        let work = store.create_flag("Work", [1, 2, 3]).unwrap();
        assert!(store.create_flag(" work ", [0; 3]).is_err());
        let personal = store.create_flag("Personal", [4, 5, 6]).unwrap();
        assert!(store.update_flag(work, "PERSONAL", [0; 3]).is_err());
        store.update_flag(work, " Office ", [7, 8, 9]).unwrap();
        assert_eq!(store.flags[0].name, "Office");
        assert_eq!(store.flags[0].color, [7, 8, 9]);
        store.set_flag(a, work, true);
        store.set_flag(a, personal, true);
        store.set_flag(b, work, true);
        store.set_flag(a, work, false);
        assert_eq!(store.get(a).unwrap().flag_ids, vec![personal]);
        let (index, removed) = store.remove(b).unwrap();
        store.delete_flag(work);
        store.restore(index, removed);
        assert!(store.get(b).unwrap().flag_ids.is_empty());
        store.delete_flag(personal);
        assert!(store.get(a).unwrap().flag_ids.is_empty());
        assert_eq!(store.visible_ids(false), vec![b, a]);
    }

    fn store_with(n: usize) -> Store {
        let mut s = Store::default();
        for i in 0..n {
            s.add(format!("task {i}"));
        }
        s
    }

    #[test]
    fn add_puts_the_newest_todo_at_the_top() {
        let s = store_with(50);
        assert_eq!(s.todos.len(), 50);
        assert_eq!(s.todos[0].title, "task 49", "newest first");
        assert_eq!(s.todos[49].title, "task 0");
    }

    #[test]
    fn ids_are_unique_and_strictly_increasing() {
        let s = store_with(50);
        // Newest is at index 0, so ids descend down the list.
        for w in s.todos.windows(2) {
            assert!(w[0].id > w[1].id, "ids must be unique and ordered");
        }
    }

    #[test]
    fn ids_stay_unique_after_reload_even_if_clock_went_backwards() {
        // Simulates a file written far in the future, then reopened.
        let mut s = Store::default();
        s.todos.push(Todo::new(u64::MAX / 2, "from the future"));
        let id = s.add("now");
        assert!(id > u64::MAX / 2);
    }

    #[test]
    fn toggle_sets_and_clears_completed_at() {
        let mut s = store_with(1);
        let id = s.todos[0].id;
        s.toggle(id);
        assert!(s.get(id).unwrap().done);
        assert!(s.get(id).unwrap().completed_at.is_some());
        s.toggle(id);
        assert!(!s.get(id).unwrap().done);
        assert!(s.get(id).unwrap().completed_at.is_none());
    }

    #[test]
    fn remove_then_restore_puts_it_back_where_it_was() {
        let mut s = store_with(5);
        let id = s.todos[2].id;
        let (ix, todo) = s.remove(id).unwrap();
        assert_eq!(ix, 2);
        assert_eq!(s.todos.len(), 4);
        s.restore(ix, todo);
        assert_eq!(s.todos.len(), 5);
        assert_eq!(s.todos[2].id, id);
    }

    #[test]
    fn restore_past_the_end_clamps_instead_of_panicking() {
        let mut s = store_with(2);
        s.restore(99, Todo::new(1, "x"));
        assert_eq!(s.todos.len(), 3);
        assert_eq!(s.todos[2].title, "x");
    }

    #[test]
    fn move_up_and_down_are_no_ops_at_the_boundaries() {
        let mut s = store_with(3);
        let (first, last) = (s.todos[0].id, s.todos[2].id);
        s.move_up(first);
        assert_eq!(s.todos[0].id, first);
        s.move_down(last);
        assert_eq!(s.todos[2].id, last);
    }

    #[test]
    fn move_down_then_up_returns_to_start() {
        let mut s = store_with(3);
        let id = s.todos[0].id;
        s.move_down(id);
        assert_eq!(s.index_of(id), Some(1));
        s.move_up(id);
        assert_eq!(s.index_of(id), Some(0));
    }

    #[test]
    fn move_to_clamps_and_reorders() {
        let mut s = store_with(4);
        let id = s.todos[0].id;
        s.move_to(id, 99);
        assert_eq!(s.index_of(id), Some(3));
        s.move_to(id, 0);
        assert_eq!(s.index_of(id), Some(0));
    }

    #[test]
    fn mutations_on_a_missing_id_do_nothing() {
        let mut s = store_with(2);
        let before = s.todos.clone();
        s.toggle(999);
        s.move_up(999);
        s.move_down(999);
        s.move_to(999, 0);
        assert!(s.remove(999).is_none());
        assert_eq!(s.todos, before);
    }

    #[test]
    fn visible_ids_respects_hide_completed() {
        let mut s = store_with(3);
        let mid = s.todos[1].id;
        s.toggle(mid);
        assert_eq!(s.visible_ids(false).len(), 3);
        let shown = s.visible_ids(true);
        assert_eq!(shown.len(), 2);
        assert!(!shown.contains(&mid));
        assert!(s.any_completed());
    }

    #[test]
    fn unknown_fields_survive_a_round_trip() {
        let json = r#"{
            "version": 99,
            "todos": [{
                "id": 7, "title": "hi", "someFutureField": {"a": 1}
            }],
            "topLevelFuture": [1, 2, 3]
        }"#;
        let s: Store = serde_json::from_str(json).unwrap();
        assert_eq!(s.version, 99);
        assert_eq!(s.todos[0].title, "hi");

        let out = serde_json::to_string(&s).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["topLevelFuture"], serde_json::json!([1, 2, 3]));
        assert_eq!(v["todos"][0]["someFutureField"]["a"], 1);
    }

    #[test]
    fn missing_optional_fields_use_defaults() {
        let s: Store = serde_json::from_str(r#"{"todos":[{"id":1,"title":"a"}]}"#).unwrap();
        assert_eq!(s.version, STORE_VERSION);
        let t = &s.todos[0];
        assert!(!t.done && !t.expanded && t.body.is_empty() && t.completed_at.is_none());
        assert_eq!(t.importance, Importance::None);
    }

    #[test]
    fn importance_round_trips_without_losing_legacy_todos_or_unknown_fields() {
        let legacy = r#"{"version":1,"todos":[{"id":7,"title":"Old task","body":"notes","done":true,"created_at":42,"completed_at":43,"expanded":true,"future":"kept"},{"id":8,"title":"Next"}],"futureStore":true}"#;
        let mut store: Store = serde_json::from_str(legacy).unwrap();
        assert!(store.todos.iter().all(|t| t.importance == Importance::None));
        let before = store.clone();
        for importance in Importance::ALL {
            store.todos[0].importance = importance;
            let encoded = serde_json::to_string(&store).unwrap();
            let loaded: Store = serde_json::from_str(&encoded).unwrap();
            assert_eq!(loaded.todos[0].importance, importance);
            let mut old_task = loaded.todos[0].clone();
            old_task.importance = Importance::None;
            assert_eq!(old_task, before.todos[0]);
            assert_eq!(loaded.todos[1], before.todos[1]);
            assert_eq!(loaded.extra, before.extra);
            assert_eq!(loaded.version, 1);
        }
    }

    #[test]
    fn a_pasted_list_loses_its_markers_and_blank_lines() {
        let got = parse_paste(
            "Groceries:\n\n- oat milk\n* coffee\n  + bread  \n• jam\n1. call Sam\n12) renew domain\n",
        );
        let titles: Vec<_> = got.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Groceries:",
                "oat milk",
                "coffee",
                "bread",
                "jam",
                "call Sam",
                "renew domain"
            ]
        );
        assert!(got.iter().all(|i| !i.done));
    }

    #[test]
    fn pasted_checkboxes_keep_their_state() {
        let got = parse_paste("- [ ] open\n- [x] shut\n[X] also shut\n");
        let state: Vec<_> = got.iter().map(|i| (i.title.as_str(), i.done)).collect();
        assert_eq!(
            state,
            [("open", false), ("shut", true), ("also shut", true)]
        );
    }

    #[test]
    fn a_number_that_is_the_title_is_not_a_marker() {
        let got = parse_paste("2026 plans\n3.5 kg flour");
        assert_eq!(got[0].title, "2026 plans");
        assert_eq!(got[1].title, "3.5 kg flour");
    }

    #[test]
    fn add_all_keeps_the_pasted_order_top_down() {
        let mut s = store_with(1);
        let ids = s.add_all(&parse_paste("first\n- [x] second\nthird"));
        assert_eq!(ids.len(), 3);
        let titles: Vec<_> = s.todos.iter().map(|t| t.title.as_str()).collect();
        assert_eq!(titles, ["first", "second", "third", "task 0"]);
        assert_eq!(s.todos[0].id, ids[0]);
        assert!(s.todos[1].done && s.todos[1].completed_at.is_some());
    }

    #[test]
    fn clearing_completed_can_be_undone_exactly() {
        let mut s = store_with(6);
        for ix in [0, 2, 5] {
            let id = s.todos[ix].id;
            s.toggle(id);
        }
        let before = s.todos.clone();
        let removed = s.clear_completed();
        assert_eq!(removed.len(), 3);
        assert_eq!(s.todos.len(), 3);
        assert!(!s.any_completed());
        assert_eq!(s.completed_count(), 0);
        s.restore_all(removed);
        assert_eq!(s.todos, before, "every row back where it was");
    }

    #[test]
    fn has_body_ignores_whitespace_only_bodies() {
        let mut t = Todo::new(1, "x");
        assert!(!t.has_body());
        t.body = "   \n\t ".into();
        assert!(!t.has_body());
        t.body = "note".into();
        assert!(t.has_body());
    }
}
