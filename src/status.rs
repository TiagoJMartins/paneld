//! The per-device record behind `GET /api/status`.
//!
//! Nothing here is persisted. Every field describes *this process's* lifetime:
//! when a device last polled, what it last reported, which frame it is being
//! served, and how many renders have happened since start. Reloading a saved
//! `render_count` would make the counter meaningless.
//!
//! `render_count` is not decoration. It is the observable that makes the render
//! loop testable from outside: coalescing, startup rendering and the
//! hash-unchanged path are all asserted through it, so a render that produced an
//! unchanged hash still increments it — it did perform a render. Together with
//! `last_render_at` it is also the only way an operator can see a render loop
//! that has died or wedged while the HTTP listener keeps serving the last frame
//! it produced, which is the one failure mode a poll-driven design cannot have.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::Serialize;
use time::OffsetDateTime;

use crate::telemetry::Telemetry;

/// Everything known about one device, as served by the status endpoint.
///
/// `Default` is the state of a device that has neither polled nor rendered: all
/// timestamps `None`, telemetry empty, count zero.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DeviceStatus {
    /// When this device last completed a display poll.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_poll_at: Option<OffsetDateTime>,
    /// The merged reading across every poll seen, not just the most recent one.
    pub telemetry: Telemetry,
    /// Content hash of the frame currently being served to this device.
    pub frame_hash: Option<String>,
    /// When the most recent render for this device completed.
    #[serde(with = "time::serde::rfc3339::option")]
    pub last_render_at: Option<OffsetDateTime>,
    /// Monotonic count of renders performed for this device since process start.
    pub render_count: u64,
}

/// The status store: device id to [`DeviceStatus`], in memory only.
///
/// Interior mutability so that `&self` methods work from `axum` handlers holding
/// a shared reference.
#[derive(Debug, Default)]
pub struct StatusStore {
    devices: Mutex<BTreeMap<String, DeviceStatus>>,
}

impl StatusStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a completed display poll, folding `telemetry` into what is known.
    ///
    /// The merge is the point: a header-light poll must not blank fields learned
    /// from an earlier, richer one. A poll never renders, so nothing here touches
    /// `render_count`, `frame_hash` or `last_render_at`.
    pub fn record_poll(&self, device: &str, telemetry: Telemetry, at: OffsetDateTime) {
        let mut devices = self.lock();
        let status = devices.entry(device.to_owned()).or_default();
        status.last_poll_at = Some(at);
        status.telemetry.merge_from(telemetry);
    }

    /// Records a completed render, whether or not it changed the frame.
    ///
    /// Called on every render attempt that finished, including one whose bytes
    /// hashed to the value already stored — that is what makes the
    /// hash-unchanged path visible from outside the render loop.
    pub fn record_render(&self, device: &str, frame_hash: &str, at: OffsetDateTime) {
        let mut devices = self.lock();
        let status = devices.entry(device.to_owned()).or_default();
        status.frame_hash = Some(frame_hash.to_owned());
        status.last_render_at = Some(at);
        status.render_count += 1;
    }

    /// Every device's status, ordered by device id.
    ///
    /// A `BTreeMap` rather than a `HashMap` so the JSON object's keys come out
    /// stable: an operator diffing two `/api/status` responses should see only
    /// real changes, never spurious reordering.
    pub fn snapshot(&self) -> BTreeMap<String, DeviceStatus> {
        self.lock().clone()
    }

    /// What `device` last reported about itself, merged across every poll.
    ///
    /// A copy rather than a borrow: the renderer needs the values, not the lock,
    /// and a status bar that held this mutex while rasterising a frame would stall
    /// every display poll behind it.
    pub fn telemetry(&self, device: &str) -> Telemetry {
        self.lock()
            .get(device)
            .map(|status| status.telemetry.clone())
            .unwrap_or_default()
    }

    /// A panicking handler must not wedge status reporting for the rest of the
    /// process: the map is structurally intact either way, so recover the guard.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, DeviceStatus>> {
        self.devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed stamp, so nothing depends on wall-clock time.
    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
    }

    /// A rich reading, as a well-behaved poll would report.
    fn full() -> Telemetry {
        Telemetry {
            battery_percent: Some(88.0),
            battery_millivolts: Some(4_010.0),
            rssi: Some(-52),
            firmware_version: Some("1.2.3".to_owned()),
            width: Some(1404),
            height: Some(1872),
            mac: Some("AA:BB:CC:DD:EE:FF".to_owned()),
            model: Some("kobo".to_owned()),
        }
    }

    #[test]
    fn a_render_increments_the_count_and_moves_the_stamp() {
        let store = StatusStore::new();
        store.record_render("panel", "aaaa", at(0));
        store.record_render("panel", "bbbb", at(10));

        let status = store.snapshot().remove("panel").unwrap();
        assert_eq!(status.render_count, 2);
        assert_eq!(status.frame_hash.as_deref(), Some("bbbb"));
        assert_eq!(status.last_render_at, Some(at(10)));
    }

    #[test]
    fn recording_the_same_hash_twice_still_counts_as_two_renders() {
        let store = StatusStore::new();
        store.record_render("panel", "aaaa", at(0));
        store.record_render("panel", "aaaa", at(30));

        let status = store.snapshot().remove("panel").unwrap();
        assert_eq!(
            status.render_count, 2,
            "a render with unchanged output still performed a render"
        );
        assert_eq!(
            status.last_render_at,
            Some(at(30)),
            "an unchanged hash must not freeze the liveness stamp"
        );
    }

    #[test]
    fn a_poll_creates_the_device_without_pretending_it_rendered() {
        let store = StatusStore::new();
        store.record_poll("panel", full(), at(0));

        let status = store.snapshot().remove("panel").unwrap();
        assert_eq!(status.last_poll_at, Some(at(0)));
        assert_eq!(status.render_count, 0);
        assert_eq!(status.frame_hash, None);
        assert_eq!(status.last_render_at, None);
    }

    #[test]
    fn a_render_creates_a_device_that_has_never_polled() {
        let store = StatusStore::new();
        store.record_render("panel", "aaaa", at(0));

        let status = store.snapshot().remove("panel").unwrap();
        assert_eq!(status.last_poll_at, None);
        assert_eq!(status.render_count, 1);
        assert_eq!(status.telemetry, Telemetry::default());
    }

    #[test]
    fn a_sparser_second_poll_preserves_what_the_first_taught_us() {
        let store = StatusStore::new();
        store.record_poll("panel", full(), at(0));

        let sparse = Telemetry {
            battery_percent: Some(71.0),
            ..Telemetry::default()
        };
        store.record_poll("panel", sparse, at(60));

        let status = store.snapshot().remove("panel").unwrap();
        assert_eq!(status.last_poll_at, Some(at(60)));
        assert_eq!(
            status.telemetry.battery_percent,
            Some(71.0),
            "the fresh reading wins"
        );
        assert_eq!(status.telemetry.rssi, Some(-52), "and the rest survives");
        assert_eq!(status.telemetry.mac.as_deref(), Some("AA:BB:CC:DD:EE:FF"));
        assert_eq!(status.telemetry.width, Some(1404));
        assert_eq!(status.telemetry.firmware_version.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn no_number_of_polls_changes_the_render_count() {
        let store = StatusStore::new();
        store.record_render("panel", "aaaa", at(0));

        for second in 1..=5 {
            store.record_poll("panel", full(), at(second));
        }

        let status = store.snapshot().remove("panel").unwrap();
        assert_eq!(status.render_count, 1);
        assert_eq!(status.frame_hash.as_deref(), Some("aaaa"));
        assert_eq!(
            status.last_render_at,
            Some(at(0)),
            "polling is not rendering"
        );
    }

    #[test]
    fn devices_do_not_share_state() {
        let store = StatusStore::new();
        store.record_render("kitchen", "aaaa", at(0));
        store.record_render("kitchen", "bbbb", at(1));
        store.record_poll("hallway", full(), at(2));

        let snapshot = store.snapshot();
        assert_eq!(snapshot["kitchen"].render_count, 2);
        assert_eq!(snapshot["hallway"].render_count, 0);
        assert_eq!(snapshot["kitchen"].last_poll_at, None);
        assert_eq!(snapshot["hallway"].last_poll_at, Some(at(2)));
    }

    #[test]
    fn the_snapshot_is_ordered_by_device_id_however_devices_arrived() {
        let store = StatusStore::new();
        for device in ["study", "hallway", "kitchen", "attic"] {
            store.record_poll(device, Telemetry::default(), at(0));
        }

        let keys: Vec<String> = store.snapshot().into_keys().collect();
        assert_eq!(keys, ["attic", "hallway", "kitchen", "study"]);
    }

    #[test]
    fn absent_timestamps_serialise_as_null() {
        let store = StatusStore::new();
        store.record_poll("panel", Telemetry::default(), at(0));

        let json = serde_json::to_value(store.snapshot()).unwrap();
        let panel = &json["panel"];
        assert!(panel["last_render_at"].is_null());
        assert_eq!(panel["frame_hash"], serde_json::Value::Null);
        assert_eq!(panel["render_count"], 0);
        assert!(
            panel["telemetry"].is_object(),
            "telemetry nests as an object"
        );
    }

    #[test]
    fn present_timestamps_serialise_as_rfc_3339_strings() {
        let store = StatusStore::new();
        store.record_poll("panel", full(), at(0));
        store.record_render("panel", "deadbeef", at(5));

        let json = serde_json::to_value(store.snapshot()).unwrap();
        let panel = &json["panel"];
        assert_eq!(panel["last_poll_at"], "2023-11-14T22:13:20Z");
        assert_eq!(panel["last_render_at"], "2023-11-14T22:13:25Z");
        assert_eq!(panel["frame_hash"], "deadbeef");
        assert_eq!(panel["render_count"], 1);
        assert_eq!(panel["telemetry"]["battery_percent"], 88.0);
    }
}
