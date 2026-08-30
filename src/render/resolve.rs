//! What a cell shows: resolving a widget's configuration plus whatever data
//! exists — pushed content, or a Home Assistant reading — into a [`Cell`], before
//! `scaffold` and `body` draw it.

use std::collections::HashMap;

use time::OffsetDateTime;

use crate::config::{Device, Reading as ConfiguredReading, Widget, WidgetKind};
use crate::content::{ContentRecord, Row};
use crate::ha::{Reading, Reported};
use crate::icon::Icon;
use crate::state::{Trend, trend_key};

use super::RenderInputs;
use super::icon;
use super::types::{Body, Cell, Ink, Line};

/// Every kind is named rather than swept into a wildcard, so adding one is a
/// compile error here instead of a cell that silently waits for a push that will
/// never come.
pub(super) fn resolve(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    match widget.kind {
        // A group holds no reading of its own; its children are resolved
        // individually, which is what `cell_node` does when it draws them.
        WidgetKind::Group => Cell {
            body: Body::Group,
            ink: Ink::Current,
        },
        // A list whose readings the author declared reads Home Assistant; one that
        // declares none is fed by push, which is how a publisher sends a shopping
        // list nobody could have written into a config file. Told apart by
        // `Widget::fed_by_push`, which the layout reads too — a cell drawn from a
        // push and charged a track as though it read Home Assistant is a list drawn
        // over its neighbour.
        WidgetKind::List if widget.fed_by_push() => resolve_pushed(widget, inputs),
        WidgetKind::HaEntity | WidgetKind::Weather | WidgetKind::List => resolve_ha(widget, inputs),
        WidgetKind::Value | WidgetKind::Beacon | WidgetKind::Text => resolve_pushed(widget, inputs),
    }
}

/// A cell fed by `PUT /api/content/{id}`.
fn resolve_pushed(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    let Some(record) = inputs.content.get(&widget.id) else {
        return Cell {
            body: Body::Absent("no data"),
            ink: Ink::Current,
        };
    };

    // A publisher that has gone quiet past its `stale_after` keeps its last value
    // on the glass, muted and marked. It is still the most recent thing anyone
    // said, and replacing it with a countdown throws away the only information
    // the cell had.
    let ink = match is_stale(widget, record, inputs.now) {
        true => Ink::Held,
        false => Ink::Current,
    };

    // `rows` is a presentation override available to any kind: when it is present
    // the scalar `value` is ignored, which is what lets one widget show a small
    // group of related readings.
    //
    // Every row shares the record's trust, unlike a `list`'s. One push either
    // arrived in time or did not: a pushed row names no source of its own, so there
    // is nothing for it to be separately unreachable from.
    if let Some(rows) = &record.rows {
        return Cell {
            body: Body::Rows(
                rows.iter()
                    .map(|row| Line {
                        row: rounded_row(row, widget.precision),
                        // A pushed row names no icon: the push protocol carries
                        // values, and a publisher that could choose glyphs would be
                        // choosing the dashboard's appearance from outside it.
                        icon: None,
                        // Nor a trend: a pushed row is identified by whatever the
                        // publisher called it, so there is no configured reading for
                        // an arrow to belong to and no stable key to remember it by.
                        trend: None,
                        ink,
                    })
                    .collect(),
            ),
            ink,
        };
    }

    let body = match widget.kind {
        WidgetKind::Value => Body::Figure {
            text: format_reading(&value_text(&record.value), widget.precision),
            unit: record.unit.clone().or_else(|| widget.unit.clone()),
            trend: arrow(inputs, &widget.id, None, widget.trend),
        },
        WidgetKind::Beacon => {
            let on = beacon_is_on(record, &widget.on_values);
            Body::Beacon {
                on,
                icon: beacon_icon(widget, on, inputs),
                text: widget.state_text,
            }
        }
        WidgetKind::Text => Body::Prose(value_text(&record.value)),
        // A pushed list whose record carried a scalar rather than rows: the
        // publisher sent something, and it was not a list. Named rather than drawn
        // as a one-row table, because a cell that quietly showed the scalar would
        // leave an author reloading the page wondering where the other rows went.
        WidgetKind::List => Body::Absent("no rows"),
        WidgetKind::HaEntity | WidgetKind::Weather => {
            unreachable!("handled by resolve_ha")
        }
        WidgetKind::Group => unreachable!("handled by resolve"),
    };
    Cell { body, ink }
}

/// A pushed row with its value rounded to the widget's precision.
///
/// A publisher's row is a numeric reading like any other, so it goes through the one
/// formatter rather than around it. With no precision configured — the default —
/// [`format_reading`] hands the text back untouched, so a publisher's own digits
/// survive unless somebody actually asked for rounding.
fn rounded_row(row: &Row, precision: Option<u8>) -> Row {
    Row {
        // A row with no value at all keeps none, rather than becoming an empty
        // string that would render as a label with a blank beside it.
        value: row
            .value
            .as_ref()
            .map(|value| serde_json::Value::String(format_reading(&value_text(value), precision))),
        ..row.clone()
    }
}

/// A beacon's indicator icon for the state it is currently in.
///
/// `None` when that state names no icon, and equally when it names one the icon
/// store could not resolve. Both fall back to the dot, and that is what makes
/// configuring `icon_on` alone legal: the on state gets its picture and the off
/// state stays the hollow dot it has always been, rather than the cell losing its
/// indicator entirely.
fn beacon_icon(widget: &Widget, on: bool, inputs: &RenderInputs<'_>) -> Option<Icon> {
    let spec = match on {
        true => widget.icon_on.as_ref(),
        false => widget.icon_off.as_ref(),
    }?;
    inputs.icons.get(spec).cloned()
}

/// A cell read from Home Assistant. A fetch failure degrades this cell only: the
/// frame still renders, because one unreachable integration must not blank the
/// dashboard.
fn resolve_ha(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    // A list has no reading of its own — its body *is* its readings, each naming its
    // own entity — so it resolves ahead of the entity check below, which it would
    // otherwise fail for want of a widget-level entity it never needed.
    if widget.kind == WidgetKind::List {
        return resolve_list(widget, inputs);
    }

    let Some(entity) = &widget.entity else {
        // Config validation rejects this, so it cannot happen from a config file.
        return Cell {
            body: Body::Absent("no entity"),
            ink: Ink::Current,
        };
    };
    let reading = ha_reading(entity, widget.attribute.as_deref());

    let (value, ink) = match inputs.ha_states.get(&reading) {
        Some(Reported::Fresh(value)) => (value.as_str(), Ink::Current),
        Some(Reported::Held(value)) => (value.as_str(), Ink::Held),
        // Nothing was ever read. A missing key means the caller never asked, which
        // for a validated config means Home Assistant is not configured.
        //
        // Unmarked, unlike a held value: the mark says the value below is not
        // confirmed current, and there is no value below for it to qualify. `no data`
        // and a corner mark say the same thing twice, and the absence is already
        // drawn muted.
        Some(Reported::Lost) | None => {
            return Cell {
                body: Body::Absent("no data"),
                ink: Ink::Current,
            };
        }
    };

    // Empty for every kind but `weather`, and cheap when it is: an empty `Vec` does
    // not allocate.
    let rows = lines(widget, inputs);
    // A weather cell's condition may be current while a reading beside it is held,
    // so the cell's mark is decided over both. Per-line muting says which line is
    // stale; the mark is what a viewer scanning the whole dashboard sees first.
    let ink = held_over(ink, &rows);

    let body = match widget.kind {
        WidgetKind::Weather => {
            let (svg, label) = match icon::Condition::parse(value) {
                Some(condition) => (condition.svg(), condition.label().to_owned()),
                // An unrecognised condition still shows what Home Assistant said,
                // because a new condition slug is a thing to notice rather than hide.
                None => (icon::UNKNOWN_SKY, value.to_owned()),
            };
            Body::Sky {
                svg,
                condition: widget.state_text.then_some(label),
                rows,
            }
        }
        _ => Body::Figure {
            text: format_reading(value, widget.precision),
            unit: widget.unit.clone(),
            trend: arrow(inputs, &widget.id, None, widget.trend),
        },
    };
    Cell { body, ink }
}

/// A cell whose body is its configured readings, one line each.
///
/// Every line is trusted on its own, which is the feature: a reading the last
/// request could not confirm is muted where it stands, one that was never read shows
/// an em dash, and the lines around either of them stay black. Muting the whole cell
/// because one sensor is unreachable would throw away the readings the panel does
/// still have.
///
/// Reached only by a list that declared readings: one that declared none is a pushed
/// cell, which [`resolve`] routes to [`resolve_pushed`] before this is called.
fn resolve_list(widget: &Widget, inputs: &RenderInputs<'_>) -> Cell {
    let rows = lines(widget, inputs);
    let ink = held_over(Ink::Current, &rows);
    Cell {
        body: Body::Rows(rows),
        ink,
    }
}

/// Every configured reading of a cell, resolved to lines in the order written.
fn lines(widget: &Widget, inputs: &RenderInputs<'_>) -> Vec<Line> {
    widget
        .readings
        .iter()
        .enumerate()
        .map(|(index, reading)| reading_line(reading, index, widget, inputs))
        .collect()
}

/// The arrow a cell or a row draws, and `None` for one that asked for none.
///
/// Also `None` for a reading that asked but whose value is not a number: the
/// caller never stepped a key for it, so there is nothing to draw and nothing to
/// invent.
fn arrow(
    inputs: &RenderInputs<'_>,
    widget_id: &str,
    reading: Option<usize>,
    wanted: bool,
) -> Option<Trend> {
    if !wanted {
        return None;
    }
    inputs
        .trends
        .get(&trend_key(&inputs.device.id, widget_id, reading))
        .copied()
}

/// One configured reading as a line, carrying how far its own value can be trusted.
///
/// A reading nothing was ever read for keeps its label and its place with a null
/// value, which renders as an em dash: the cell still lists what it is meant to
/// have, and says of that one line that it does not know. Dropping the line instead
/// would silently shorten the list, and a short list reads as configuration rather
/// than as a failure.
fn reading_line(
    reading: &ConfiguredReading,
    index: usize,
    widget: &Widget,
    inputs: &RenderInputs<'_>,
) -> Line {
    let key = ha_reading(&reading.entity, reading.attribute.as_deref());
    let (value, unit, ink) = match inputs.ha_states.get(&key) {
        Some(Reported::Fresh(text)) => (
            serde_json::Value::String(format_reading(text, reading.precision)),
            reading.unit.clone(),
            Ink::Current,
        ),
        Some(Reported::Held(text)) => (
            serde_json::Value::String(format_reading(text, reading.precision)),
            reading.unit.clone(),
            Ink::Held,
        ),
        // The unit goes with the value it qualified. `— °C` claims a reading in
        // degrees that nobody has.
        Some(Reported::Lost) | None => (serde_json::Value::Null, None, Ink::Held),
    };
    Line {
        row: Row {
            id: None,
            label: reading.label.clone(),
            value: Some(value),
            unit,
            state: None,
        },
        icon: reading
            .icon
            .as_ref()
            .and_then(|spec| inputs.icons.get(spec))
            .cloned(),
        trend: arrow(inputs, &widget.id, Some(index), reading.trend),
        ink,
    }
}

/// The Home Assistant reading an entity and an optional attribute name.
///
/// One function so that a widget's own reading, a `list` row's and a weather cell's
/// extra readings are all looked up under the same key. A second spelling of this
/// arithmetic is a cell reading `no data` because it asked the map a question the
/// fetcher never answered.
fn ha_reading(entity: &str, attribute: Option<&str>) -> Reading {
    match attribute {
        Some(attribute) => Reading::attribute(entity, attribute),
        None => Reading::state(entity),
    }
}

/// A cell's ink, given its own reading's and its lines'.
///
/// [`Ink::Held`] wins. The corner mark means "something in this cell is not
/// confirmed current", and that is true the moment one line is holding or missing a
/// value, however black the rest of the cell is.
fn held_over(own: Ink, lines: &[Line]) -> Ink {
    match own == Ink::Held || lines.iter().any(|line| line.ink == Ink::Held) {
        true => Ink::Held,
        false => Ink::Current,
    }
}

/// A reading's text at a configured number of decimal places.
///
/// The one place rounding happens, so that a widget's `precision`, a device's
/// default and a single reading's override cannot come to mean three slightly
/// different things in three cells.
///
/// `None` hands the text back untouched, and so does anything that does not parse as
/// a number. That fallthrough is the whole subtlety: a Home Assistant state is a
/// string that merely happens to be numeric most of the time, so `unavailable`,
/// `partlycloudy` and a publisher's `23.4 °C` all arrive here and all have to
/// survive verbatim. Refusing to render them would turn a cosmetic setting into a
/// blank cell, and coercing them to `0.0` would be worse still — a number on the
/// glass that no sensor ever reported, and one that looks exactly like a reading.
fn format_reading(text: &str, precision: Option<u8>) -> String {
    let Some(places) = precision else {
        return text.to_owned();
    };
    let Ok(number) = text.trim().parse::<f64>() else {
        return text.to_owned();
    };
    let places = places as usize;
    format!("{number:.places$}")
}

/// Every reading on `device` that asked for a trend, with the number the coming
/// frame will print for it.
///
/// Walked here rather than in the render loop because the number a cell shows is
/// this module's business: sharing [`ha_reading`] and [`format_reading`] with the
/// resolve path is what keeps an arrow describing the very digits beside it. The
/// caller steps each of these into the persisted trend and hands the directions
/// back in [`RenderInputs::trends`], so the render itself stays pure.
///
/// A reading whose text is not a number is left out entirely, and its cell simply
/// draws no arrow: `unavailable` has no direction.
pub(crate) fn shown_numbers(
    device: &Device,
    content: &HashMap<String, ContentRecord>,
    ha_states: &HashMap<Reading, Reported>,
) -> Vec<(String, f64)> {
    let mut shown = Vec::new();
    for widget in device.all_widgets() {
        if widget.trend {
            let text = match widget.kind {
                WidgetKind::HaEntity => widget.entity.as_deref().and_then(|entity| {
                    reported_text(ha_states, entity, widget.attribute.as_deref()).map(str::to_owned)
                }),
                WidgetKind::Value => content
                    .get(&widget.id)
                    .map(|record| value_text(&record.value)),
                // A list or weather cell has no figure of its own to mark: the flag
                // was inherited by its readings, and it is spent on them below.
                _ => None,
            };
            if let Some(number) = text.and_then(|text| shown_number(&text, widget.precision)) {
                shown.push((trend_key(&device.id, &widget.id, None), number));
            }
        }
        for (index, reading) in widget.readings.iter().enumerate() {
            if !reading.trend {
                continue;
            }
            let Some(text) =
                reported_text(ha_states, &reading.entity, reading.attribute.as_deref())
            else {
                continue;
            };
            if let Some(number) = shown_number(text, reading.precision) {
                shown.push((trend_key(&device.id, &widget.id, Some(index)), number));
            }
        }
    }
    shown
}

/// The text a Home Assistant reading currently shows, whether it was confirmed or
/// is being held.
///
/// Held counts: the cell is showing that number, so it is the number an arrow
/// must be measured against. Treating a held reading as absent would flip every
/// arrow to steady for the duration of an outage and then invent a direction from
/// a value hours old when it came back.
fn reported_text<'a>(
    ha_states: &'a HashMap<Reading, Reported>,
    entity: &str,
    attribute: Option<&str>,
) -> Option<&'a str> {
    match ha_states.get(&ha_reading(entity, attribute)) {
        Some(Reported::Fresh(text) | Reported::Held(text)) => Some(text.as_str()),
        Some(Reported::Lost) | None => None,
    }
}

/// The number a cell will print, or `None` when what it prints is not one.
///
/// Rounded through [`format_reading`] before parsing, deliberately: the trend is a
/// statement about the *displayed* value, so `21.4` and `21.2` at precision `0`
/// are the same number and the arrow between them does not move. That is what
/// makes the mark free — it can only change on a frame where the digits changed
/// too — and it is why no deadband has to be invented.
fn shown_number(text: &str, precision: Option<u8>) -> Option<f64> {
    format_reading(text, precision).trim().parse().ok()
}

/// Whether a pushed record is older than its widget's `stale_after`.
fn is_stale(widget: &Widget, record: &ContentRecord, now: OffsetDateTime) -> bool {
    widget.stale_after > 0 && is_stale_after(widget.stale_after, record, now)
}

/// Whether a pushed record is older than `stale_after` seconds.
///
/// Computed at render time rather than stamped at push time, so raising or
/// lowering the window takes effect on the next frame. A record stamped in the
/// future is never stale: that is a clock disagreement, not freshness
/// information, and treating it as stale would mute a cell that is fine.
///
/// Shared with the status bar's alerts, which apply the same age to the opposite
/// decision — a stale cell mutes its reading, a stale alert withdraws itself — so
/// the arithmetic is written once and the policy lives at each call site.
pub(super) fn is_stale_after(
    stale_after: u64,
    record: &ContentRecord,
    now: OffsetDateTime,
) -> bool {
    let age = now - record.received_at;
    age.whole_seconds() >= 0 && age.unsigned_abs().as_secs() > stale_after
}

/// Whether a beacon reads as "on".
///
/// `state` takes precedence: when a publisher sends one it is the authoritative
/// signal, so a non-matching `state` means off rather than falling through to
/// `value`. Falling through would make `{"state":"idle","value":"on"}` read as on,
/// which is not what the publisher said.
pub(super) fn beacon_is_on(record: &ContentRecord, on_values: &[String]) -> bool {
    let candidate = match &record.state {
        Some(state) => state.clone(),
        None => value_text(&record.value),
    };
    on_values
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(candidate.trim()))
}

/// Renders a pushed JSON value as display text.
pub(super) fn value_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "\u{2014}".to_owned(),
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

/// Truncates to a character count, so a hostile device id cannot blow up the
/// placeholder's layout.
pub(super) fn truncate(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    text.chars().take(chars).collect::<String>() + "\u{2026}"
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use time::Duration;

    use crate::telemetry::Telemetry;

    use super::super::body::body_nodes;
    use super::super::types::Space;

    use super::super::test_support::*;
    use super::*;

    #[test]
    fn a_stale_widget_renders_differently_from_a_fresh_one() {
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let device = device(vec![w]);

        let mut fresh = HashMap::new();
        fresh.insert("a".to_owned(), record(serde_json::json!(42), now()));
        let mut stale = HashMap::new();
        stale.insert(
            "a".to_owned(),
            record(serde_json::json!(42), now() - Duration::seconds(3_600)),
        );

        assert_ne!(
            render(&device, &fresh),
            render(&device, &stale),
            "a stale widget must not render its last value as though current"
        );
    }

    #[test]
    fn is_stale_reports_the_staleness_window_correctly() {
        // stale_after == 0 disables the timer even for an ancient record.
        let w = widget("a", WidgetKind::Value, 0, 0);
        assert_eq!(w.stale_after, 0);
        let ancient = record(serde_json::json!(1), now() - Duration::days(400));
        assert!(!is_stale(&w, &ancient, now()));

        // The window triggers strictly after it closes, not at the boundary.
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let at_limit = record(serde_json::json!(1), now() - Duration::seconds(60));
        assert!(
            !is_stale(&w, &at_limit, now()),
            "exactly at the window is still fresh"
        );
        let past = record(serde_json::json!(1), now() - Duration::seconds(61));
        assert!(is_stale(&w, &past, now()));

        // A publisher's clock is irrelevant here because we stamp receipt
        // ourselves, but a clock step backwards on this host must not read as a
        // negative age.
        let ahead = record(serde_json::json!(1), now() + Duration::seconds(500));
        assert!(!is_stale(&w, &ahead, now()));
    }

    #[test]
    fn a_stale_push_keeps_its_value_and_is_marked_not_confirmed() {
        // The whole point of holding a value: the last thing a publisher said is
        // still the best answer the panel has, so it stays on the glass, muted,
        // rather than being replaced by a countdown that says nothing.
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let device = device(vec![w.clone()]);
        let mut content = HashMap::new();
        content.insert(
            "a".to_owned(),
            record(serde_json::json!(42), now() - Duration::seconds(3_600)),
        );

        let cell = resolve(
            &w,
            &RenderInputs {
                device: &device,
                content: &content,
                ha_states: &HashMap::new(),
                icons: &HashMap::new(),
                trends: &NO_TRENDS,
                now: now(),
                telemetry: &Telemetry::default(),
            },
        );
        assert_eq!(
            cell,
            Cell {
                body: Body::Figure {
                    text: "42".to_owned(),
                    unit: None,
                    trend: None,
                },
                ink: Ink::Held,
            }
        );
    }

    #[test]
    fn beacon_is_on_resolves_state_over_value_case_and_space_insensitively() {
        let on_values = vec!["on".to_owned(), "alert".to_owned()];

        // State decides when present.
        let mut alerting = record(serde_json::json!("off"), now());
        alerting.state = Some("alert".to_owned());
        assert!(
            beacon_is_on(&alerting, &on_values),
            "state decides when present"
        );

        // A non-matching state means off. Falling through to `value` would report
        // on for a publisher that explicitly said it was idle.
        let mut contradictory = record(serde_json::json!("on"), now());
        contradictory.state = Some("idle".to_owned());
        assert!(!beacon_is_on(&contradictory, &on_values));

        // With no state pushed, the value decides — for strings and booleans alike.
        let on_values = vec!["on".to_owned(), "true".to_owned()];
        assert!(beacon_is_on(
            &record(serde_json::json!("on"), now()),
            &on_values
        ));
        assert!(beacon_is_on(
            &record(serde_json::json!(true), now()),
            &on_values
        ));
        assert!(!beacon_is_on(
            &record(serde_json::json!("off"), now()),
            &on_values
        ));
        assert!(!beacon_is_on(
            &record(serde_json::json!(false), now()),
            &on_values
        ));

        // Matching ignores case and surrounding whitespace.
        let on_values = vec!["on".to_owned()];
        assert!(beacon_is_on(
            &record(serde_json::json!(" ON "), now()),
            &on_values
        ));
    }

    #[test]
    fn renders_every_value_shape() {
        assert_eq!(value_text(&serde_json::json!("text")), "text");
        assert_eq!(value_text(&serde_json::json!(7)), "7");
        assert_eq!(value_text(&serde_json::json!(1.5)), "1.5");
        assert_eq!(value_text(&serde_json::json!(true)), "true");
        assert_eq!(value_text(&serde_json::Value::Null), "\u{2014}");
    }

    #[test]
    fn rows_replace_the_scalar_value() {
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let mut with_rows = HashMap::new();
        let mut rec = record(serde_json::json!("ignored"), now());
        rec.rows = Some(vec![
            Row {
                id: Some("one".to_owned()),
                label: Some("One".to_owned()),
                value: Some(serde_json::json!(1)),
                unit: Some("C".to_owned()),
                state: None,
            },
            Row {
                id: None,
                label: Some("Two".to_owned()),
                value: Some(serde_json::json!(2)),
                unit: None,
                state: None,
            },
        ]);
        with_rows.insert("a".to_owned(), rec);

        let mut scalar = HashMap::new();
        scalar.insert("a".to_owned(), record(serde_json::json!("ignored"), now()));

        assert_ne!(render(&device, &with_rows), render(&device, &scalar));
        assert_eq!(dimensions(&render(&device, &with_rows)), (400, 300));
    }

    #[test]
    fn a_home_assistant_failure_holds_the_last_value_rather_than_blanking_the_cell() {
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        let device = device(vec![w.clone()]);
        let reading = Reading::state("sensor.office");

        let held = HashMap::from([(reading.clone(), Reported::Held("21.4".to_owned()))]);
        let fresh = HashMap::from([(reading, Reported::Fresh("21.4".to_owned()))]);

        let muted_frame = render_with(&device, &HashMap::new(), &held, &HashMap::new());
        let live_frame = render_with(&device, &HashMap::new(), &fresh, &HashMap::new());

        assert_eq!(dimensions(&muted_frame), (400, 300));
        assert_ne!(
            muted_frame, live_frame,
            "a held value must be visibly distinct from a confirmed one"
        );
    }

    #[test]
    fn a_held_reading_keeps_its_value_and_a_lost_one_says_so() {
        // The distinction that matters: "the request failed but I know what it said
        // last" is a muted figure, whereas "nothing has ever been read" is an
        // absence. Collapsing both to the word `unavailable`, as this once did,
        // threw away the reading a viewer actually wanted.
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        let reading = Reading::state("sensor.office");

        assert_eq!(
            resolved(
                &w,
                &HashMap::from([(reading.clone(), Reported::Held("21.4".to_owned()))])
            ),
            Cell {
                body: Body::Figure {
                    text: "21.4".to_owned(),
                    unit: None,
                    trend: None,
                },
                ink: Ink::Held,
            }
        );
        assert_eq!(
            resolved(
                &w,
                &HashMap::from([(reading.clone(), Reported::Fresh("21.4".to_owned()))])
            ),
            Cell {
                body: Body::Figure {
                    text: "21.4".to_owned(),
                    unit: None,
                    trend: None,
                },
                ink: Ink::Current,
            }
        );
        // Lost carries no mark: `Ink::Current` here is not "this is fresh" but "there
        // is nothing for the mark to qualify". The absence is drawn muted either way.
        assert_eq!(
            resolved(&w, &HashMap::from([(reading, Reported::Lost)])),
            Cell {
                body: Body::Absent("no data"),
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_reading_that_was_never_fetched_reads_as_no_data() {
        // A missing key means the caller never asked, which for a validated config
        // means Home Assistant is not configured at all.
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        assert_eq!(
            resolved(&w, &HashMap::new()),
            Cell {
                body: Body::Absent("no data"),
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_weather_cell_draws_a_condition_as_an_icon_and_a_name() {
        // `partlycloudy` in the tabular-numeric figure style was the defect: a word
        // from a closed set put where a number goes.
        let w = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        let reading = Reading::state("weather.braga");

        let cell = resolved(
            &w,
            &HashMap::from([(reading.clone(), Reported::Fresh("partlycloudy".to_owned()))]),
        );
        assert_eq!(
            cell,
            Cell {
                body: Body::Sky {
                    svg: icon::Condition::PartlyCloudy.svg(),
                    condition: Some("Partly cloudy".to_owned()),
                    rows: Vec::new(),
                },
                ink: Ink::Current,
            }
        );

        // An unrecognised slug still shows what Home Assistant said, because a new
        // condition is a thing to notice rather than to hide.
        let unknown = resolved(
            &w,
            &HashMap::from([(reading, Reported::Fresh("meteor-shower".to_owned()))]),
        );
        assert_eq!(
            unknown,
            Cell {
                body: Body::Sky {
                    svg: icon::UNKNOWN_SKY,
                    condition: Some("meteor-shower".to_owned()),
                    rows: Vec::new(),
                },
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_reading_fills_the_cell_it_is_given() {
        // The defect this pins: every type size was a fraction of the cell capped at
        // a pixel count chosen against the 400x300 test device, and fitting only ever
        // shrank to width. On the real panel a 2x2 weather cell is 699x688 and held a
        // 316px glyph with 34px words — under half of each axis — while a figure cell
        // used 40% of its height. Both axes are asserted because either one alone
        // passes with the cell half empty.
        let mut weather = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        weather.col_span = 2;
        weather.row_span = 2;
        let mut figure = widget("temp", WidgetKind::Value, 2, 0);
        figure.unit = Some("\u{b0}C".to_owned());
        let device = panel(vec![weather, figure]);

        let content =
            HashMap::from([("temp".to_owned(), record(serde_json::json!("23.4"), now()))]);
        let ha = HashMap::from([(
            Reading::state("weather.braga"),
            Reported::Fresh("partlycloudy".to_owned()),
        )]);

        let (sky_w, sky_h) = cell_fill(&device, "sky", &content, &ha);
        assert!(
            sky_w > 0.7 && sky_h > 0.7,
            "the weather block must use its cell: {sky_w:.2} of the width, {sky_h:.2} \
             of the height"
        );

        let (figure_w, figure_h) = cell_fill(&device, "temp", &content, &ha);
        assert!(
            figure_w > 0.9,
            "a figure is width-bound on this grid, so it must use nearly all of it: \
             {figure_w:.2}"
        );
        // Not higher, and this is the honest limit of the fix: a four-glyph reading
        // with a unit is width-bound on a 350x344 cell, so the leftover height is
        // structural. Buying it back means a wider cell — a grid choice, not a
        // rendering one.
        assert!(
            figure_h > 0.55,
            "and it must still use over half the height it is given: {figure_h:.2}"
        );
    }

    #[test]
    fn an_ha_entity_ignores_pushed_content() {
        // The kind reads from Home Assistant, so a push to the same id must not
        // masquerade as the entity's state.
        let w = ha_widget("temp", "sensor.office", WidgetKind::HaEntity);
        let device = device(vec![w.clone()]);
        let mut content = HashMap::new();
        content.insert("temp".to_owned(), record(serde_json::json!("99"), now()));
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("21.4".to_owned()),
        )]);

        assert_eq!(
            resolve(
                &w,
                &RenderInputs {
                    device: &device,
                    content: &content,
                    ha_states: &ha,
                    icons: &HashMap::new(),
                    trends: &NO_TRENDS,
                    now: now(),
                    telemetry: &Telemetry::default(),
                }
            ),
            Cell {
                body: Body::Figure {
                    text: "21.4".to_owned(),
                    unit: None,
                    trend: None,
                },
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn truncates_an_over_long_device_id() {
        assert_eq!(truncate("short", 64), "short");
        assert_eq!(truncate("abcdef", 3), "abc\u{2026}");
    }

    #[test]
    fn precision_rounds_a_number_and_leaves_anything_else_alone() {
        // One formatter for every reading on the dashboard, so the same `precision`
        // cannot come to mean two things in two cells.
        assert_eq!(format_reading("21.456", None), "21.456");
        assert_eq!(format_reading("21.456", Some(1)), "21.5");
        assert_eq!(format_reading("21.456", Some(0)), "21");
        // Padded when the source carries fewer digits than were asked for, so a
        // value walking between 21 and 21.05 does not change width under the eye.
        assert_eq!(format_reading("21", Some(2)), "21.00");
        assert_eq!(format_reading("-3.75", Some(1)), "-3.8");
        assert_eq!(format_reading(" 4.2 ", Some(1)), "4.2");

        // The fallthrough that matters: a Home Assistant state is a string that
        // merely happens to be numeric most of the time, and every one of these
        // reaches the formatter on a cell whose author set a precision.
        assert_eq!(format_reading("partlycloudy", Some(1)), "partlycloudy");
        assert_eq!(format_reading("unavailable", Some(2)), "unavailable");
        assert_eq!(
            format_reading("23.4 \u{b0}C", Some(1)),
            "23.4 \u{b0}C",
            "a value carrying its own unit is not a number"
        );
        assert_eq!(format_reading("", Some(2)), "");
    }

    #[test]
    fn a_configured_precision_reaches_both_reading_paths() {
        // Pushed and read are two functions, and a formatter applied in only one of
        // them is a dashboard where `precision` works on some cells and not others.
        let pushed = Widget {
            precision: Some(1),
            ..widget("a", WidgetKind::Value, 0, 0)
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("21.456"), now()))]);
        assert_eq!(
            resolved_push(&pushed, &content, &HashMap::new()).body,
            Body::Figure {
                text: "21.5".to_owned(),
                unit: None,
                trend: None,
            }
        );

        let read = Widget {
            precision: Some(1),
            ..ha_widget("temp", "sensor.office", WidgetKind::HaEntity)
        };
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("21.456".to_owned()),
        )]);
        assert_eq!(
            resolved(&read, &ha).body,
            Body::Figure {
                text: "21.5".to_owned(),
                unit: None,
                trend: None,
            }
        );
    }

    #[test]
    fn a_weather_cell_can_stand_without_its_condition_in_words() {
        let captioned = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        let bare = Widget {
            state_text: false,
            ..captioned.clone()
        };
        let ha = HashMap::from([(
            Reading::state("weather.braga"),
            Reported::Fresh("partlycloudy".to_owned()),
        )]);

        assert_eq!(
            resolved(&captioned, &ha).body,
            Body::Sky {
                svg: icon::Condition::PartlyCloudy.svg(),
                condition: Some("Partly cloudy".to_owned()),
                rows: Vec::new(),
            }
        );
        assert_eq!(
            resolved(&bare, &ha).body,
            Body::Sky {
                svg: icon::Condition::PartlyCloudy.svg(),
                condition: None,
                rows: Vec::new(),
            },
            "the glyph is the reading, so it stands on its own"
        );
    }

    #[test]
    fn a_beacon_draws_its_state_icon_and_falls_back_to_the_dot_without_one() {
        // Configuring `icon_on` alone is legal, which is the point of the fallback:
        // the on state gets its picture and the off state stays the hollow dot,
        // rather than the cell losing its indicator half the time.
        let w = Widget {
            icon_on: Some("mdi-lightbulb-on".to_owned()),
            ..widget("a", WidgetKind::Beacon, 0, 0)
        };
        let bulb = Icon::Svg {
            markup: r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="currentColor"><circle cx="12" cy="12" r="9"/></svg>"#
                .to_owned(),
            ink: None,
        };
        let icons = HashMap::from([("mdi-lightbulb-on".to_owned(), bulb.clone())]);
        let on = HashMap::from([("a".to_owned(), record(serde_json::json!("on"), now()))]);
        let off = HashMap::from([("a".to_owned(), record(serde_json::json!("off"), now()))]);

        assert_eq!(
            resolved_push(&w, &on, &icons).body,
            Body::Beacon {
                on: true,
                icon: Some(bulb),
                text: true,
            }
        );
        assert_eq!(
            resolved_push(&w, &off, &icons).body,
            Body::Beacon {
                on: false,
                icon: None,
                text: true,
            },
            "the off state configured no icon, so it stays a dot"
        );
        // A spec the icon store could not resolve falls back the same way: an
        // unreachable icon host must not cost the cell its indicator.
        assert_eq!(
            resolved_push(&w, &on, &HashMap::new()).body,
            Body::Beacon {
                on: true,
                icon: None,
                text: true,
            }
        );
    }

    #[test]
    fn a_weather_cell_carries_its_own_readings_beside_the_condition() {
        // The attributes a `weather.*` entity's state does not carry: the condition is
        // a word, and the temperature and humidity hang off the same entity.
        let w = Widget {
            readings: vec![
                reading("Temp", "weather.braga", Some("temperature")),
                crate::config::Reading {
                    unit: Some("%".to_owned()),
                    precision: Some(0),
                    ..reading("Humidity", "weather.braga", Some("humidity"))
                },
            ],
            ..ha_widget("sky", "weather.braga", WidgetKind::Weather)
        };
        let ha = HashMap::from([
            (
                Reading::state("weather.braga"),
                Reported::Fresh("sunny".to_owned()),
            ),
            (
                Reading::attribute("weather.braga", "temperature"),
                Reported::Fresh("23.456".to_owned()),
            ),
            (
                Reading::attribute("weather.braga", "humidity"),
                Reported::Fresh("61.8".to_owned()),
            ),
        ]);

        assert_eq!(
            resolved(&w, &ha),
            Cell {
                body: Body::Sky {
                    svg: icon::Condition::Sunny.svg(),
                    condition: Some("Sunny".to_owned()),
                    rows: vec![
                        resolved_line(
                            "Temp",
                            serde_json::json!("23.5"),
                            Some("\u{b0}C"),
                            Ink::Current
                        ),
                        resolved_line("Humidity", serde_json::json!("62"), Some("%"), Ink::Current),
                    ],
                },
                ink: Ink::Current,
            }
        );
    }

    #[test]
    fn a_list_mutes_only_the_reading_it_could_not_get() {
        // The whole point of a per-line ink: one unreachable sensor must not take the
        // two working readings beside it off the glass.
        let w = Widget {
            readings: vec![
                reading("Office", "sensor.office", None),
                reading("Hall", "sensor.hall", None),
                reading("Shed", "sensor.shed", None),
            ],
            ..widget("rooms", WidgetKind::List, 0, 0)
        };
        let ha = HashMap::from([
            (
                Reading::state("sensor.office"),
                Reported::Fresh("21.44".to_owned()),
            ),
            (
                Reading::state("sensor.hall"),
                Reported::Fresh("19.0".to_owned()),
            ),
            (Reading::state("sensor.shed"), Reported::Lost),
        ]);

        assert_eq!(
            resolved(&w, &ha),
            Cell {
                body: Body::Rows(vec![
                    resolved_line(
                        "Office",
                        serde_json::json!("21.4"),
                        Some("\u{b0}C"),
                        Ink::Current
                    ),
                    resolved_line(
                        "Hall",
                        serde_json::json!("19.0"),
                        Some("\u{b0}C"),
                        Ink::Current
                    ),
                    // The lost reading keeps its label and its place. Its value is
                    // null, which renders as an em dash, and its unit goes with the
                    // value it qualified — `— °C` would claim a reading in degrees
                    // that nobody has.
                    resolved_line("Shed", serde_json::Value::Null, None, Ink::Held),
                ]),
                // Marked, because one line in this cell is not confirmed current.
                ink: Ink::Held,
            }
        );
    }

    #[test]
    fn a_list_renders_a_frame_with_a_reading_it_could_not_get() {
        // The resolved cell says the right thing; this says the em-dash path actually
        // rasterises, which is the half a `Cell` assertion cannot reach.
        let w = Widget {
            readings: vec![
                reading("Office", "sensor.office", None),
                reading("Shed", "sensor.shed", None),
            ],
            ..widget("rooms", WidgetKind::List, 0, 0)
        };
        let device = device(vec![w]);
        let partial = HashMap::from([
            (
                Reading::state("sensor.office"),
                Reported::Fresh("21.4".to_owned()),
            ),
            (Reading::state("sensor.shed"), Reported::Lost),
        ]);
        let whole = HashMap::from([
            (
                Reading::state("sensor.office"),
                Reported::Fresh("21.4".to_owned()),
            ),
            (
                Reading::state("sensor.shed"),
                Reported::Fresh("8.2".to_owned()),
            ),
        ]);

        let degraded = render_with(&device, &HashMap::new(), &partial, &HashMap::new());
        assert_eq!(dimensions(&degraded), (400, 300));
        assert_ne!(
            degraded,
            render_with(&device, &HashMap::new(), &whole, &HashMap::new()),
            "a reading that could not be got must be visibly distinct from one that could"
        );
    }

    /// A list that declares no reading is fed by push, which is the only way a
    /// shopping list gets onto a ticket: nobody writes one into a config file.
    #[test]
    fn a_list_with_no_reading_draws_the_rows_it_was_pushed() {
        let list = widget("items", WidgetKind::List, 0, 0);
        let sent = HashMap::from([("items".to_owned(), rows_record(2))]);

        assert_eq!(
            resolved_push(&list, &sent, &HashMap::new()),
            Cell {
                body: Body::Rows(vec![
                    resolved_line("item 0", serde_json::json!("0"), None, Ink::Current),
                    resolved_line("item 1", serde_json::json!("1"), None, Ink::Current),
                ]),
                ink: Ink::Current,
            },
            "a pushed list's rows are its body"
        );

        // Nothing pushed yet, and a push that was not a list, are both named rather
        // than drawn as an empty table: a cell that quietly showed the scalar would
        // leave an author wondering where the other rows went.
        assert_eq!(
            resolved_push(&list, &HashMap::new(), &HashMap::new()).body,
            Body::Absent("no data")
        );
        let scalar =
            HashMap::from([("items".to_owned(), record(serde_json::json!("21.4"), now()))]);
        assert_eq!(
            resolved_push(&list, &scalar, &HashMap::new()).body,
            Body::Absent("no rows")
        );
    }

    #[test]
    fn a_group_resolves_to_a_marker_with_no_body_of_its_own() {
        // The body is deliberately empty: `cell_node` draws a group's children, and a
        // body node here would take a share of the box they are laid out in.
        let w = widget("box", WidgetKind::Group, 0, 0);
        assert_eq!(
            resolved(&w, &HashMap::new()),
            Cell {
                body: Body::Group,
                ink: Ink::Current,
            }
        );
        assert!(
            body_nodes(
                &FONTS,
                &Body::Group,
                Ink::Current,
                Space {
                    width: 200.0,
                    height: 200.0,
                    label_px: 14.0,
                    style: &STYLE,
                },
            )
            .is_empty(),
            "a group's cell has no body of its own to draw"
        );
    }

    #[test]
    fn a_list_reading_held_keeps_its_value_and_mutes() {
        // The `Held` branch of a per-reading line: a sensor that once answered and
        // then went quiet still shows what it last said, muted rather than blanked
        // — the list's version of `a_stale_push_keeps_its_value_and_is_marked_not_confirmed`.
        let w = Widget {
            readings: vec![reading("Office", "sensor.office", None)],
            ..widget("rooms", WidgetKind::List, 0, 0)
        };
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Held("21.44".to_owned()),
        )]);
        assert_eq!(
            resolved(&w, &ha),
            Cell {
                body: Body::Rows(vec![resolved_line(
                    "Office",
                    serde_json::json!("21.4"),
                    Some("\u{b0}C"),
                    Ink::Held
                )]),
                ink: Ink::Held,
            }
        );
    }

    #[test]
    fn value_text_renders_a_pushed_array_or_object_with_its_json_text() {
        // The catch-all: a publisher that pushes an array or object where a
        // scalar was expected still gets *something* legible rather than a panic.
        assert_eq!(value_text(&serde_json::json!([1, 2])), "[1,2]");
        assert_eq!(value_text(&serde_json::json!({"a": 1})), "{\"a\":1}");
    }

    /// A `value` cell and an `ha_entity` cell, both asking for a trend at whole
    /// numbers — the shape [`crate::config::validate_widget`] produces for
    /// `trend = true`.
    fn trending_device() -> Device {
        let pushed = Widget {
            trend: true,
            precision: Some(0),
            ..widget("pushed", WidgetKind::Value, 0, 0)
        };
        let read = Widget {
            trend: true,
            precision: Some(0),
            ..ha_widget("read", "sensor.office", WidgetKind::HaEntity)
        };
        let listed = Widget {
            readings: vec![crate::config::Reading {
                trend: true,
                precision: Some(0),
                ..reading("Office", "sensor.office", None)
            }],
            ..widget("listed", WidgetKind::List, 1, 0)
        };
        device(vec![pushed, read, listed])
    }

    #[test]
    fn shown_numbers_reports_what_each_trending_cell_will_print() {
        let device = trending_device();
        let content = HashMap::from([(
            "pushed".to_owned(),
            record(serde_json::json!("21.4"), now()),
        )]);
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("18.6".to_owned()),
        )]);

        let shown: HashMap<String, f64> =
            shown_numbers(&device, &content, &ha).into_iter().collect();
        assert_eq!(
            shown,
            HashMap::from([
                ("kindle/pushed".to_owned(), 21.0),
                ("kindle/read".to_owned(), 19.0),
                ("kindle/listed#0".to_owned(), 19.0),
            ]),
            "each number is the rounded one the cell prints, not the raw reading"
        );
    }

    #[test]
    fn shown_numbers_skips_a_reading_that_is_not_a_number() {
        // `unavailable` has no direction, and inventing one would put an arrow on a
        // cell that is not reporting anything.
        let device = trending_device();
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("unavailable".to_owned()),
        )]);
        assert!(shown_numbers(&device, &HashMap::new(), &ha).is_empty());
    }

    #[test]
    fn shown_numbers_reads_a_held_value_too() {
        // The cell is showing that number, so it is the number the arrow describes.
        let device = trending_device();
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Held("18.6".to_owned()),
        )]);
        let shown = shown_numbers(&device, &HashMap::new(), &ha);
        assert!(
            shown.contains(&("kindle/read".to_owned(), 19.0)),
            "{shown:?}"
        );
    }

    #[test]
    fn a_cell_that_asked_for_no_trend_contributes_no_key_and_draws_no_arrow() {
        let w = ha_widget("plain", "sensor.office", WidgetKind::HaEntity);
        let device = device(vec![w.clone()]);
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("21.4".to_owned()),
        )]);
        assert!(shown_numbers(&device, &HashMap::new(), &ha).is_empty());

        // And even handed a direction under its key, it draws none: the flag is
        // what decides, so a stale key left in the file cannot mark a cell.
        let trends = HashMap::from([("kindle/plain".to_owned(), Trend::Up)]);
        let cell = resolve(
            &w,
            &RenderInputs {
                device: &device,
                content: &HashMap::new(),
                ha_states: &ha,
                icons: &HashMap::new(),
                trends: &trends,
                now: now(),
                telemetry: &Telemetry::default(),
            },
        );
        assert_eq!(
            cell.body,
            Body::Figure {
                text: "21.4".to_owned(),
                unit: None,
                trend: None,
            }
        );
    }

    #[test]
    fn a_trending_figure_and_row_carry_the_direction_under_their_own_key() {
        let device = trending_device();
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("18.6".to_owned()),
        )]);
        let trends = HashMap::from([
            ("kindle/read".to_owned(), Trend::Up),
            ("kindle/listed#0".to_owned(), Trend::Down),
        ]);
        let inputs = RenderInputs {
            device: &device,
            content: &HashMap::new(),
            ha_states: &ha,
            icons: &HashMap::new(),
            trends: &trends,
            now: now(),
            telemetry: &Telemetry::default(),
        };

        let figure = resolve(&device.widgets[1], &inputs).body;
        assert_eq!(
            figure,
            Body::Figure {
                text: "19".to_owned(),
                unit: None,
                trend: Some(Trend::Up),
            }
        );

        let Body::Rows(rows) = resolve(&device.widgets[2], &inputs).body else {
            panic!("a list resolves to rows");
        };
        assert_eq!(rows[0].trend, Some(Trend::Down));
    }

    #[test]
    fn a_trend_arrow_reaches_the_glass_and_moves_the_frame_only_with_the_reading() {
        let w = Widget {
            trend: true,
            precision: Some(0),
            unit: Some("\u{b0}C".to_owned()),
            ..ha_widget("temp", "sensor.office", WidgetKind::HaEntity)
        };
        let device = panel(vec![w]);
        let ha = HashMap::from([(
            Reading::state("sensor.office"),
            Reported::Fresh("18.6".to_owned()),
        )]);

        let frame = |trend: Trend| {
            let trends = HashMap::from([("kindle/temp".to_owned(), trend)]);
            super::super::render_frame(
                &FONTS,
                RenderInputs {
                    device: &device,
                    content: &HashMap::new(),
                    ha_states: &ha,
                    icons: &HashMap::new(),
                    trends: &trends,
                    now: now(),
                    telemetry: &Telemetry::default(),
                },
            )
            .expect("frame should render")
        };

        let steady = frame(Trend::Steady);
        let up = frame(Trend::Up);
        let down = frame(Trend::Down);
        assert_ne!(steady, up, "the arrow is actually drawn");
        assert_ne!(up, down, "and the three marks are told apart on the glass");

        // The property the feature rests on: the frame is a function of the
        // direction, so a reading that has not moved keeps the same bytes and the
        // panel does not repaint. `state::Trends::step` is what holds the direction
        // still between changes.
        assert_eq!(up, frame(Trend::Up));
    }
}
