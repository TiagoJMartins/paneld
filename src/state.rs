//! Everything the process remembers across a restart, in one file.
//!
//! Three unrelated things outlive paneld: what publishers pushed
//! ([`crate::content`]), what panels reported about their own batteries
//! ([`crate::battery`]), and which way each numeric reading last moved
//! ([`Trends`]). They are held together here, behind one mutex and one path,
//! rather than in a file each.
//!
//! One file rather than three, because the count would only ever grow. Each new
//! thing worth remembering would otherwise arrive with its own path key to
//! document, its own load-and-warn, its own atomic write and its own chance to
//! be forgotten in a backup — for data that all together is a few kilobytes of
//! JSON written by one process. The cost is that clearing one domain means
//! editing the file rather than deleting it, which is a thing an operator does
//! approximately never.
//!
//! One mutex rather than three, because nothing here contends. A poll folds a
//! battery reading, a push stores a record, a render takes a content snapshot
//! and steps the trends: all are short, none holds the lock across I/O, and a
//! device polls at most every thirty seconds. Splitting the lock would buy
//! nothing measurable and cost the guarantee that a persist writes one
//! consistent view of all three.
//!
//! **Nothing is migrated.** A build before this one wrote two separate files,
//! and this one does not read them: the pushed values are re-pushed within a
//! poll interval and the battery history rebuilds itself, so carrying migration
//! code for one upgrade would outlive its usefulness by years.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::battery::{Histories, Power, Report};
use crate::content::{ContentBody, ContentRecord, PutError, Records};
use crate::jsonfile;

/// Distinct readings whose last-shown value is remembered.
///
/// A bound rather than a policy. The keys are derived from configuration, so a
/// single run cannot exceed the widgets an author actually wrote — but the file
/// outlives configurations, and a dashboard rewritten every few months would
/// otherwise accumulate the keys of every reading it ever had.
pub const MAX_TRENDS: usize = 1_024;

/// The key one reading's remembered value is stored under.
///
/// Written once, here, because two callers must agree on it exactly: the render
/// loop steps a key before the frame is built and the resolve path looks the same
/// key up while building it. A second spelling of this is a cell whose arrow is
/// permanently steady.
///
/// The device id leads, because a widget id is unique only within its device, and
/// two panels showing the same sensor at different precisions round to different
/// numbers.
pub fn trend_key(device_id: &str, widget_id: &str, reading: Option<usize>) -> String {
    match reading {
        Some(index) => format!("{device_id}/{widget_id}#{index}"),
        None => format!("{device_id}/{widget_id}"),
    }
}

/// Which way a reading last moved.
///
/// Persisted, because that is the whole point: the arrow describes the change
/// from the previous *frame*, and a process that forgot the previous frame would
/// draw [`Trend::Steady`] on every reading after every restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Trend {
    /// The displayed value has not moved since it last did — and, for a reading
    /// nothing was ever shown for, that it has never moved at all.
    #[default]
    Steady,
    /// The last change was upwards.
    Up,
    /// The last change was downwards.
    Down,
}

/// One reading's last displayed value and the direction that put it there.
///
/// The direction is stored rather than recomputed because it is *sticky*: it
/// describes the last change, not the last render. Recomputing it from the
/// current value alone would make an unchanged reading read as steady on the
/// very next frame, and a frame that changed for that reason is a repaint the
/// owner pays for to be told nothing happened.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
struct Mark {
    /// The value as it was last *shown*, already rounded to the reading's
    /// precision — never the raw reading.
    shown: f64,
    trend: Trend,
}

/// Each reading's last displayed value, keyed by whatever the render path calls
/// it.
///
/// A `BTreeMap` so the persisted object's keys come out in a stable order: a
/// file that reordered itself on every write would be undiffable.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct Trends(BTreeMap<String, Mark>);

impl Trends {
    /// Folds one frame's displayed value in and answers which way to draw the
    /// arrow.
    ///
    /// `shown` is the value *as the cell will print it*, rounded already. That is
    /// what makes this safe for the panel: the arrow can only change on a frame
    /// where the number changed too, so it never provokes a repaint of its own.
    /// Comparing raw readings instead would need a deadband picked out of the air,
    /// and would flip the arrow under a frame that looked identical.
    ///
    /// An unchanged value keeps the direction that produced it, which is what
    /// makes an arrow mean "last moved up" rather than "moved up between these two
    /// renders".
    pub fn step(&mut self, key: &str, shown: f64) -> Trend {
        if let Some(mark) = self.0.get_mut(key) {
            if shown != mark.shown {
                mark.trend = if shown > mark.shown {
                    Trend::Up
                } else {
                    Trend::Down
                };
                mark.shown = shown;
            }
            return mark.trend;
        }

        // Wholesale eviction rather than an LRU, as the placeholder cache does:
        // the cap exists to absorb configurations that came and went, and the only
        // cost of clearing is that every arrow reads steady until its reading next
        // moves. Tracking recency to avoid that would be machinery for a case an
        // operator hits once a year.
        if self.0.len() >= MAX_TRENDS {
            self.0.clear();
        }
        self.0.insert(
            key.to_owned(),
            Mark {
                shown,
                trend: Trend::Steady,
            },
        );
        Trend::Steady
    }

    /// How many readings are remembered. For the bound's own test.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether nothing is remembered yet.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The whole persisted state, as it is written to disk.
///
/// Every field defaults, so a file written by an older build — or one an
/// operator trimmed by hand — loads with the domains it does carry and starts the
/// rest empty.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(default)]
struct State {
    content: Records,
    battery: Histories,
    trend: Trends,
}

/// The state store: everything remembered, plus the file it is persisted to.
///
/// Interior mutability so that `&self` methods work from `axum` handlers holding
/// a shared reference.
#[derive(Debug)]
pub struct StateStore {
    path: PathBuf,
    state: Mutex<State>,
}

impl StateStore {
    /// Opens the state persisted at `path`.
    ///
    /// Never fails; see [`crate::jsonfile`]. A missing file starts empty silently
    /// and a corrupt one logs a warning and starts empty, because refusing to boot
    /// over a damaged cache of last-seen values would leave the panel showing
    /// nothing at all. Whatever battery history is loaded is capped, so a file
    /// that grew under a different build cannot grow this process's memory.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut state: State = jsonfile::read(&path, "state");
        state.battery.truncate();
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    /// Stores content for `widget_id`, replacing any previous record wholesale.
    pub fn put_content(
        &self,
        widget_id: &str,
        body: ContentBody,
        now: OffsetDateTime,
    ) -> std::result::Result<ContentRecord, PutError> {
        self.lock().content.put(widget_id, body, now)
    }

    /// The record stored for `widget_id`, if any.
    pub fn content(&self, widget_id: &str) -> Option<ContentRecord> {
        self.lock().content.get(widget_id).cloned()
    }

    /// Every stored record, for rendering a whole dashboard from one consistent
    /// view of the store.
    ///
    /// Cloned rather than handed out behind the guard: the render that reads this
    /// is the most expensive thing in the process, and holding the state lock
    /// across it would block every push and every poll for the duration.
    pub fn content_snapshot(&self) -> HashMap<String, ContentRecord> {
        self.lock().content.all().clone()
    }

    /// Folds one poll's reading into `device`'s battery history.
    pub fn record_battery(&self, device: &str, percent: f64, power: Power, at: OffsetDateTime) {
        self.lock().battery.record(device, percent, power, at);
    }

    /// Each device's battery history and what it implies, ordered by device id.
    pub fn battery_reports(&self) -> BTreeMap<String, Report> {
        self.lock().battery.reports()
    }

    /// Folds one frame's displayed value in and answers which way to draw
    /// `key`'s arrow. See [`Trends::step`].
    pub fn trend(&self, key: &str, shown: f64) -> Trend {
        self.lock().trend.step(key, shown)
    }

    /// Writes the whole state to its configured path, atomically.
    ///
    /// One write for all three domains, which is the point of the merge: a poll
    /// that folded a battery reading also flushes the trend the render before it
    /// recorded, and there is no ordering between two files to get wrong.
    pub fn persist(&self) -> Result<()> {
        let state = self.lock();
        jsonfile::write(&self.path, &*state, "state")
    }

    /// A panicking handler must not wedge the state for the rest of the process:
    /// the maps are structurally intact either way, so recover the guard.
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battery::MAX_READINGS;
    use serde_json::Value;
    use time::Duration;

    /// A state file of its own per test, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "paneld-state-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("state.json")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fixed instant plus `minutes`, so a test reads as a sequence of polls.
    fn at(minutes: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap() + Duration::minutes(minutes)
    }

    /// A panel reporting nothing about its charger, which is every Kindle.
    const SILENT: Power = Power {
        charging: None,
        usb_connected: None,
    };

    /// A panel on the charger.
    const PLUGGED: Power = Power {
        charging: Some(true),
        usb_connected: Some(true),
    };

    fn body(json: &str) -> ContentBody {
        serde_json::from_str(json).expect("test body should parse")
    }

    #[test]
    fn a_missing_file_starts_empty() {
        let dir = Dir::new("missing");
        let store = StateStore::load(dir.file());
        assert!(store.content_snapshot().is_empty());
        assert!(store.battery_reports().is_empty());
        assert!(!dir.file().exists(), "load does not create the file");
    }

    #[test]
    fn one_persist_round_trips_all_three_domains() {
        let dir = Dir::new("roundtrip");
        let store = StateStore::load(dir.file());
        store
            .put_content("sensor", body(r#"{"value":"on","unit":"°C"}"#), at(0))
            .unwrap();
        for (percent, minute) in [(80.0, 0), (80.0, 30), (79.0, 60), (78.0, 120)] {
            store.record_battery("kindle", percent, PLUGGED, at(minute));
        }
        assert_eq!(store.trend("kitchen/temp", 21.0), Trend::Steady);
        assert_eq!(store.trend("kitchen/temp", 22.0), Trend::Up);
        store.persist().unwrap();

        // One file, so one load brings back what three used to.
        let reloaded = StateStore::load(dir.file());
        assert_eq!(reloaded.content("sensor").unwrap().value, Value::from("on"));

        let kindle = &reloaded.battery_reports()["kindle"];
        assert_eq!(kindle.percent, Some(78.0));
        assert_eq!(kindle.readings.len(), 3);
        assert_eq!(kindle.readings[0].polls, 2, "the run length survives");
        assert_eq!(kindle.readings[0].until, at(30));
        assert_eq!(kindle.power, PLUGGED);
        assert_eq!(
            kindle.trend.percent_per_hour,
            Some(-1.0),
            "the rate is measured from reloaded samples: {:?}",
            kindle.trend
        );

        assert_eq!(
            reloaded.trend("kitchen/temp", 22.0),
            Trend::Up,
            "an unchanged reading keeps the direction it was persisted with"
        );
    }

    #[test]
    fn the_persisted_file_names_its_three_domains() {
        let dir = Dir::new("shape");
        let store = StateStore::load(dir.file());
        store
            .put_content("sensor", body(r#"{"value":1}"#), at(0))
            .unwrap();
        store.record_battery("kindle", 50.0, SILENT, at(0));
        store.trend("kindle/temp", 21.0);
        store.persist().unwrap();

        let json: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.file()).unwrap()).unwrap();
        assert_eq!(json["content"]["sensor"]["value"], 1);
        assert_eq!(json["battery"]["kindle"][0]["percent"], 50.0);
        assert_eq!(json["trend"]["kindle/temp"]["shown"], 21.0);
        assert_eq!(json["trend"]["kindle/temp"]["trend"], "steady");
    }

    #[test]
    fn devices_keep_separate_histories_across_a_reload() {
        let dir = Dir::new("devices");
        let store = StateStore::load(dir.file());
        store.record_battery("kitchen", 90.0, SILENT, at(0));
        store.record_battery("hallway", 20.0, PLUGGED, at(0));
        store.persist().unwrap();

        let reloaded = StateStore::load(dir.file()).battery_reports();
        assert_eq!(reloaded["kitchen"].percent, Some(90.0));
        assert_eq!(reloaded["hallway"].percent, Some(20.0));
        assert_eq!(reloaded["hallway"].power, PLUGGED);
    }

    #[test]
    fn a_file_holding_more_readings_than_the_cap_is_trimmed_on_load() {
        // The cap is a memory bound, so it has to hold against a file written by
        // a build with a larger one, or hand-edited.
        let dir = Dir::new("cap");
        let padded = format!(
            r#"{{"battery":{{"kindle":[{}]}}}}"#,
            (0..MAX_READINGS + 20)
                .map(|step| format!(
                    r#"{{"percent":{},"power":{{}},"since":"2023-01-01T00:00:00Z","until":"2023-01-01T00:00:00Z","polls":1}}"#,
                    1000 - step as i64
                ))
                .collect::<Vec<_>>()
                .join(",")
        );
        std::fs::write(dir.file(), padded).unwrap();

        let reloaded = StateStore::load(dir.file()).battery_reports();
        assert_eq!(reloaded["kindle"].readings.len(), MAX_READINGS);
        assert_eq!(
            reloaded["kindle"].readings[0].percent,
            (1000 - 20) as f64,
            "the oldest readings are the ones dropped"
        );
    }

    #[test]
    fn a_corrupt_file_yields_an_empty_state_and_is_replaced_on_persist() {
        let dir = Dir::new("corrupt");
        std::fs::write(dir.file(), "{not json at all").unwrap();

        let store = StateStore::load(dir.file());
        assert!(
            store.content_snapshot().is_empty(),
            "damaged state must not stop the boot"
        );

        store
            .put_content("sensor", body(r#"{"value":1}"#), at(0))
            .unwrap();
        store.persist().unwrap();
        assert_eq!(
            StateStore::load(dir.file()).content_snapshot().len(),
            1,
            "the store is usable afterwards, replacing the bad file"
        );
    }

    #[test]
    fn a_file_of_the_wrong_shape_yields_an_empty_state() {
        let dir = Dir::new("wrong-shape");
        std::fs::write(dir.file(), r#"["not","an","object"]"#).unwrap();
        assert!(StateStore::load(dir.file()).content_snapshot().is_empty());
    }

    #[test]
    fn persist_leaves_no_temporary_file_behind() {
        let dir = Dir::new("temp");
        let store = StateStore::load(dir.file());
        store
            .put_content("sensor", body(r#"{"value":1}"#), at(0))
            .unwrap();
        store.persist().unwrap();
        store.persist().unwrap();

        let entries: Vec<String> = std::fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, ["state.json"]);
    }

    #[test]
    fn an_unwritable_path_is_an_error_and_not_a_panic() {
        // The poll path logs this and carries on; it must never unwind into a
        // handler.
        let store = StateStore::load("/proc/paneld-cannot-write/state.json");
        store.record_battery("kindle", 50.0, SILENT, at(0));
        assert!(store.persist().is_err());
        assert_eq!(store.battery_reports()["kindle"].percent, Some(50.0));
    }

    #[test]
    fn a_first_sighting_is_steady_and_a_change_names_its_direction() {
        let mut trends = Trends::default();
        for (shown, want) in [
            (21.0, Trend::Steady),
            (22.0, Trend::Up),
            (21.0, Trend::Down),
            (19.0, Trend::Down),
            (20.0, Trend::Up),
        ] {
            assert_eq!(trends.step("k", shown), want, "stepping to {shown}");
        }
    }

    #[test]
    fn an_unchanged_value_holds_its_arrow_still() {
        // The property the whole feature rests on: between two changes the arrow
        // is a constant, so the frame hash is too and the panel does not repaint.
        let mut trends = Trends::default();
        trends.step("k", 21.0);
        assert_eq!(trends.step("k", 22.0), Trend::Up);
        for _ in 0..5 {
            assert_eq!(
                trends.step("k", 22.0),
                Trend::Up,
                "a repeated value must not decay to steady"
            );
        }
        assert_eq!(trends.step("k", 22.0), Trend::Up);
    }

    #[test]
    fn readings_are_tracked_independently() {
        let mut trends = Trends::default();
        trends.step("a", 10.0);
        trends.step("b", 10.0);
        assert_eq!(trends.step("a", 11.0), Trend::Up);
        assert_eq!(trends.step("b", 9.0), Trend::Down);
        assert_eq!(trends.step("a", 11.0), Trend::Up, "b did not disturb a");
    }

    #[test]
    fn the_trend_cap_is_a_bound_and_clearing_it_costs_only_the_arrows() {
        let mut trends = Trends::default();
        for index in 0..MAX_TRENDS {
            trends.step(&format!("k{index}"), 1.0);
        }
        assert_eq!(trends.len(), MAX_TRENDS);

        // A key past the cap clears what came before rather than growing the file.
        assert_eq!(trends.step("one-too-many", 1.0), Trend::Steady);
        assert_eq!(trends.len(), 1);
        assert_eq!(
            trends.step("k0", 2.0),
            Trend::Steady,
            "a forgotten reading is a first sighting again, not a false arrow"
        );
    }
}
