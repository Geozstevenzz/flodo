//! Three-way merge of the synced files. Pure logic, no IO.
//!
//! `base` is the last state both sides agreed on, `local` is this computer's
//! file and `remote` is what the other computers pushed since. Records are
//! matched by id and merged field by field: a side that changed a field wins,
//! and when both changed it differently the local value wins. A record one
//! side deleted and the other edited is kept, because losing an edit is worse
//! than having to delete something twice.

use crate::model::{CustomFlag, Store, Todo};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

type Obj = Map<String, Value>;

/// Per-machine UI state: never merged, never pushed.
const EXPANDED: &str = "expanded";

/// The three-way rule for one value. `None` is "absent", so an optional key
/// one side added or removed follows the same rule as any edit.
fn pick<'a>(
    base: Option<&'a Value>,
    local: Option<&'a Value>,
    remote: Option<&'a Value>,
) -> Option<&'a Value> {
    if local == remote || remote == base {
        local
    } else if local == base {
        remote
    } else {
        local
    }
}

fn merge_object(base: Option<&Obj>, local: &Obj, remote: &Obj) -> Obj {
    let mut keys: Vec<&String> = local.keys().chain(remote.keys()).collect();
    if let Some(base) = base {
        keys.extend(base.keys());
    }
    keys.sort();
    keys.dedup();
    keys.into_iter()
        .filter_map(|key| {
            pick(
                base.and_then(|b| b.get(key)),
                local.get(key),
                remote.get(key),
            )
            .map(|value| (key.clone(), value.clone()))
        })
        .collect()
}

fn id_of(record: &Obj) -> Option<u64> {
    record.get("id").and_then(Value::as_u64)
}

/// True when `side` put the records it shares with `base` in another order.
/// Additions and deletions alone are not a reorder.
fn reordered(base: &[u64], side: &[u64]) -> bool {
    let in_base: HashSet<_> = base.iter().collect();
    let in_side: HashSet<_> = side.iter().collect();
    let a = side.iter().filter(|id| in_base.contains(id));
    let b = base.iter().filter(|id| in_side.contains(id));
    !a.eq(b)
}

/// The order rule: the remote order is used only when the remote alone
/// reordered; otherwise the local order. Records that only the other side has
/// go in where they sit there: after the same neighbour, or at the top.
fn merge_order(base: &[u64], local: &[u64], remote: &[u64], keep: &HashSet<u64>) -> Vec<u64> {
    let (primary, other) = if !reordered(base, local) && reordered(base, remote) {
        (remote, local)
    } else {
        (local, remote)
    };
    let mut out: Vec<u64> = primary
        .iter()
        .copied()
        .filter(|id| keep.contains(id))
        .collect();
    let mut placed: HashSet<u64> = out.iter().copied().collect();
    let mut previous: Option<u64> = None;
    for id in other.iter().copied().filter(|id| keep.contains(id)) {
        if placed.insert(id) {
            // Right after the record it follows over there, or at the top if
            // it is first there: positions shift, neighbours do not.
            let at = previous
                .and_then(|p| out.iter().position(|x| *x == p))
                .map_or(0, |ix| ix + 1);
            out.insert(at, id);
        }
        previous = Some(id);
    }
    out
}

/// Merges two lists of records keyed by `id`, keeping list order meaningful.
/// Records without a numeric id cannot be matched, so they are kept from the
/// local side only.
fn merge_records(base: &[Obj], local: &[Obj], remote: &[Obj]) -> Vec<Obj> {
    let index = |list: &[Obj]| -> HashMap<u64, Obj> {
        list.iter()
            .filter_map(|r| Some((id_of(r)?, r.clone())))
            .collect()
    };
    let ids = |list: &[Obj]| -> Vec<u64> { list.iter().filter_map(id_of).collect() };
    let (b, l, r) = (index(base), index(local), index(remote));

    let mut merged: HashMap<u64, Obj> = HashMap::new();
    for id in l.keys().chain(r.keys()) {
        if merged.contains_key(id) {
            continue;
        }
        let record = match (b.get(id), l.get(id), r.get(id)) {
            (base, Some(local), Some(remote)) => Some(merge_object(base, local, remote)),
            // Present on one side only: added there, or deleted by the other.
            // A delete applies only when the survivor is still unchanged.
            (Some(base), Some(only), None) | (Some(base), None, Some(only)) => {
                (only != base).then(|| only.clone())
            }
            (None, Some(only), None) | (None, None, Some(only)) => Some(only.clone()),
            (_, None, None) => None,
        };
        if let Some(record) = record {
            merged.insert(*id, record);
        }
    }

    let keep: HashSet<u64> = merged.keys().copied().collect();
    let mut out: Vec<Obj> = merge_order(&ids(base), &ids(local), &ids(remote), &keep)
        .into_iter()
        .filter_map(|id| merged.remove(&id))
        .collect();
    out.extend(local.iter().filter(|r| id_of(r).is_none()).cloned());
    out
}

fn to_obj<T: serde::Serialize>(value: &T) -> Obj {
    match serde_json::to_value(value) {
        Ok(Value::Object(obj)) => obj,
        _ => Obj::new(),
    }
}

fn objs<T: serde::Serialize>(list: &[T]) -> Vec<Obj> {
    list.iter().map(to_obj).collect()
}

/// Top-level keys other than the lists, merged per key. `version` is the
/// highest either side has seen.
fn merge_extra(base: &Obj, local: &Obj, remote: &Obj) -> Obj {
    merge_object(Some(base), local, remote)
}

/// Three-way merge of the task list.
pub fn merge_store(base: &Store, local: &Store, remote: &Store) -> Store {
    let strip = |store: &Store| -> Vec<Obj> {
        objs(&store.todos)
            .into_iter()
            .map(|mut todo| {
                todo.remove(EXPANDED);
                todo
            })
            .collect()
    };
    let local_by_id: HashMap<u64, &Todo> = local.todos.iter().map(|t| (t.id, t)).collect();
    let remote_by_id: HashMap<u64, &Todo> = remote.todos.iter().map(|t| (t.id, t)).collect();

    let mut todos: Vec<Todo> = merge_records(&strip(base), &strip(local), &strip(remote))
        .into_iter()
        .filter_map(|obj| {
            let id = id_of(&obj);
            // Every field came from a valid record, so this only fails if a
            // future version adds a cross-field rule; then take a whole side.
            serde_json::from_value::<Todo>(Value::Object(obj))
                .ok()
                .or_else(|| {
                    let id = id?;
                    local_by_id
                        .get(&id)
                        .or_else(|| remote_by_id.get(&id))
                        .map(|t| (*t).clone())
                })
        })
        .collect();
    for todo in &mut todos {
        todo.expanded = local_by_id.get(&todo.id).is_some_and(|t| t.expanded);
    }

    let flags = merge_flags(&base.flags, &local.flags, &remote.flags);
    let known: HashSet<u64> = flags.iter().map(|f| f.id).collect();
    for todo in &mut todos {
        let mut seen = HashSet::new();
        todo.flag_ids
            .retain(|id| known.contains(id) && seen.insert(*id));
    }

    Store {
        version: base.version.max(local.version).max(remote.version),
        todos,
        flags,
        extra: merge_extra(&base.extra, &local.extra, &remote.extra),
    }
}

/// Flags merge like tasks. Names must stay unique, so when two different
/// flags end up with the same name the one that did not arrive with that name
/// from this computer gets a suffix.
fn merge_flags(
    base: &[CustomFlag],
    local: &[CustomFlag],
    remote: &[CustomFlag],
) -> Vec<CustomFlag> {
    let mut flags: Vec<CustomFlag> = merge_records(&objs(base), &objs(local), &objs(remote))
        .into_iter()
        .filter_map(|obj| serde_json::from_value(Value::Object(obj)).ok())
        .collect();
    let local_name: HashMap<u64, &str> = local.iter().map(|f| (f.id, f.name.as_str())).collect();
    let is_local = |flag: &CustomFlag| local_name.get(&flag.id) == Some(&flag.name.as_str());

    let all: HashSet<String> = flags.iter().map(|f| f.name.to_lowercase()).collect();
    let mut taken: HashSet<String> = HashSet::new();
    let mut order: Vec<usize> = (0..flags.len()).collect();
    // Local names claim first; the stable sort keeps list order within each.
    order.sort_by_key(|&ix| !is_local(&flags[ix]));
    for ix in order {
        let name = flags[ix].name.clone();
        if taken.insert(name.to_lowercase()) {
            continue;
        }
        let mut n = 2;
        let renamed = loop {
            let candidate = format!("{name} ({n})");
            let lower = candidate.to_lowercase();
            if !all.contains(&lower) && taken.insert(lower) {
                break candidate;
            }
            n += 1;
        };
        flags[ix].name = renamed;
    }
    flags
}

/// The copy that goes to the remote: per-machine state removed.
pub fn for_repo(store: &Store) -> Store {
    let mut store = store.clone();
    for todo in &mut store.todos {
        todo.expanded = false;
    }
    store
}

/// What a merge changed on this computer, for the status line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Changes {
    pub added: usize,
    pub removed: usize,
    pub updated: usize,
}

impl Changes {
    pub fn total(&self) -> usize {
        self.added + self.removed + self.updated
    }

    pub fn add(&mut self, other: Changes) {
        self.added += other.added;
        self.removed += other.removed;
        self.updated += other.updated;
    }

    fn between(before: &[Obj], after: &[Obj]) -> Self {
        let before: HashMap<Option<u64>, &Obj> = before.iter().map(|r| (id_of(r), r)).collect();
        let after: HashMap<Option<u64>, &Obj> = after.iter().map(|r| (id_of(r), r)).collect();
        let mut changes = Self::default();
        for (id, record) in &after {
            match before.get(id) {
                None => changes.added += 1,
                Some(old) if old != record => changes.updated += 1,
                Some(_) => {}
            }
        }
        changes.removed = before.keys().filter(|id| !after.contains_key(id)).count();
        changes
    }
}

pub fn todo_changes(before: &Store, after: &Store) -> Changes {
    Changes::between(&objs(&before.todos), &objs(&after.todos))
}

fn events(agenda: &Value) -> Vec<Obj> {
    agenda
        .get("events")
        .and_then(Value::as_array)
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

pub fn event_changes(before: &Value, after: &Value) -> Changes {
    Changes::between(&events(before), &events(after))
}

/// Three-way merge of the calendar, in its canonical JSON form.
///
/// Event ids used to be small sequential numbers, so two computers can both
/// have created "event 3". An id absent from the base with different content
/// on each side is that collision: both are kept and the local one gets a
/// fresh id.
pub fn merge_agenda(base: &Value, local: &Value, remote: &Value) -> Value {
    let (base_events, mut local_events, remote_events) =
        (events(base), events(local), events(remote));
    let in_base: HashSet<u64> = base_events.iter().filter_map(id_of).collect();
    let remote_by_id: HashMap<u64, &Obj> = remote_events
        .iter()
        .filter_map(|e| Some((id_of(e)?, e)))
        .collect();
    let mut used: HashSet<u64> = base_events
        .iter()
        .chain(&local_events)
        .chain(&remote_events)
        .filter_map(id_of)
        .collect();
    for event in &mut local_events {
        let Some(id) = id_of(event) else { continue };
        if !in_base.contains(&id) && remote_by_id.get(&id).is_some_and(|r| *r != event) {
            let fresh = crate::calendar::new_event_id(used.iter().copied());
            used.insert(fresh);
            event.insert("id".into(), Value::from(fresh));
        }
    }

    let local_by_id: HashMap<u64, Obj> = local_events
        .iter()
        .filter_map(|e| Some((id_of(e)?, e.clone())))
        .collect();
    let merged: Vec<Value> = merge_records(&base_events, &local_events, &remote_events)
        .into_iter()
        .map(|event| {
            let event = Value::Object(event);
            if crate::calendar::event_is_valid(&event) {
                return event;
            }
            // Mixed fields made no sense together (say, one side's start with
            // the other side's end): take one side's event whole.
            let id = event.get("id").and_then(Value::as_u64);
            id.and_then(|id| {
                local_by_id
                    .get(&id)
                    .or_else(|| remote_by_id.get(&id).copied())
                    .cloned()
            })
            .map(Value::Object)
            .unwrap_or(event)
        })
        .collect();

    let top = |v: &Value| -> Obj {
        let mut obj = v.as_object().cloned().unwrap_or_default();
        obj.remove("events");
        obj
    };
    let version = [base, local, remote]
        .iter()
        .filter_map(|v| v.get("version").and_then(Value::as_u64))
        .max()
        .unwrap_or(1);
    let mut out = merge_extra(&top(base), &top(local), &top(remote));
    out.insert("version".into(), Value::from(version));
    out.insert("events".into(), Value::Array(merged));
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Importance;
    use serde_json::json;

    fn todo(id: u64, title: &str) -> Todo {
        let mut t = Todo::new(id, title);
        t.created_at = 100;
        t
    }

    fn store(todos: &[Todo]) -> Store {
        Store {
            todos: todos.to_vec(),
            ..Default::default()
        }
    }

    fn ids(s: &Store) -> Vec<u64> {
        s.todos.iter().map(|t| t.id).collect()
    }

    fn titles(s: &Store) -> Vec<&str> {
        s.todos.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn an_empty_base_gives_the_union_of_both_sides() {
        let local = store(&[todo(1, "a"), todo(2, "b")]);
        let remote = store(&[todo(3, "c"), todo(2, "b")]);
        let merged = merge_store(&Store::default(), &local, &remote);
        assert_eq!(ids(&merged), vec![3, 1, 2]);
    }

    #[test]
    fn additions_on_either_side_are_kept() {
        let base = store(&[todo(1, "a")]);
        let local = store(&[todo(2, "local"), todo(1, "a")]);
        let remote = store(&[todo(1, "a"), todo(3, "remote")]);
        let merged = merge_store(&base, &local, &remote);
        assert_eq!(titles(&merged), vec!["local", "a", "remote"]);
    }

    #[test]
    fn a_task_added_at_the_top_elsewhere_arrives_at_the_top() {
        let base = store(&[todo(1, "a"), todo(2, "b")]);
        let local = store(&[todo(1, "a"), todo(2, "b")]);
        let remote = store(&[todo(9, "new"), todo(1, "a"), todo(2, "b")]);
        assert_eq!(ids(&merge_store(&base, &local, &remote)), vec![9, 1, 2]);
    }

    #[test]
    fn unchanged_tasks_deleted_on_either_side_are_deleted() {
        let base = store(&[todo(1, "a"), todo(2, "b"), todo(3, "c")]);
        let local = store(&[todo(1, "a"), todo(3, "c")]);
        let remote = store(&[todo(1, "a"), todo(2, "b")]);
        assert_eq!(ids(&merge_store(&base, &local, &remote)), vec![1]);
    }

    #[test]
    fn an_edit_beats_a_delete_from_either_side() {
        let base = store(&[todo(1, "a"), todo(2, "b")]);
        let local = store(&[todo(1, "a edited"), todo(2, "b")]);
        let remote = store(&[todo(2, "b edited")]);
        let merged = merge_store(&base, &local, &remote);
        assert_eq!(titles(&merged), vec!["a edited", "b edited"]);

        let local = store(&[todo(1, "a")]);
        let remote = store(&[todo(1, "a"), todo(2, "b changed")]);
        assert_eq!(
            titles(&merge_store(&base, &local, &remote)),
            vec!["a", "b changed"]
        );
    }

    #[test]
    fn edits_to_different_fields_are_both_kept() {
        let base = store(&[todo(1, "a")]);
        let mut l = todo(1, "renamed here");
        l.importance = Importance::None;
        let mut r = todo(1, "a");
        r.done = true;
        r.completed_at = Some(200);
        r.target_date = chrono::NaiveDate::from_ymd_opt(2026, 12, 1);
        let merged = merge_store(&base, &store(&[l]), &store(&[r]));
        let t = &merged.todos[0];
        assert_eq!(t.title, "renamed here");
        assert!(t.done);
        assert_eq!(t.completed_at, Some(200));
        assert_eq!(t.target_date, chrono::NaiveDate::from_ymd_opt(2026, 12, 1));
    }

    #[test]
    fn the_same_field_changed_on_both_sides_keeps_the_local_value() {
        let base = store(&[todo(1, "a")]);
        let merged = merge_store(
            &base,
            &store(&[todo(1, "mine")]),
            &store(&[todo(1, "theirs")]),
        );
        assert_eq!(merged.todos[0].title, "mine");
    }

    #[test]
    fn a_field_cleared_remotely_is_cleared_here() {
        let mut b = todo(1, "a");
        b.target_date = chrono::NaiveDate::from_ymd_opt(2026, 1, 1);
        let base = store(&[b.clone()]);
        let remote = store(&[todo(1, "a")]);
        let merged = merge_store(&base, &base, &remote);
        assert_eq!(merged.todos[0].target_date, None);
    }

    #[test]
    fn order_follows_the_remote_only_when_only_the_remote_reordered() {
        let base = store(&[todo(1, "a"), todo(2, "b"), todo(3, "c")]);
        let remote = store(&[todo(3, "c"), todo(1, "a"), todo(2, "b")]);
        assert_eq!(ids(&merge_store(&base, &base, &remote)), vec![3, 1, 2]);

        let local = store(&[todo(2, "b"), todo(1, "a"), todo(3, "c")]);
        assert_eq!(ids(&merge_store(&base, &local, &remote)), vec![2, 1, 3]);

        // A local addition alone is not a reorder, so the remote order holds.
        let local = store(&[todo(4, "d"), todo(1, "a"), todo(2, "b"), todo(3, "c")]);
        assert_eq!(ids(&merge_store(&base, &local, &remote)), vec![4, 3, 1, 2]);
    }

    #[test]
    fn expanded_is_never_merged_and_never_pushed() {
        let base = store(&[todo(1, "a"), todo(2, "b")]);
        let mut l1 = todo(1, "a");
        l1.expanded = true;
        let mut r2 = todo(2, "b");
        r2.expanded = true;
        let merged = merge_store(
            &base,
            &store(&[l1, todo(2, "b")]),
            &store(&[todo(1, "a"), r2]),
        );
        assert!(merged.todos[0].expanded, "local value kept");
        assert!(!merged.todos[1].expanded, "remote value ignored");
        assert!(for_repo(&merged).todos.iter().all(|t| !t.expanded));
        // Toggling it is not a change worth a commit or a write.
        assert_eq!(
            serde_json::to_value(for_repo(&merged)).unwrap(),
            serde_json::to_value(for_repo(&base)).unwrap()
        );
    }

    #[test]
    fn unknown_keys_merge_per_key_and_survive() {
        let raw = |v: Value| -> Store { serde_json::from_value(v).unwrap() };
        let base = raw(
            json!({"version": 1, "todos": [{"id": 1, "title": "a", "x": 1, "y": 1}], "top": 1}),
        );
        let local = raw(
            json!({"version": 1, "todos": [{"id": 1, "title": "a", "x": 2, "y": 1}], "top": 1}),
        );
        let remote = raw(
            json!({"version": 3, "todos": [{"id": 1, "title": "a", "x": 1, "y": 5, "z": true}], "top": 2, "new": "k"}),
        );
        let merged = merge_store(&base, &local, &remote);
        let out = serde_json::to_value(&merged).unwrap();
        assert_eq!(out["version"], 3);
        assert_eq!(out["top"], 2);
        assert_eq!(out["new"], "k");
        assert_eq!(out["todos"][0]["x"], 2);
        assert_eq!(out["todos"][0]["y"], 5);
        assert_eq!(out["todos"][0]["z"], true);
    }

    fn flag(id: u64, name: &str) -> CustomFlag {
        CustomFlag {
            id,
            name: name.into(),
            color: [1, 2, 3],
            extra: Map::new(),
        }
    }

    #[test]
    fn flags_merge_and_dangling_assignments_are_dropped() {
        let mut t = todo(1, "a");
        t.flag_ids = vec![10, 20];
        let base = Store {
            todos: vec![t.clone()],
            flags: vec![flag(10, "Work"), flag(20, "Home")],
            ..Default::default()
        };
        // Remote deleted Home; local added Errands and renamed Work.
        let mut local = base.clone();
        local.flags = vec![flag(10, "Office"), flag(20, "Home"), flag(30, "Errands")];
        local.todos[0].flag_ids = vec![10, 20, 30];
        let mut remote = base.clone();
        remote.flags = vec![flag(10, "Work")];
        remote.todos[0].flag_ids = vec![10];
        let merged = merge_store(&base, &local, &remote);
        let names: Vec<_> = merged.flags.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["Office", "Errands"]);
        assert_eq!(merged.todos[0].flag_ids, vec![10, 30]);
    }

    #[test]
    fn two_new_flags_with_one_name_are_both_kept_and_told_apart() {
        let local = Store {
            flags: vec![flag(1, "Work")],
            ..Default::default()
        };
        let remote = Store {
            flags: vec![flag(2, "work"), flag(3, "Work (2)")],
            ..Default::default()
        };
        let merged = merge_store(&Store::default(), &local, &remote);
        let names: Vec<_> = merged
            .flags
            .iter()
            .map(|f| (f.id, f.name.as_str()))
            .collect();
        assert!(names.contains(&(1, "Work")), "{names:?}");
        assert!(names.contains(&(2, "work (3)")), "{names:?}");
        assert!(names.contains(&(3, "Work (2)")), "{names:?}");
    }

    #[test]
    fn changes_count_what_arrived() {
        let before = store(&[todo(1, "a"), todo(2, "b")]);
        let after = store(&[todo(1, "a2"), todo(3, "c")]);
        assert_eq!(
            todo_changes(&before, &after),
            Changes {
                added: 1,
                removed: 1,
                updated: 1
            }
        );
    }

    fn event(id: u64, title: &str, start: &str, end: &str) -> Value {
        json!({"id": id, "title": title, "date": "2026-11-20", "start": start,
               "end": end, "kind": "meeting", "location": ""})
    }

    fn agenda(events: Vec<Value>) -> Value {
        json!({"version": 1, "events": events})
    }

    #[test]
    fn calendar_events_merge_like_tasks() {
        let base = agenda(vec![
            event(1, "a", "09:00:00", "10:00:00"),
            event(2, "b", "09:00:00", "10:00:00"),
        ]);
        let local = agenda(vec![
            event(1, "a moved", "09:00:00", "10:00:00"),
            event(2, "b", "09:00:00", "10:00:00"),
        ]);
        let remote = agenda(vec![event(1, "a", "09:00:00", "10:00:00")]);
        let merged = merge_agenda(&base, &local, &remote);
        let titles: Vec<_> = events(&merged)
            .iter()
            .map(|e| e["title"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(titles, vec!["a moved"]);
        assert_eq!(merged["version"], 1);
    }

    #[test]
    fn colliding_new_event_ids_keep_both_events() {
        let base = agenda(vec![]);
        let local = agenda(vec![event(3, "dentist", "09:00:00", "10:00:00")]);
        let remote = agenda(vec![event(3, "standup", "11:00:00", "11:15:00")]);
        let merged = merge_agenda(&base, &local, &remote);
        let list = events(&merged);
        assert_eq!(list.len(), 2);
        let standup = list.iter().find(|e| e["title"] == "standup").unwrap();
        let dentist = list.iter().find(|e| e["title"] == "dentist").unwrap();
        assert_eq!(standup["id"], 3, "the remote one keeps its id");
        assert_ne!(dentist["id"], 3, "the local one gets a fresh id");
        assert!(crate::calendar::parse_agenda(&merged.to_string()).is_some());

        // The same event on both sides is not a collision.
        let merged = merge_agenda(&base, &local, &local);
        assert_eq!(events(&merged).len(), 1);
    }

    #[test]
    fn an_event_whose_merged_times_make_no_sense_takes_one_side_whole() {
        let base = agenda(vec![event(1, "a", "09:00:00", "10:00:00")]);
        let local = agenda(vec![event(1, "a", "09:45:00", "10:00:00")]);
        let remote = agenda(vec![event(1, "a", "09:00:00", "09:30:00")]);
        let merged = merge_agenda(&base, &local, &remote);
        let e = &events(&merged)[0];
        assert_eq!(e["start"], "09:45:00");
        assert_eq!(e["end"], "10:00:00");
    }
}
