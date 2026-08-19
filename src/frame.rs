//! The frame currently served for each device, plus exactly one generation back.
//!
//! This module is where the e-ink refresh story lives. `filename` is the
//! device's cache key: if it is unchanged the device does not download the frame
//! at all, it repaints from its own flash. So the whole of
//! [`FrameStore::offer`] exists to answer one question — did the *encoded bytes*
//! change? — and to leave the served record byte-for-byte untouched when they
//! did not.
//!
//! Two consequences worth stating, because both are easy to break:
//!
//! - An unchanged offer does not refresh [`Frame::rendered_at`]. That stamp
//!   describes the frame being served, not the last time a render ran; the
//!   status endpoint's `last_render_at` is a separate observable owned by
//!   [`crate::status::StatusStore`], and a render that produced identical bytes
//!   still moves it.
//! - Exactly one generation back is retained, and it stays fetchable at its own
//!   URL, because a device may be mid-download of the frame it just replaced.
//!   Retaining more would only grow memory: nothing older can still be in
//!   flight, since a device holds one frame request at a time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use time::OffsetDateTime;

use crate::render::frame_hash;

/// One rendered frame, ready to serve.
///
/// `bytes` is an [`Arc<[u8]>`] so that handing a frame to an HTTP handler is a
/// refcount bump rather than a copy of the whole PNG.
#[derive(Debug, Clone)]
pub struct Frame {
    /// The encoded PNG, exactly as it goes on the wire.
    pub bytes: Arc<[u8]>,
    /// Filename stem: [`crate::render::frame_hash`] of `bytes`.
    pub hash: String,
    /// When the render that produced these bytes completed.
    pub rendered_at: OffsetDateTime,
}

impl Frame {
    /// Builds a frame from bytes whose hash has already been computed.
    ///
    /// Private, and the hash is passed in rather than recomputed: `offer` needs
    /// it before it can decide whether to build a frame at all, and hashing an
    /// 80 kB PNG twice per tick buys nothing.
    fn new(bytes: Vec<u8>, hash: String, rendered_at: OffsetDateTime) -> Self {
        Self {
            bytes: bytes.into(),
            hash,
            rendered_at,
        }
    }
}

/// What is retained for one device: the frame being served, and the one it
/// replaced.
#[derive(Debug)]
struct Generations {
    current: Frame,
    previous: Option<Frame>,
}

/// The frames served for every device.
///
/// Interior mutability so that `&self` methods work from `axum` handlers holding
/// a shared reference, and so the render task can promote a frame without any
/// handler having to cooperate.
#[derive(Debug, Default)]
pub struct FrameStore {
    devices: Mutex<HashMap<String, Generations>>,
}

impl FrameStore {
    /// An empty store: no device has a frame yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The frame currently served for `device`, or `None` before its first
    /// render.
    pub fn current(&self, device: &str) -> Option<Frame> {
        self.lock().get(device).map(|slot| slot.current.clone())
    }

    /// The frame with this hash, if it is either current or the one retained
    /// generation back.
    ///
    /// A hash that has aged out is indistinguishable from one that never
    /// existed, and both are `None`: the device's move in either case is to poll
    /// again and take the filename it is given.
    pub fn by_hash(&self, device: &str, hash: &str) -> Option<Frame> {
        let devices = self.lock();
        let slot = devices.get(device)?;
        if slot.current.hash == hash {
            return Some(slot.current.clone());
        }
        slot.previous
            .as_ref()
            .filter(|frame| frame.hash == hash)
            .cloned()
    }

    /// Offers freshly encoded bytes for `device`.
    ///
    /// Returns `true` when the hash changed and the frame was promoted, `false`
    /// when it did not. On `false` the offered bytes are dropped and the stored
    /// record — bytes, hash and `rendered_at` alike — is left exactly as it was,
    /// which is what makes the device's next poll see an unchanged `filename`
    /// and skip both the download and the repaint.
    pub fn offer(&self, device: &str, bytes: Vec<u8>, rendered_at: OffsetDateTime) -> bool {
        let hash = frame_hash(&bytes);
        let mut devices = self.lock();

        if let Some(slot) = devices.get_mut(device) {
            if slot.current.hash == hash {
                return false;
            }
            let promoted = Frame::new(bytes, hash, rendered_at);
            // The frame this replaces becomes the retained generation; whatever
            // was retained before it is dropped here.
            slot.previous = Some(std::mem::replace(&mut slot.current, promoted));
            return true;
        }

        devices.insert(
            device.to_owned(),
            Generations {
                current: Frame::new(bytes, hash, rendered_at),
                previous: None,
            },
        );
        true
    }

    /// A panicking handler must not wedge frame serving for the rest of the
    /// process: the map is structurally intact either way, so recover the guard.
    fn lock(&self) -> MutexGuard<'_, HashMap<String, Generations>> {
        self.devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed stamp, so nothing here depends on wall-clock time.
    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
    }

    /// Distinct payloads that genuinely hash differently, checked below.
    const RED: &[u8] = b"\x89PNG frame one";
    const GREEN: &[u8] = b"\x89PNG frame two";
    const BLUE: &[u8] = b"\x89PNG frame three";

    #[test]
    fn payload_fixtures_hash_differently() {
        assert_ne!(frame_hash(RED), frame_hash(GREEN));
        assert_ne!(frame_hash(GREEN), frame_hash(BLUE));
        assert_ne!(frame_hash(RED), frame_hash(BLUE));
    }

    #[test]
    fn first_offer_becomes_current() {
        let store = FrameStore::new();
        assert!(store.offer("kindle", RED.to_vec(), at(0)));

        let frame = store.current("kindle").expect("frame after first offer");
        assert_eq!(&*frame.bytes, RED);
        assert_eq!(frame.rendered_at, at(0));
        assert!(store.by_hash("kindle", &frame.hash).is_some());
    }

    #[test]
    fn frame_hash_matches_its_bytes() {
        let store = FrameStore::new();
        store.offer("kindle", GREEN.to_vec(), at(0));

        let frame = store.current("kindle").unwrap();
        assert_eq!(frame.hash, frame_hash(&frame.bytes));
    }

    /// The highest-value behaviour in the module: identical bytes must not
    /// disturb the served record, or the panel repaints for nothing.
    #[test]
    fn identical_bytes_leave_the_record_untouched() {
        let store = FrameStore::new();
        store.offer("kindle", RED.to_vec(), at(0));
        let before = store.current("kindle").unwrap();

        assert!(!store.offer("kindle", RED.to_vec(), at(600)));

        let after = store.current("kindle").unwrap();
        assert_eq!(after.hash, before.hash);
        assert_eq!(
            after.rendered_at,
            at(0),
            "rendered_at describes the served frame, not the last render"
        );
        assert_eq!(&*after.bytes, RED);
        assert!(
            store.by_hash("kindle", &before.hash).is_some(),
            "an unchanged offer must not make the served frame unreachable"
        );
    }

    #[test]
    fn changed_bytes_promote_and_retain_the_previous_generation() {
        let store = FrameStore::new();
        store.offer("kindle", RED.to_vec(), at(0));
        let first = store.current("kindle").unwrap().hash;

        assert!(store.offer("kindle", GREEN.to_vec(), at(300)));

        let current = store.current("kindle").unwrap();
        assert_ne!(current.hash, first);
        assert_eq!(&*current.bytes, GREEN);
        assert_eq!(current.rendered_at, at(300));

        let retained = store
            .by_hash("kindle", &first)
            .expect("the replaced frame stays fetchable mid-download");
        assert_eq!(&*retained.bytes, RED);
        assert_eq!(retained.rendered_at, at(0));
    }

    #[test]
    fn only_one_generation_is_retained() {
        let store = FrameStore::new();
        store.offer("kindle", RED.to_vec(), at(0));
        let oldest = store.current("kindle").unwrap().hash;
        store.offer("kindle", GREEN.to_vec(), at(300));
        let middle = store.current("kindle").unwrap().hash;
        store.offer("kindle", BLUE.to_vec(), at(600));
        let newest = store.current("kindle").unwrap().hash;

        assert!(store.by_hash("kindle", &newest).is_some());
        assert!(store.by_hash("kindle", &middle).is_some());
        assert!(
            store.by_hash("kindle", &oldest).is_none(),
            "two generations back must be dropped"
        );
    }

    /// An unchanged offer is not a promotion, so it must not age out the
    /// retained generation either.
    #[test]
    fn unchanged_offer_does_not_age_out_the_generation() {
        let store = FrameStore::new();
        store.offer("kindle", RED.to_vec(), at(0));
        let previous = store.current("kindle").unwrap().hash;
        store.offer("kindle", GREEN.to_vec(), at(300));

        assert!(!store.offer("kindle", GREEN.to_vec(), at(600)));

        assert!(store.by_hash("kindle", &previous).is_some());
    }

    #[test]
    fn unknown_hash_and_unknown_device_are_none() {
        let store = FrameStore::new();
        store.offer("kindle", RED.to_vec(), at(0));

        assert!(store.by_hash("kindle", "not-a-hash").is_none());
        assert!(store.by_hash("kindle", &frame_hash(BLUE)).is_none());
        assert!(store.by_hash("desk", &frame_hash(RED)).is_none());
        assert!(store.current("desk").is_none());
    }

    #[test]
    fn devices_are_independent() {
        let store = FrameStore::new();
        store.offer("kindle", RED.to_vec(), at(0));
        store.offer("kindle", GREEN.to_vec(), at(300));
        store.offer("desk", BLUE.to_vec(), at(600));

        // The desk offer must not have touched the kindle's frame or its
        // retained generation.
        let kindle = store.current("kindle").unwrap();
        assert_eq!(&*kindle.bytes, GREEN);
        assert_eq!(kindle.rendered_at, at(300));
        assert!(store.by_hash("kindle", &frame_hash(RED)).is_some());

        let desk = store.current("desk").unwrap();
        assert_eq!(&*desk.bytes, BLUE);
        assert!(store.by_hash("desk", &frame_hash(RED)).is_none());

        // And the reverse: a fresh kindle offer leaves the desk alone.
        assert!(store.offer("kindle", RED.to_vec(), at(900)));
        assert_eq!(&*store.current("desk").unwrap().bytes, BLUE);
        assert_eq!(store.current("desk").unwrap().rendered_at, at(600));
    }

    #[test]
    fn a_poisoned_lock_does_not_wedge_the_store() {
        let store = Arc::new(FrameStore::new());
        store.offer("kindle", RED.to_vec(), at(0));

        let poisoner = Arc::clone(&store);
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.lock();
            panic!("handler panicked while holding the frame lock");
        })
        .join();

        assert_eq!(&*store.current("kindle").unwrap().bytes, RED);
        assert!(store.offer("kindle", GREEN.to_vec(), at(300)));
    }
}
