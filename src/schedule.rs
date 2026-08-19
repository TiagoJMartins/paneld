//! Which devices the render loop should rebuild right now.
//!
//! [`due_devices`] is the whole of it: a pure function of the device set, their
//! `render_interval`s, their last render times and a supplied `now`. The render
//! loop owns that state and passes it in, so the decision can be asserted
//! directly instead of against the wall clock — which is the entire reason it is
//! extracted from the loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// The devices in `devices` that are due for a rebuild at `now`.
///
/// `devices` yields `(device id, render_interval in seconds)`; `last_render`
/// maps a device id to the instant its frame was last rebuilt. A device is due
/// when it has never been rendered, or when at least its `render_interval` has
/// elapsed since it last was.
///
/// Input order is preserved. The caller unions this with the device ids drained
/// from its wake channel and renders each survivor once, so a stable order keeps
/// that union — and the resulting render sequence — reproducible.
pub fn due_devices<'a>(
    devices: impl IntoIterator<Item = (&'a str, u32)>,
    last_render: &HashMap<String, Instant>,
    now: Instant,
) -> Vec<&'a str> {
    devices
        .into_iter()
        .filter(|&(id, render_interval)| is_due(last_render.get(id), render_interval, now))
        .map(|(id, _)| id)
        .collect()
}

/// Whether one device is due, given the instant it was last rendered.
fn is_due(last_render: Option<&Instant>, render_interval: u32, now: Instant) -> bool {
    // No record means the device has never been rendered. Reporting it due is
    // what renders every configured device at startup, before the listener
    // accepts, so a device polling immediately gets a real frame.
    let Some(&last_render) = last_render else {
        return true;
    };

    // Saturating, not `now - last_render`: a caller holding a stale instant must
    // get "not due" rather than a panic.
    now.saturating_duration_since(last_render) >= Duration::from_secs(u64::from(render_interval))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All instants in these tests are offsets from one base, so nothing sleeps
    /// and nothing depends on how long the test itself takes.
    fn base() -> Instant {
        Instant::now()
    }

    fn last_render(entries: &[(&str, Instant)]) -> HashMap<String, Instant> {
        entries
            .iter()
            .map(|&(id, at)| (id.to_owned(), at))
            .collect()
    }

    #[test]
    fn elapsed_interval_is_due_and_unelapsed_is_not() {
        let base = base();
        let rendered = last_render(&[("kitchen", base)]);

        let elapsed = due_devices(
            [("kitchen", 300)],
            &rendered,
            base + Duration::from_secs(301),
        );
        assert_eq!(elapsed, ["kitchen"]);

        let unelapsed = due_devices(
            [("kitchen", 300)],
            &rendered,
            base + Duration::from_secs(299),
        );
        assert!(unelapsed.is_empty());
    }

    #[test]
    fn boundary_is_inclusive_to_the_nanosecond() {
        let base = base();
        let interval = Duration::from_secs(300);
        let rendered = last_render(&[("kitchen", base)]);

        let exactly = due_devices([("kitchen", 300)], &rendered, base + interval);
        assert_eq!(exactly, ["kitchen"], "elapsed == interval is due");

        let one_short = due_devices(
            [("kitchen", 300)],
            &rendered,
            base + interval - Duration::from_nanos(1),
        );
        assert!(one_short.is_empty(), "one nanosecond short is not due");
    }

    #[test]
    fn device_never_rendered_is_due() {
        let base = base();
        let rendered = last_render(&[("kitchen", base)]);

        // `study` has no record at all, and the longest possible interval must
        // not keep it from its first frame.
        let due = due_devices([("study", 86_400)], &rendered, base);
        assert_eq!(due, ["study"]);
    }

    #[test]
    fn differing_intervals_become_due_independently() {
        let base = base();
        let rendered = last_render(&[("fast", base), ("slow", base)]);
        let devices = [("fast", 5), ("slow", 600)];

        let due = due_devices(devices, &rendered, base + Duration::from_secs(10));
        assert_eq!(
            due,
            ["fast"],
            "slow is absent until its own interval elapses"
        );

        let due = due_devices(devices, &rendered, base + Duration::from_secs(600));
        assert_eq!(due, ["fast", "slow"]);
    }

    #[test]
    fn last_render_in_the_future_is_not_due() {
        let base = base();
        let rendered = last_render(&[("kitchen", base + Duration::from_secs(60))]);

        let due = due_devices([("kitchen", 300)], &rendered, base);
        assert!(due.is_empty());
    }

    #[test]
    fn input_order_is_preserved() {
        let base = base();
        let rendered = last_render(&[("b", base), ("skipped", base)]);
        let devices = [("c", 300), ("skipped", 86_400), ("a", 300), ("b", 300)];

        let due = due_devices(devices, &rendered, base + Duration::from_secs(300));

        // Not sorted, not HashMap iteration order: exactly the input order with
        // the undue device removed.
        assert_eq!(due, ["c", "a", "b"]);
    }

    #[test]
    fn empty_device_set_yields_nothing() {
        let base = base();
        let rendered = last_render(&[("kitchen", base)]);

        let due = due_devices([], &rendered, base + Duration::from_secs(86_400));
        assert!(due.is_empty());
    }
}
