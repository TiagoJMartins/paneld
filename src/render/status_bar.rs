//! The status bar: a strip along one edge of the frame, outside the widget grid.
//!
//! Almost everything the bar can say is something this process already knows for
//! certain — the clock it was handed, the device's own configuration, the last
//! telemetry it reported. The exception is an [`Alert`], which reads pushed
//! content, and it reads it from [`RenderInputs`] like every other resolved input,
//! so [`node`] stays as pure as the rest of the render pipeline.
//!
//! The strip's geometry is not decided here. [`Device::status_bar_area`] owns it,
//! because the grid beside it is sized by the same arithmetic and the tap hit test
//! resolves against that: a bar that derived its own thickness would be one
//! rounding away from covering a row of cells that taps still fire.

use takumi::prelude::*;
use time::{Month, OffsetDateTime, Weekday};

use super::{
    MIN_TYPE_PX, NUMERIC_FAMILY, RenderInputs, UI_FAMILY, fitted, muted, one_line, rule,
    rule_width, text_node, text_style,
};
use crate::config::{Alert, Device, Edge, StatusBar, StatusField};
use crate::telemetry::Telemetry;

/// The type size a bar's thickness affords, as a fraction of it.
///
/// Half, so that a line of text sits inside the strip with as much air above and
/// below it as the glyphs are tall. Derived from the thickness rather than fixed
/// because the thickness is the only thing an author configures about a bar's
/// proportions, and a constant here would ignore them.
const TYPE_SCALE: f32 = 0.5;

/// The margin at each end of the strip, as a fraction of the type size.
const MARGIN_SCALE: f32 = 0.6;

/// The space between two things in the bar, as a fraction of the type size.
///
/// Fixed spacing rather than an even spread across the strip, because the fields
/// are a row of chrome and not a set of columns: spread evenly, adding a field
/// moved every other field, and a bar showing two things put one of them in the
/// middle of the panel where it read as a caption for the cell above it.
const GAP_SCALE: f32 = 1.4;

/// What a field with nothing to report says.
///
/// An em dash, and never a zero. A panel that has not polled yet has no battery
/// reading, and rendering that as `0%` is not merely wrong but actionable: it
/// reads as a flat panel, which is a thing an owner gets up and goes to deal with.
const ABSENT: &str = "—";

/// The status bar strip, sized to the rect the grid was shrunk to make room for.
///
/// The one entry point: `dashboard_node` decides which side of the grid this sits
/// on and nothing else about it.
pub fn node(fonts: &Fonts, device: &Device, bar: &StatusBar, inputs: &RenderInputs<'_>) -> Node {
    let Some((_, _, width, height)) = device.status_bar_area() else {
        // Unreachable for a bar taken off this device, and an empty strip is a
        // cheaper thing to say than a panic: a frame is what the owner sees, and
        // losing the clock off it beats losing the dashboard with it.
        return Node::container(Vec::new());
    };

    let horizontal = matches!(bar.edge, Edge::Top | Edge::Bottom);
    let size = (bar.thickness as f32 * TYPE_SCALE).max(MIN_TYPE_PX);
    let margin = size * MARGIN_SCALE;
    let gap = size * GAP_SCALE;
    // The axis the fields are spread along, and the one the strip is thick in.
    // Parenthesised: `as` binds tighter than the `if`, so the cast has to be told
    // it applies to whichever arm was taken rather than to the last one.
    let along = (if horizontal { width } else { height }) as f32;
    let across = (if horizontal { height } else { width }) as f32;

    // How wide one run may be set. The fields sit at fixed spacing rather than
    // spread across the strip, so what any one of them may take is the strip's
    // length less the margins and the gaps, shared equally. A vertical bar stacks
    // its contents instead, so each has the whole width.
    let count = (bar.fields.len() + bar.alerts.len()).max(1) as f32;
    let available = if horizontal {
        ((along - margin * 2.0 - gap * (count - 1.0)) / count).max(1.0)
    } else {
        across
    };

    let fields = bar
        .fields
        .iter()
        .map(|&field| {
            field_node(
                fonts,
                &field_text(field, device, bar, inputs.now, inputs.telemetry),
                field,
                available,
                size,
            )
        })
        .collect::<Vec<_>>();

    // Only the alerts that are actually up. An alert that is not firing contributes
    // no node at all, which is the point of it: nothing on the panel moves when one
    // appears or goes, because the fields are anchored to the leading edge and the
    // alerts to the trailing one.
    let alerts = bar
        .alerts
        .iter()
        .filter(|alert| is_raised(alert, inputs))
        .map(|alert| alert_node(fonts, alert, inputs, available, size))
        .collect::<Vec<_>>();

    let direction = if horizontal {
        FlexDirection::Row
    } else {
        FlexDirection::Column
    };

    // Two groups rather than one flat row, so that `space-between` puts the fields
    // at the leading edge and the alerts at the trailing one. Flat, the alerts
    // would sit wherever the fields happened to end.
    let children = vec![group(fields, direction, gap), group(alerts, direction, gap)];

    let mut style = Style::default()
        .with(StyleDeclaration::display(Display::Flex))
        .with(StyleDeclaration::flex_direction(direction))
        .with(StyleDeclaration::justify_content(
            JustifyContent::SpaceBetween,
        ))
        .with(StyleDeclaration::align_items(AlignItems::Center))
        .with(StyleDeclaration::width(Length::Px(width as f32)))
        .with(StyleDeclaration::height(Length::Px(height as f32)))
        // Never shrunk to make room for the grid: the two are already sized to
        // partition the frame exactly, so shrinking either would draw the bar over
        // cells the hit test still resolves taps to.
        .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0))));

    // Padding on the strip's length only. Its thickness is already the type's line
    // box plus air, so padding across it would squeeze the text out of a bar an
    // author sized deliberately.
    style = if horizontal {
        style
            .with(StyleDeclaration::padding_left(Length::Px(margin)))
            .with(StyleDeclaration::padding_right(Length::Px(margin)))
    } else {
        style
            .with(StyleDeclaration::padding_top(Length::Px(margin)))
            .with(StyleDeclaration::padding_bottom(Length::Px(margin)))
    };
    for declaration in separator(bar.edge) {
        style = style.with(declaration);
    }

    Node::container(children).with_style(style)
}

/// One end of the bar: its contents in a row at fixed spacing, sized to them.
fn group(children: Vec<Node>, direction: FlexDirection, gap: f32) -> Node {
    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(direction))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(gap))))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap)))),
    )
}

/// Whether an alert is up: something was pushed, it reads as on, and it has been
/// confirmed recently enough to believe.
///
/// The staleness rule is the opposite of a cell's, deliberately. A cell keeps its
/// last reading and mutes it, because the reading is what the cell is for. An alert
/// that nothing has confirmed within its window disappears, because an unconfirmed
/// alert is a publisher that died with its hand up — and a panel that cried wolf
/// until someone restarted a script is a panel nobody trusts.
fn is_raised(alert: &Alert, inputs: &RenderInputs<'_>) -> bool {
    let Some(record) = inputs.content.get(&alert.id) else {
        return false;
    };
    if alert.stale_after > 0 && super::is_stale_after(alert.stale_after, record, inputs.now) {
        return false;
    }
    super::beacon_is_on(record, &alert.on_values)
}

/// One raised alert: its glyph, and its words when it was given any.
fn alert_node(
    fonts: &Fonts,
    alert: &Alert,
    inputs: &RenderInputs<'_>,
    available: f32,
    size: f32,
) -> Node {
    let mut children = Vec::new();
    if let Some(icon) = alert
        .icon
        .as_ref()
        .and_then(|spec| inputs.icons.get(spec.as_str()))
    {
        // Drawn in full ink, not the bar's grey: the rest of the strip is chrome
        // that is always there, and this is the one thing on it that means
        // something happened.
        children.push(super::icon_node(icon, size, super::Ink::Current));
    }
    if let Some(label) = &alert.label {
        children.push(fitted(fonts, available, size, |size| {
            text_node(
                label,
                one_line(
                    text_style(size, 700.0, UI_FAMILY).with(StyleDeclaration::color(super::ink())),
                ),
            )
        }));
    }

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                size * 0.4,
            )))),
    )
}

/// The rule between the bar and the dashboard, on the one side of the strip that
/// faces it.
///
/// That side and no other: the bar's remaining three sides are the frame's own
/// edges, and a line drawn along those reads as a border around the whole panel
/// rather than as a strip set apart from the grid.
fn separator(edge: Edge) -> [StyleDeclaration; 3] {
    match edge {
        Edge::Top => [
            StyleDeclaration::border_bottom_width(rule_width(1.0)),
            StyleDeclaration::border_bottom_style(BorderStyle::Solid),
            StyleDeclaration::border_bottom_color(rule()),
        ],
        Edge::Bottom => [
            StyleDeclaration::border_top_width(rule_width(1.0)),
            StyleDeclaration::border_top_style(BorderStyle::Solid),
            StyleDeclaration::border_top_color(rule()),
        ],
        Edge::Left => [
            StyleDeclaration::border_right_width(rule_width(1.0)),
            StyleDeclaration::border_right_style(BorderStyle::Solid),
            StyleDeclaration::border_right_color(rule()),
        ],
        Edge::Right => [
            StyleDeclaration::border_left_width(rule_width(1.0)),
            StyleDeclaration::border_left_style(BorderStyle::Solid),
            StyleDeclaration::border_left_color(rule()),
        ],
    }
}

/// One field's run, set in the secondary grey at the largest size that fits the
/// room it has.
///
/// Held to one line, which is not a nicety: the strip is a fixed number of pixels
/// thick, and a run that wrapped would be laid out taller than that and push the
/// bar out of the shape the grid was shrunk to sit beside.
fn field_node(fonts: &Fonts, text: &str, field: StatusField, available: f32, design: f32) -> Node {
    fitted(fonts, available, design, |size| {
        text_node(
            text,
            one_line(text_style(size, 400.0, face(field)).with(StyleDeclaration::color(muted()))),
        )
    })
}

/// The face one field is set in.
///
/// Tabular figures for everything numeric, and the clock is the case that earns
/// them: the fields are spread evenly along the strip, so a proportional face
/// setting `11:11` narrower than `08:48` would nudge every other field along with
/// every tick. A device id is a name rather than a number, so it takes the UI face.
fn face(field: StatusField) -> &'static str {
    match field {
        StatusField::Device => UI_FAMILY,
        _ => NUMERIC_FAMILY,
    }
}

/// What one field says.
///
/// Takes the two live values a field can come from rather than the whole
/// [`RenderInputs`], because a bar reads no pushed content, no Home Assistant
/// state and no icon — and a signature that said otherwise would invite one to.
fn field_text(
    field: StatusField,
    device: &Device,
    bar: &StatusBar,
    now: OffsetDateTime,
    telemetry: &Telemetry,
) -> String {
    match field {
        StatusField::Date => date_text(bar.timezone.at(now)),
        StatusField::Time => time_text(bar.timezone.at(now)),
        StatusField::Battery => match telemetry.battery_percent {
            Some(percent) => format!("{}%", percent.round() as i64),
            None => ABSENT.to_owned(),
        },
        StatusField::Refresh => period_text(device.refresh_rate),
        StatusField::Device => device.id.clone(),
        StatusField::Signal => signal_text(telemetry.rssi),
    }
}

/// The date as `Wed 20 Aug`.
///
/// Day before month, both abbreviated, and no year: a wall panel is glanced at,
/// and nobody has ever needed a clock to tell them which year it is.
///
/// Assembled from the date's own components rather than through a format
/// description. A description is parsed at run time and can therefore fail at run
/// time, which on a panel is a field that silently stops saying anything.
fn date_text(at: OffsetDateTime) -> String {
    format!(
        "{} {} {}",
        weekday_name(at.weekday()),
        at.day(),
        month_name(at.month())
    )
}

/// The time as 24-hour `HH:MM`.
///
/// Know what a clock costs before configuring one, because it is the one field
/// that changes on its own: every frame differs from the last, so the panel
/// repaints on every render interval instead of only when a reading changed. On a
/// battery panel that is the difference between a refresh the owner asked for and
/// one they pay for.
///
/// Zero-padded and never 12-hour: a fixed four digits is the same width whatever
/// the hour, so the fields either side of it do not shuffle as the day goes on.
fn time_text(at: OffsetDateTime) -> String {
    format!("{:02}:{:02}", at.hour(), at.minute())
}

/// A count of seconds as the shortest exact period, e.g. `5m`, `90s`, `1h`.
///
/// Exact rather than rounded, and that is why it steps down rather than always
/// using the largest unit: `90s` is the truth about a 90-second refresh where
/// `1m` and `2m` are both a lie about it.
fn period_text(seconds: u32) -> String {
    if seconds.is_multiple_of(3_600) {
        return format!("{}h", seconds / 3_600);
    }
    if seconds.is_multiple_of(60) {
        return format!("{}m", seconds / 60);
    }
    format!("{seconds}s")
}

/// Signal strength in dBm, or an em dash when the device has not reported any.
///
/// Worth knowing before configuring this field: the KOReader client in service
/// hardcodes `rssi` to `0` as an unfinished TODO, so on that panel this reads
/// `0 dBm` — a present reading of nothing rather than an absent one, which this
/// function cannot tell apart and must not pretend to.
fn signal_text(rssi: Option<i64>) -> String {
    match rssi {
        Some(dbm) => format!("{dbm} dBm"),
        None => ABSENT.to_owned(),
    }
}

/// A weekday's three-letter abbreviation.
///
/// A table rather than `Weekday`'s own `Display`, which spells the day out in
/// full: `Wednesday` is three times the width of the widest of these, and a bar
/// sizes its type to fit its widest field.
fn weekday_name(day: Weekday) -> &'static str {
    match day {
        Weekday::Monday => "Mon",
        Weekday::Tuesday => "Tue",
        Weekday::Wednesday => "Wed",
        Weekday::Thursday => "Thu",
        Weekday::Friday => "Fri",
        Weekday::Saturday => "Sat",
        Weekday::Sunday => "Sun",
    }
}

/// A month's three-letter abbreviation, for the same reason as [`weekday_name`].
fn month_name(month: Month) -> &'static str {
    match month {
        Month::January => "Jan",
        Month::February => "Feb",
        Month::March => "Mar",
        Month::April => "Apr",
        Month::May => "May",
        Month::June => "Jun",
        Month::July => "Jul",
        Month::August => "Aug",
        Month::September => "Sep",
        Month::October => "Oct",
        Month::November => "Nov",
        Month::December => "Dec",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::ContentRecord;
    use std::collections::HashMap;

    /// A bar in UTC. Spelt as a config fragment, and every fixture below goes
    /// through `parse`, so a field this module reads is one a file can really set.
    const UTC_BAR: &str = r#"edge = "bottom"
fields = ["time"]"#;

    /// The same bar in a zone that is two hours east at the instant below, where it
    /// is already the next day. Helsinki is on EET in November, so this is a
    /// standard-time offset rather than a summer one.
    const HELSINKI_BAR: &str = r#"edge = "bottom"
fields = ["time"]
timezone = "Europe/Helsinki""#;

    /// Five hours west: New York on EST, again standard time in November.
    const NEW_YORK_BAR: &str = r#"edge = "bottom"
fields = ["time"]
timezone = "America/New_York""#;

    /// A zone whose offset changes, for the transition test.
    const LISBON_BAR: &str = r#"edge = "bottom"
fields = ["time"]
timezone = "Europe/Lisbon""#;

    /// 2023-11-14T22:13:20Z — late enough in the day that a positive offset lands
    /// on the next date, which is the case a naive clock gets wrong.
    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("a valid instant")
    }

    fn device(bar: &str) -> Device {
        let text = format!(
            r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 1024
height = 758
palette = "gray16"
dither = "atkinson"
refresh_rate = 300
grid = {{ cols = 4, rows = 3 }}

[device.status_bar]
{bar}
"#
        );
        let mut config = crate::config::parse(&text).expect("the fixture must be valid");
        config.devices.remove(0)
    }

    /// What one field renders to on a device configured by `bar`.
    fn text(bar: &str, field: StatusField, telemetry: &Telemetry) -> String {
        let device = device(bar);
        let status_bar = device
            .status_bar
            .as_ref()
            .expect("the fixture configures a bar");
        field_text(field, &device, status_bar, now(), telemetry)
    }

    fn nothing_reported() -> Telemetry {
        Telemetry::default()
    }

    #[test]
    fn date_reads_as_weekday_day_and_short_month() {
        assert_eq!(
            text(UTC_BAR, StatusField::Date, &nothing_reported()),
            "Tue 14 Nov"
        );
    }

    #[test]
    fn time_reads_as_zero_padded_twenty_four_hour() {
        assert_eq!(
            text(UTC_BAR, StatusField::Time, &nothing_reported()),
            "22:13"
        );
    }

    #[test]
    fn a_zone_east_of_utc_shifts_the_clock_past_midnight() {
        // At this instant a panel in Helsinki is already on the next day, and a bar
        // that rendered UTC would show yesterday's date beside tonight's time.
        assert_eq!(
            text(HELSINKI_BAR, StatusField::Time, &nothing_reported()),
            "00:13"
        );
        assert_eq!(
            text(HELSINKI_BAR, StatusField::Date, &nothing_reported()),
            "Wed 15 Nov"
        );
    }

    #[test]
    fn a_zone_west_of_utc_shifts_the_clock_back_within_the_day() {
        assert_eq!(
            text(NEW_YORK_BAR, StatusField::Time, &nothing_reported()),
            "17:13"
        );
        assert_eq!(
            text(NEW_YORK_BAR, StatusField::Date, &nothing_reported()),
            "Tue 14 Nov"
        );
    }

    #[test]
    fn the_clock_follows_its_zone_across_a_daylight_saving_transition() {
        // The reason the bar takes a zone and not an offset. One configuration, one
        // panel, two answers — and neither of them is anything the author has to
        // remember to edit in spring and autumn.
        let device = device(LISBON_BAR);
        let bar = device
            .status_bar
            .as_ref()
            .expect("the fixture configures a bar");
        let telemetry = nothing_reported();

        // 2026-01-15T12:00:00Z, when Lisbon is on WET, and 2026-07-15T12:00:00Z,
        // when it is on WEST.
        let winter = OffsetDateTime::from_unix_timestamp(1_768_478_400).expect("valid");
        let summer = OffsetDateTime::from_unix_timestamp(1_784_116_800).expect("valid");

        assert_eq!(
            field_text(StatusField::Time, &device, bar, winter, &telemetry),
            "12:00",
            "WET is UTC itself"
        );
        assert_eq!(
            field_text(StatusField::Time, &device, bar, summer, &telemetry),
            "13:00",
            "WEST is an hour ahead, and the database is what knows that"
        );
    }

    #[test]
    fn battery_reads_as_a_whole_percentage() {
        let telemetry = Telemetry {
            battery_percent: Some(83.6),
            ..Telemetry::default()
        };
        assert_eq!(text(UTC_BAR, StatusField::Battery, &telemetry), "84%");
    }

    #[test]
    fn an_unreported_battery_reads_as_an_em_dash_and_never_a_zero() {
        // A panel that has not polled yet is not a flat panel, and `0%` is the one
        // rendering of that an owner would get up and act on.
        assert_eq!(
            text(UTC_BAR, StatusField::Battery, &nothing_reported()),
            ABSENT
        );
    }

    #[test]
    fn refresh_reads_as_a_period() {
        assert_eq!(
            text(UTC_BAR, StatusField::Refresh, &nothing_reported()),
            "5m",
            "the fixture's 300 second refresh_rate"
        );
    }

    #[test]
    fn a_period_uses_the_largest_unit_it_is_exact_in() {
        assert_eq!(period_text(45), "45s");
        assert_eq!(period_text(90), "90s", "90 seconds is not a whole minute");
        assert_eq!(period_text(300), "5m");
        assert_eq!(period_text(3_600), "1h");
        assert_eq!(period_text(5_400), "90m", "an hour and a half is not hours");
        assert_eq!(period_text(86_400), "24h");
    }

    #[test]
    fn device_reads_as_the_device_id() {
        assert_eq!(
            text(UTC_BAR, StatusField::Device, &nothing_reported()),
            "kindle"
        );
    }

    #[test]
    fn signal_reads_as_dbm() {
        let telemetry = Telemetry {
            rssi: Some(-58),
            ..Telemetry::default()
        };
        assert_eq!(text(UTC_BAR, StatusField::Signal, &telemetry), "-58 dBm");
    }

    #[test]
    fn an_unreported_signal_reads_as_an_em_dash() {
        assert_eq!(
            text(UTC_BAR, StatusField::Signal, &nothing_reported()),
            ABSENT
        );
    }

    #[test]
    fn numbers_are_set_in_the_tabular_face_and_the_device_id_is_not() {
        // Not cosmetic: a proportional digit width would move the fields beside a
        // figure whenever it changed.
        assert_eq!(face(StatusField::Time), NUMERIC_FAMILY);
        assert_eq!(face(StatusField::Battery), NUMERIC_FAMILY);
        assert_eq!(face(StatusField::Device), UI_FAMILY);
    }

    /// A bar carrying one alert, and a content store in the state `pushed` names.
    fn alerting(pushed: Option<(&str, i64)>) -> (Device, HashMap<String, ContentRecord>) {
        let device = device(
            r#"edge = "bottom"
fields = ["date"]

[[device.status_bar.alert]]
id = "slack_unread"
icon = "si-slack"
stale_after = 3600"#,
        );
        let content = match pushed {
            Some((state, age)) => HashMap::from([(
                "slack_unread".to_owned(),
                ContentRecord {
                    value: serde_json::Value::String(state.to_owned()),
                    state: Some(state.to_owned()),
                    unit: None,
                    rows: None,
                    received_at: now() - time::Duration::seconds(age),
                },
            )]),
            None => HashMap::new(),
        };
        (device, content)
    }

    /// Whether the bar's one alert is up, given the pushed state.
    fn raised(pushed: Option<(&str, i64)>) -> bool {
        let (device, content) = alerting(pushed);
        let bar = device.status_bar.as_ref().expect("a bar");
        let icons = HashMap::new();
        let ha = HashMap::new();
        let telemetry = Telemetry::default();
        let inputs = RenderInputs {
            device: &device,
            content: &content,
            ha_states: &ha,
            icons: &icons,
            telemetry: &telemetry,
            now: now(),
        };
        is_raised(&bar.alerts[0], &inputs)
    }

    #[test]
    fn an_alert_is_up_only_while_something_says_so() {
        assert!(raised(Some(("alert", 0))), "a fresh on reads as up");
        assert!(!raised(None), "nothing pushed is not an alert");
        assert!(!raised(Some(("clear", 0))), "an off push clears it");
    }

    #[test]
    fn a_stale_alert_withdraws_instead_of_muting() {
        // The opposite of a cell, and deliberately: a cell keeps its last reading
        // because the reading is what it is for, where an alert nobody has confirmed
        // lately is a publisher that died with its hand up. A panel that cried wolf
        // until someone restarted a script is a panel nobody reads.
        assert!(raised(Some(("alert", 60))), "within the window it stands");
        assert!(
            !raised(Some(("alert", 7_200))),
            "past the window it withdraws"
        );
    }
}
