//! Tap actions: what touching a widget's cell does.
//!
//! **No shipping client reports a tap to a BYOS server.** The KOReader plugin's
//! only gesture handler closes the dashboard; it registers a single full-screen tap
//! range that carries no coordinates, and its display poll sends no touch header.
//! The ESP32 firmware sends none either, and `special_function` runs server to
//! device, never the other way.
//!
//! So this surface has exactly two callers in mind: a fork of the KOReader plugin,
//! which is the eventual client and explicitly not in scope here, and any script,
//! phone shortcut or automation on the LAN that can issue one HTTP request.
//!
//! That second caller is why nothing here trusts its input and nothing here is
//! fatal. A tap arrives from something with no user interface: it may repeat itself,
//! it may name a point in a gutter, and it can do nothing useful with an error. Each
//! of those is an [`Outcome`] and a log line, never a failed request.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde::Serialize;

use crate::config::{Tap, Widget};
use crate::ha::HaClient;

/// Event ids remembered per device.
///
/// Enough that a client retrying a burst of taps is still deduplicated after
/// several unrelated taps have gone through, and small enough that the whole
/// ledger stays a few kilobytes.
const MAX_EVENTS_PER_DEVICE: usize = 32;

/// Devices whose event ids are remembered.
///
/// Bounded because the key is client-supplied: a device id nobody configured can
/// tap just as easily as a real panel can.
const MAX_DEVICES: usize = 64;

/// What came of a tap. A closed vocabulary, because it is the response body and
/// a client's next move is the same for most of it: poll again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// A service call was made.
    Dispatched,
    /// A re-render was requested and nothing left this process.
    Refreshed,
    /// The point landed in a gutter, outside the frame, or on an empty cell.
    NoTarget,
    /// The point landed on a widget that declares no `tap`.
    NoAction,
    /// Already handled: the same event id arrived twice.
    Deduped,
    /// Home Assistant refused or was unreachable. Never fatal.
    Failed,
}

impl std::fmt::Display for Outcome {
    /// The spelling the wire uses, so a log line and a response body agree.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Dispatched => "dispatched",
            Self::Refreshed => "refreshed",
            Self::NoTarget => "no_target",
            Self::NoAction => "no_action",
            Self::Deduped => "deduped",
            Self::Failed => "failed",
        })
    }
}

/// One tap, resolved: what happened and what it happened to.
///
/// The [`Outcome`] alone is not enough to answer with. A caller aiming at a
/// coordinate cannot know which cell that was, so naming the widget it resolved to
/// is the only way to tell a hit on the wrong cell from a hit on the right one.
pub struct Report {
    pub outcome: Outcome,
    /// The widget the point landed on, absent when it landed on nothing.
    pub widget: Option<String>,
    /// What the outcome was about: the `domain.service` a service tap names.
    ///
    /// Never a failure's message. That text comes from another system, has no bound
    /// on its length, and is of use to an operator reading logs rather than to a
    /// caller whose only move is to try again.
    pub detail: Option<String>,
}

impl Report {
    /// An outcome that names no widget, because the tap resolved to none.
    pub fn bare(outcome: Outcome) -> Self {
        Self {
            outcome,
            widget: None,
            detail: None,
        }
    }
}

/// Performs what a widget's `tap` names.
///
/// Never fails. A Home Assistant that refuses or cannot be reached is
/// [`Outcome::Failed`] and a warning, because the caller is an HTTP handler that
/// must answer either way and the panel has nowhere to show an exception.
pub async fn dispatch(ha: Option<&dyn HaClient>, widget: &Widget) -> Report {
    let named = |outcome, detail| Report {
        outcome,
        widget: Some(widget.id.clone()),
        detail,
    };

    let Some(tap) = &widget.tap else {
        return named(Outcome::NoAction, None);
    };

    let call = match tap {
        // Asking the render loop for the rebuild is the caller's job: it holds the
        // wake channel, and a dispatched service call wants the same rebuild.
        Tap::Refresh => return named(Outcome::Refreshed, None),
        Tap::Service(call) => call,
    };

    // Config validation refuses a service tap without a `[home_assistant]`
    // section, so a missing client here means building it failed at startup.
    let Some(ha) = ha else {
        tracing::warn!(
            widget = %widget.id,
            service = %call,
            "a tap named a Home Assistant service but no client was built"
        );
        return named(Outcome::Failed, Some(call.to_string()));
    };

    match ha.call(call).await {
        Ok(()) => {
            tracing::info!(widget = %widget.id, service = %call, "tap dispatched");
            named(Outcome::Dispatched, Some(call.to_string()))
        }
        Err(error) => {
            tracing::warn!(
                widget = %widget.id,
                service = %call,
                error = format!("{error:#}"),
                "tap failed; Home Assistant refused or was unreachable"
            );
            named(Outcome::Failed, Some(call.to_string()))
        }
    }
}

/// Recent tap event ids, per device, so a client that retries does not act twice.
///
/// A bounded ring rather than a single last-seen id: a retry may arrive after an
/// unrelated tap, and equality against only the newest would let the retry
/// through. Never a time window — two deliberate taps a second apart are two
/// taps, and swallowing the second is worse than acting twice on a duplicate.
///
/// A tap carrying no event id is never deduplicated, because there is nothing to
/// compare it against. That makes at-most-once delivery something a client asks for
/// by naming its taps, rather than something guessed at on its behalf.
#[derive(Debug, Default)]
pub struct Taps {
    /// Least recently tapped device first, so eviction takes a device nothing is
    /// touching. A `Mutex` rather than anything cleverer because a tap is a
    /// human-rate event and there is no contention here to design around.
    devices: Mutex<VecDeque<Ledger>>,
}

/// One device's ring of event ids, oldest first.
#[derive(Debug)]
struct Ledger {
    device_id: String,
    events: VecDeque<String>,
}

impl Taps {
    /// An empty ledger: nothing has been tapped yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an event id, returning whether it had already been seen.
    ///
    /// The recording happens either way, so a caller cannot forget to make it and
    /// let the next retry through.
    pub fn seen(&self, device_id: &str, event_id: &str) -> bool {
        let mut devices = self
            .devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        let mut ledger = match devices.iter().position(|d| d.device_id == device_id) {
            Some(index) => devices
                .remove(index)
                .expect("the index came from this deque"),
            None => Ledger {
                device_id: device_id.to_owned(),
                events: VecDeque::with_capacity(1),
            },
        };

        let already = ledger.events.iter().any(|seen| seen == event_id);
        if !already {
            if ledger.events.len() >= MAX_EVENTS_PER_DEVICE {
                ledger.events.pop_front();
            }
            ledger.events.push_back(event_id.to_owned());
        }

        // Pushed to the back whether or not this was a duplicate, so the devices
        // actually being tapped are the last to be evicted: a storm of mistyped
        // device ids must not push a real panel's history out from under it.
        while devices.len() >= MAX_DEVICES {
            devices.pop_front();
        }
        devices.push_back(ledger);

        already
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServiceCall, WidgetKind};
    use crate::ha::Reading;

    /// A [`HaClient`] that records every service call and answers however the test
    /// asked it to.
    ///
    /// Records under a lock rather than into a counter because these cases assert on
    /// the call that was posted, not merely that something was.
    struct StubHa {
        calls: Mutex<Vec<ServiceCall>>,
        refuses: Option<String>,
    }

    impl StubHa {
        fn accepting() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                refuses: None,
            }
        }

        fn refusing(message: &str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                refuses: Some(message.to_owned()),
            }
        }

        fn calls(&self) -> Vec<ServiceCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl HaClient for StubHa {
        async fn read(&self, reading: &Reading) -> anyhow::Result<String> {
            anyhow::bail!("no case here reads `{reading}`")
        }

        async fn call(&self, call: &ServiceCall) -> anyhow::Result<()> {
            self.calls.lock().unwrap().push(call.clone());
            match &self.refuses {
                Some(message) => anyhow::bail!("{message}"),
                None => Ok(()),
            }
        }
    }

    fn widget(id: &str, tap: Option<Tap>) -> Widget {
        Widget {
            id: id.to_owned(),
            kind: WidgetKind::Value,
            col: 0,
            row: 0,
            col_span: 1,
            row_span: 1,
            label: None,
            unit: None,
            stale_after: 0,
            entity: None,
            attribute: None,
            on_values: Vec::new(),
            icon: None,
            tap,
        }
    }

    fn toggle(entity: &str) -> Tap {
        let mut data = serde_json::Map::new();
        data.insert(
            "entity_id".to_owned(),
            serde_json::Value::String(entity.to_owned()),
        );
        Tap::Service(ServiceCall {
            domain: "light".to_owned(),
            service: "toggle".to_owned(),
            data,
        })
    }

    #[tokio::test]
    async fn a_service_tap_posts_the_call_verbatim_and_reports_it() {
        let ha = StubHa::accepting();
        let widget = widget("desk_lamp", Some(toggle("light.desk")));

        let report = dispatch(Some(&ha), &widget).await;

        assert_eq!(report.outcome, Outcome::Dispatched);
        assert_eq!(report.widget.as_deref(), Some("desk_lamp"));
        assert_eq!(report.detail.as_deref(), Some("light.toggle"));
        let calls = ha.calls();
        assert_eq!(calls.len(), 1, "exactly one call per tap");
        assert_eq!(calls[0].domain, "light");
        assert_eq!(calls[0].service, "toggle");
        assert_eq!(calls[0].data["entity_id"], "light.desk");
    }

    #[tokio::test]
    async fn a_refresh_tap_reaches_nothing_outside_this_process() {
        let ha = StubHa::accepting();
        let widget = widget("panel", Some(Tap::Refresh));

        let report = dispatch(Some(&ha), &widget).await;

        assert_eq!(report.outcome, Outcome::Refreshed);
        assert_eq!(report.widget.as_deref(), Some("panel"));
        assert_eq!(report.detail, None);
        assert!(ha.calls().is_empty(), "a refresh calls no service");
    }

    #[tokio::test]
    async fn a_widget_with_no_tap_is_hit_but_does_nothing() {
        let ha = StubHa::accepting();
        let widget = widget("office_temp", None);

        let report = dispatch(Some(&ha), &widget).await;

        assert_eq!(report.outcome, Outcome::NoAction);
        assert_eq!(
            report.widget.as_deref(),
            Some("office_temp"),
            "the cell is still identified, which is how a caller learns it is inert"
        );
        assert!(ha.calls().is_empty());
    }

    #[tokio::test]
    async fn a_refused_service_call_is_reported_rather_than_raised() {
        let ha = StubHa::refusing("Home Assistant returned HTTP 400: not a valid service");
        let widget = widget("desk_lamp", Some(toggle("light.desk")));

        let report = dispatch(Some(&ha), &widget).await;

        assert_eq!(report.outcome, Outcome::Failed);
        assert_eq!(report.widget.as_deref(), Some("desk_lamp"));
        assert_eq!(
            report.detail.as_deref(),
            Some("light.toggle"),
            "the detail names the action, never the foreign error text"
        );
        assert_eq!(ha.calls().len(), 1, "it was attempted");
    }

    #[tokio::test]
    async fn a_service_tap_with_no_client_fails_rather_than_silently_doing_nothing() {
        let widget = widget("desk_lamp", Some(toggle("light.desk")));

        let report = dispatch(None, &widget).await;

        assert_eq!(report.outcome, Outcome::Failed);
        assert_eq!(report.detail.as_deref(), Some("light.toggle"));
    }

    #[test]
    fn an_outcome_spells_itself_the_same_way_on_the_wire_and_in_a_log() {
        let cases = [
            (Outcome::Dispatched, "dispatched"),
            (Outcome::Refreshed, "refreshed"),
            (Outcome::NoTarget, "no_target"),
            (Outcome::NoAction, "no_action"),
            (Outcome::Deduped, "deduped"),
            (Outcome::Failed, "failed"),
        ];

        for (outcome, spelling) in cases {
            assert_eq!(outcome.to_string(), spelling);
            assert_eq!(
                serde_json::to_value(outcome).unwrap(),
                serde_json::Value::String(spelling.to_owned()),
                "a log line and a response body must agree"
            );
        }
    }

    #[test]
    fn a_repeated_event_id_is_seen_and_a_fresh_one_is_not() {
        let taps = Taps::new();

        assert!(
            !taps.seen("kindle", "e1"),
            "the first sight is not a repeat"
        );
        assert!(taps.seen("kindle", "e1"), "the second sight is");
        assert!(!taps.seen("kindle", "e2"), "a different id is its own tap");
        assert!(
            taps.seen("kindle", "e1"),
            "and the first is still remembered"
        );
    }

    /// The case a single last-seen id would get wrong: a retry arriving after an
    /// unrelated tap has already gone through.
    #[test]
    fn a_retry_is_caught_even_after_unrelated_taps() {
        let taps = Taps::new();
        assert!(!taps.seen("kindle", "retried"));
        for n in 0..MAX_EVENTS_PER_DEVICE - 1 {
            assert!(!taps.seen("kindle", &format!("other{n}")));
        }

        assert!(
            taps.seen("kindle", "retried"),
            "still inside the ring, so the retry is caught"
        );
    }

    #[test]
    fn devices_do_not_share_a_ledger() {
        let taps = Taps::new();

        assert!(!taps.seen("kindle", "e1"));
        assert!(
            !taps.seen("kitchen", "e1"),
            "two panels may well number their taps the same way"
        );
        assert!(taps.seen("kindle", "e1"));
        assert!(taps.seen("kitchen", "e1"));
    }

    #[test]
    fn the_ring_is_bounded_per_device_and_forgets_its_oldest_first() {
        let taps = Taps::new();

        assert!(!taps.seen("kindle", "oldest"));
        for n in 0..MAX_EVENTS_PER_DEVICE {
            assert!(!taps.seen("kindle", &format!("e{n}")));
        }

        assert!(
            !taps.seen("kindle", "oldest"),
            "the oldest id is evicted rather than the ledger growing without bound"
        );
        assert!(
            taps.seen("kindle", &format!("e{}", MAX_EVENTS_PER_DEVICE - 1)),
            "the newest ids are the ones kept"
        );
    }

    /// A storm of device ids nobody configured must cost a bounded amount of
    /// memory, and must not cost the real panel its history.
    #[test]
    fn the_device_ledger_is_bounded_and_keeps_the_devices_being_tapped() {
        let taps = Taps::new();

        assert!(!taps.seen("kindle", "e1"));
        for n in 0..MAX_DEVICES * 2 {
            assert!(!taps.seen(&format!("typo{n}"), "e1"));
            // The real panel keeps tapping throughout, which is what keeps it in.
            assert!(taps.seen("kindle", "e1"), "the real panel is never evicted");
        }
    }
}
