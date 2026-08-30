//! What a cell's content wants vertically, before the grid has decided what it
//! gets.
//!
//! One question, asked by one caller: [`super::Layout`] under
//! [`crate::config::Fit::Content`] has to size a row track before anything has
//! been laid out, let alone measured. That ordering is the whole constraint on
//! this module. The renderer sizes a cell's type from the box its track gave it,
//! so a track sized from a measurement would be a track sized from itself — which
//! is why nothing here measures anything: every number is arithmetic over the
//! configuration, the style table, and one fact about the data — how many rows a
//! cell has been pushed.
//!
//! That one fact is admitted because it cannot be declined. A pushed list's length
//! is not configuration and no arithmetic recovers it: charged as though it were
//! one line, a ten-item list is drawn over whatever sits beneath it, and the
//! cell's own type is then fitted to a box a tenth of the size it needed. The
//! alternative is asking an author to declare how many rows their publisher will
//! send and to keep the two in step forever.
//!
//! Every estimate is deliberately an over-estimate. The renderer shaves a run that
//! will not fit the width it was given, and shaving can only make a line shorter,
//! so a track a few pixels too tall costs a dashboard nothing but air. A track a
//! pixel too short clips a reading, and a clipped reading is the worst failure this
//! panel has: it looks like a value rather than like an error.

use std::collections::HashMap;

use crate::config::{self, Device, Group, Style, Widget, WidgetKind};
use crate::content::ContentRecord;

/// The pushed content a track is sized against, keyed by widget id.
///
/// The content store's own snapshot, borrowed, rather than a count projected out of
/// it: the row count is all geometry reads, and a second map keyed by the same ids
/// would be one more thing to keep in step with the first.
pub type Pushed<'a> = &'a HashMap<String, ContentRecord>;

/// The line box the layout engine gives a run, as a multiple of its type size.
///
/// Measured against the faces this binary embeds rather than read off their
/// metrics: what a track has to hold is the box takumi lays a run out in, and a
/// face's declared ascent and descent do not add up to that box. The measurement is
/// 1.21 at the sizes a panel sets a reading in and 1.25 at the legibility floor,
/// where the engine's rounding of a line box up to a whole pixel is a larger share
/// of a small size. This is the looser of those with the same rounding applied
/// again, because a line box guessed a pixel short is a reading with its descenders
/// cut off.
///
/// Pinned as a constant rather than measured per call because measuring is what
/// this module exists to avoid, and because the number is a property of the
/// embedded faces, which change only when the binary does.
///
/// Read by the renderer too, and it has to be: a column drawn at a pitch this
/// module did not charge for is a column drawn past the end of its track.
pub const LINE_BOX: f32 = 1.29;

/// The least air the renderer leaves between two readings, as a multiple of their
/// type size.
///
/// A floor rather than the gap itself: a cell with height to spare spreads its
/// readings further apart, and a cell sized to its content is by definition the case
/// with none to spare. So this is the pitch a column of readings is charged at, and
/// no track sized by it is too short for the column that lands on it.
pub const ROW_GAP_FLOOR: f32 = 0.28;

/// The height this widget's content wants, in pixels, at the width it will get.
///
/// `cell_w` is the width the grid will hand the cell. No reading's height is derived
/// from it here — the renderer fits type to whichever axis binds, and this estimate
/// takes the height axis as though it always did — but a group's children are
/// measured across their own sub-cells, so the width has to travel down.
pub fn natural_height(device: &Device, widget: &Widget, cell_w: f32, pushed: Pushed<'_>) -> f32 {
    let label_px = chrome_type(device, widget.style);
    header(widget, label_px)
        + body(device, widget, cell_w, label_px, pushed)
        + device.chrome.inset()
}

/// A cell's chrome type under content fit, in pixels.
///
/// The rule that breaks the circularity, and the reason it is public rather than
/// private to this estimate: the renderer applies the same one, and two copies of it
/// would be a cell whose label is sized against a track that was sized against a
/// different label. Taken from the panel's short side and floored at the style's
/// minimum, with no cell-height cap — a cap is what makes the label a function of
/// the track, and the track is already a function of the label.
pub fn chrome_type(device: &Device, style: Style) -> f32 {
    (device.width.min(device.height) as f32 * style.chrome_scale * style.type_scale)
        .max(style.min_type)
}

/// Raises every track a widget covers to the share of its height that widget wants.
///
/// A spanning widget is charged to each track it covers rather than to the first,
/// and at `natural_height / row_span` rather than in full. That is an approximation,
/// and it is the right one. Charging the first track in full leaves the rest free to
/// be short, so the run of tracks a two-row cell is actually laid out against can
/// still be shorter than the cell; charging each of them in full instead means
/// solving a system in which every track's height depends on every span crossing it.
/// A share each keeps the *sum* of the tracks a span covers at or above what that
/// span asked for, which is the only quantity a spanning cell is ever measured
/// against.
///
/// Shared with a group's own sub-grid, which sizes its rows by this same rule
/// against the same kind of widget list.
pub fn raise_tracks(
    device: &Device,
    widgets: &[Widget],
    cell_w: f32,
    tracks: &mut [f32],
    pushed: Pushed<'_>,
) {
    for widget in widgets {
        let span = widget.row_span.max(1);
        let share = natural_height(device, widget, cell_w, pushed) / span as f32;
        // Clamped rather than indexed: a widget placed outside its grid is a config
        // error that validation rejects, and an arithmetic panic inside a request
        // handler is a worse way to report one than a track nobody raised.
        let start = (widget.row as usize).min(tracks.len());
        let end = start.saturating_add(span as usize).min(tracks.len());
        for track in &mut tracks[start..end] {
            *track = track.max(share);
        }
    }
}

/// What a cell's header costs it, in pixels, and `0.0` for a cell that has none.
///
/// The condition is the renderer's own, down to the untitled group, and it matters
/// most in the case it looks wrong in: a leaf cell with neither label nor icon is
/// charged too, because the mark saying a value is not confirmed current is drawn in
/// that header and is a line tall. Whether it appears is not configuration — it
/// appears when a sensor stops answering — so a track sized without it is a track
/// that clips the first time Home Assistant restarts.
fn header(widget: &Widget, label_px: f32) -> f32 {
    if widget.group.is_some() && widget.label.is_none() && widget.icon.is_none() {
        return 0.0;
    }
    label_px * LINE_BOX + header_gap(label_px)
}

/// The gap between a cell's header and the body under it, in pixels.
///
/// A substitution, and worth naming as one. The renderer takes this gap as a
/// fraction of the cell's own height, clamped to a few pixels either side, and the
/// cell's height is exactly what this module is computing. So it is taken from the
/// label instead — the label is itself a fraction of the panel, which is what the
/// cell height was standing in for — and wherever the clamp binds, which is every
/// panel this has run on, the two produce the same number. Where they differ at all
/// they differ by a pixel or two of air, which a track can afford in a way it can
/// never afford a pixel of clipping.
fn header_gap(label_px: f32) -> f32 {
    (label_px * 0.6).clamp(2.0, 8.0)
}

/// What a cell holds below its header, by kind.
///
/// Every kind is named rather than swept into a wildcard, so adding one is a compile
/// error here rather than a track sized as though the new cell were empty.
fn body(device: &Device, widget: &Widget, cell_w: f32, label_px: f32, pushed: Pushed<'_>) -> f32 {
    // Ahead of the kind, because `rows` overrides a pushed cell's own body whatever
    // its kind is, and the charge has to follow what the renderer will draw rather
    // than what the author declared. A `value` cell whose publisher sends rows is a
    // list on the glass, and a track charged one figure for it is a list drawn over
    // the cell below.
    if let Some(count) = pushed_rows(widget, pushed) {
        return readings(count, label_px, widget.style);
    }

    match widget.kind {
        WidgetKind::List => readings(widget.readings.len(), label_px, widget.style),

        WidgetKind::Weather => sky(widget, label_px),

        // A figure, an entity's state and a beacon's indicator are each one thing
        // filling the cell under its header, so each is charged one line of it.
        //
        // A `text` cell is charged the same, and a pushed cell waiting for its first
        // push is too: `no data` is one line, and a list that has not arrived yet
        // cannot be charged for the rows it will bring. What makes one line the right
        // charge for prose rather than a hopeful one is that the renderer wraps that
        // content to whatever box it is given and elides the rest, so a long push is
        // a bounded paragraph and never an overflow.
        WidgetKind::Value | WidgetKind::HaEntity | WidgetKind::Beacon | WidgetKind::Text => {
            figure(label_px, widget.style)
        }

        // A group reads nothing of its own, so its height is its children's. The
        // recursion is exactly one level deep because config validation rejects a
        // group inside a group, and a `group` kind always carries its sub-grid for
        // the same reason — which is why its absence is answered with nothing rather
        // than with a panic inside a request handler.
        WidgetKind::Group => widget
            .group
            .as_ref()
            .map_or(0.0, |group| group_height(device, group, cell_w, pushed)),
    }
}

/// How many rows this cell has been pushed, and `None` for a cell drawn from
/// anything else.
///
/// Gated on [`Widget::fed_by_push`] rather than on the record's existence alone: a
/// list that names its readings reads Home Assistant and ignores whatever was pushed
/// to its id, so charging a track for those rows would reserve paper for a column
/// nothing draws. An empty `rows` is `None` for the same reason — the cell draws
/// `no data`, which is one line.
fn pushed_rows(widget: &Widget, pushed: Pushed<'_>) -> Option<usize> {
    if !widget.fed_by_push() {
        return None;
    }
    let rows = pushed.get(&widget.id)?.rows.as_ref()?;
    (!rows.is_empty()).then_some(rows.len())
}

/// One large reading's line, in pixels.
///
/// Set at the style's ceiling over the chrome naming it, which is the ceiling the
/// renderer holds a column of readings to. A single figure has no such ceiling there
/// — it fills whatever box it is handed — and that is precisely why it needs one
/// here: a cell asking to be as tall as the figure it would grow to fill is a cell
/// asking for the whole frame.
fn figure(label_px: f32, style: Style) -> f32 {
    label_px * style.reading_ceiling * LINE_BOX
}

/// The size one row of a multi-reading cell is set at when nothing shrinks it, in
/// pixels.
///
/// One rule, read by the renderer as well as by this estimate, because a track
/// charged at one size and drawn at another is either a clipped reading or a strip
/// of paper nobody asked for. `row_type` pins it outright; unpinned it is the
/// ceiling the renderer holds a fitted column to, which is what such a column comes
/// out at on a track this module sized from that same ceiling.
pub fn row_ceiling(label_px: f32, style: Style) -> f32 {
    match style.row_type > 0.0 {
        true => style.row_type,
        false => label_px * style.reading_ceiling,
    }
}

/// A column of `count` labelled readings, in pixels.
///
/// At the tightest pitch the renderer will produce for them: the type is
/// [`row_ceiling`], and the air between two rows has a floor of its own that no
/// shortage of height goes under. Both are per row, so `count` of them is the
/// column.
///
/// The last row is charged for a gap it never draws. One row's worth of air at the
/// foot of a column costs a dashboard nothing, where subtracting it would make this
/// estimate exactly as tall as the thing it estimates — which is the one thing it
/// must not be.
fn readings(count: usize, label_px: f32, style: Style) -> f32 {
    let row_px = row_ceiling(label_px, style);
    count as f32 * (row_px * LINE_BOX + row_px * ROW_GAP_FLOOR)
}

/// A weather cell's body: the condition's glyph, and the readings hung off it.
///
/// The glyph is charged as a share of the whole rather than as a height of its own,
/// because it has no height of its own — it is a picture fitted to whatever box it is
/// given. `style.glyph_share` is the fraction of a split box the renderer hands it,
/// so the readings' height *is* the rest of that box, and dividing by the rest is
/// that same statement read backwards.
///
/// Charged as though the split were always vertical, which is the taller of the two
/// arrangements the renderer picks between: it sets the glyph beside the readings in
/// a cell wider than it is tall, and whether this cell is wider than it is tall is
/// not knowable from a height this function has not finished computing.
///
/// A cell with no readings is charged one line, exactly as a single figure is. There
/// is nothing there to take a share of, and a picture with no intrinsic size of its
/// own still has to be given a box that can be read across a room.
fn sky(widget: &Widget, label_px: f32) -> f32 {
    if widget.readings.is_empty() {
        return figure(label_px, widget.style);
    }
    // Never a division by nothing: config validation bounds `glyph_share` to 0.98,
    // where the readings keep a fiftieth of the box and this multiplies their height
    // by fifty.
    readings(widget.readings.len(), label_px, widget.style) / (1.0 - widget.style.glyph_share)
}

/// A group's height: its children's, row by row.
///
/// The children are laid out on equal tracks inside the group's content box, so what
/// the group wants is the tallest child of each of those tracks, summed, plus the
/// gaps the renderer leaves between them — `n - 1` of them for `n` tracks, because
/// the group's own padding is what holds the outermost children off its frame.
fn group_height(device: &Device, group: &Group, cell_w: f32, pushed: Pushed<'_>) -> f32 {
    let content_w = (cell_w - device.chrome.inset()).max(1.0);
    // The width a child gets, read off the shared sub-cell arithmetic rather than
    // restated here. The height half of that answer is discarded, because the height
    // is what this function is computing; the box height handed over in its place is
    // the box width, which the width it answers with never reads.
    let (child_w, _) = config::sub_cell_size(content_w, content_w, group.grid, device.chrome);

    let mut tracks = vec![0.0; group.grid.rows.max(1) as usize];
    raise_tracks(device, &group.widgets, child_w, &mut tracks, pushed);
    tracks.iter().sum::<f32>() + device.chrome.gap * (tracks.len() - 1) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Chrome, Dither, Fit, Grid, Palette, Reading};

    /// A dashboard nobody has pushed to.
    ///
    /// What most of these estimates are about: a track sized from the configuration
    /// alone. The tests that are about a push build a record of their own.
    fn nothing_pushed() -> HashMap<String, ContentRecord> {
        HashMap::new()
    }

    /// One pushed record of `count` rows, stored under `id`.
    fn pushed(id: &str, count: usize) -> HashMap<String, ContentRecord> {
        let rows = (0..count)
            .map(|index| crate::content::Row {
                id: None,
                label: Some(format!("item {index}")),
                value: Some(serde_json::json!(index)),
                unit: None,
                state: None,
            })
            .collect();
        HashMap::from([(
            id.to_owned(),
            ContentRecord {
                value: serde_json::Value::Null,
                state: None,
                unit: None,
                rows: Some(rows),
                received_at: time::OffsetDateTime::UNIX_EPOCH,
            },
        )])
    }

    /// A widget of `kind` with everything an estimate does not read left empty.
    fn widget(id: &str, kind: WidgetKind) -> Widget {
        Widget {
            id: id.to_owned(),
            kind,
            col: 0,
            row: 0,
            col_span: 1,
            trend: false,
            row_span: 1,
            label: None,
            unit: None,
            precision: None,
            state_text: true,
            stale_after: 0,
            entity: None,
            attribute: None,
            on_values: Vec::new(),
            icon: None,
            icon_on: None,
            icon_off: None,
            fill: false,
            style: Style::default(),
            readings: Vec::new(),
            group: None,
            tap: None,
        }
    }

    /// A cell of `count` readings, which is what makes one list taller than another.
    fn listed(id: &str, kind: WidgetKind, count: usize) -> Widget {
        Widget {
            readings: (0..count)
                .map(|index| Reading {
                    label: Some(format!("reading {index}")),
                    icon: None,
                    entity: format!("sensor.reading_{index}"),
                    attribute: None,
                    unit: None,
                    precision: None,
                    trend: false,
                })
                .collect(),
            ..widget(id, kind)
        }
    }

    /// The panel in service: a Kindle Paperwhite in landscape, with the spacing a
    /// configuration that says nothing about `chrome` derives for it.
    fn device(widgets: Vec<Widget>) -> Device {
        let grid = Grid {
            cols: 2,
            rows: 2,
            fit: Fit::Content,
        };
        Device {
            id: "kindle".to_owned(),
            width: 1448,
            height: 1072,
            palette: Palette::Gray16,
            dither: Dither::Bayer,
            refresh_rate: 300,
            render_interval: 300,
            max_frame_bytes: 0,
            grid,
            chrome: Chrome::derived(1448, 1072, grid),
            style: Style::default(),
            status_bar: None,
            widgets,
            sink: None,
        }
    }

    /// Sub-pixel agreement, which is all a track needs: half a pixel of drift cannot
    /// move a reading out of a cell forty pixels tall.
    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 0.01
    }

    /// Two more readings cost exactly two more readings, which is what makes a
    /// content-fit grid hand two lists of different lengths different tracks.
    #[test]
    fn a_longer_list_wants_more_height_than_a_shorter_one() {
        let two = listed("two", WidgetKind::List, 2);
        let four = listed("four", WidgetKind::List, 4);
        let panel = device(vec![two.clone(), four.clone()]);

        let short = natural_height(&panel, &two, 700.0, &nothing_pushed());
        let tall = natural_height(&panel, &four, 700.0, &nothing_pushed());
        assert!(
            tall > short,
            "four readings must want more height than two, not {tall} against {short}"
        );

        let style = Style::default();
        let pitch = chrome_type(&panel, style) * style.reading_ceiling * (LINE_BOX + ROW_GAP_FLOOR);
        assert!(
            close(tall - short, pitch * 2.0),
            "the difference must be two readings' pitch ({}), not {}",
            pitch * 2.0,
            tall - short
        );
    }

    /// A weather cell is its readings *and* a picture of the sky, and the picture is
    /// what a track sized to the readings alone would clip.
    #[test]
    fn a_weather_cell_wants_more_than_its_readings_alone() {
        let sky = listed("sky", WidgetKind::Weather, 2);
        let rows = listed("rows", WidgetKind::List, 2);
        let panel = device(vec![sky.clone(), rows.clone()]);

        let with_glyph = natural_height(&panel, &sky, 700.0, &nothing_pushed());
        let without = natural_height(&panel, &rows, 700.0, &nothing_pushed());
        assert!(
            with_glyph > without,
            "the condition's glyph must cost the cell something, but {with_glyph} is \
             no more than {without}"
        );

        // And it costs exactly the share of the body the renderer gives it: the
        // readings keep the rest, so the two together are their own height over that
        // rest.
        let style = Style::default();
        let column = readings(2, chrome_type(&panel, style), style);
        assert!(
            close(
                with_glyph - without,
                column / (1.0 - style.glyph_share) - column
            ),
            "the glyph must cost the readings' height over the share left to them, \
             not {}",
            with_glyph - without
        );

        // A cell with no readings at all still wants a box for its picture.
        let bare = widget("bare", WidgetKind::Weather);
        assert!(
            natural_height(&panel, &bare, 700.0, &nothing_pushed()) > panel.chrome.inset(),
            "a weather cell with nothing hung off it still has a sky to draw"
        );
    }

    /// A group is its children, row by row — and the child deciding a row is the
    /// tallest one on it, not the first or the last.
    #[test]
    fn a_groups_height_is_its_tallest_child_of_each_row_summed() {
        let sub = Grid {
            cols: 2,
            rows: 2,
            fit: Fit::Stretch,
        };
        let tall = listed("tall", WidgetKind::List, 4);
        let short = widget("short", WidgetKind::Value);
        let middling = listed("middling", WidgetKind::List, 2);

        let mut host = widget("group", WidgetKind::Group);
        host.group = Some(Group {
            grid: sub,
            widgets: vec![
                Widget {
                    col: 0,
                    row: 0,
                    ..tall.clone()
                },
                Widget {
                    col: 1,
                    row: 0,
                    ..short.clone()
                },
                Widget {
                    col: 0,
                    row: 1,
                    ..middling.clone()
                },
            ],
        });
        let panel = device(vec![host.clone()]);

        let cell_w = 700.0;
        let content_w = cell_w - panel.chrome.inset();
        let (child_w, _) = config::sub_cell_size(content_w, content_w, sub, panel.chrome);
        let first = natural_height(&panel, &tall, child_w, &nothing_pushed());
        let second = natural_height(&panel, &middling, child_w, &nothing_pushed());

        assert!(
            first > natural_height(&panel, &short, child_w, &nothing_pushed()),
            "the fixture's first row must be decided by its list rather than by the \
             figure sharing it, or the test proves nothing"
        );
        assert!(
            close(
                natural_height(&panel, &host, cell_w, &nothing_pushed()),
                first + second + panel.chrome.gap + panel.chrome.inset()
            ),
            "a group's height is its two rows, the gap between them and its own \
             chrome, not {}",
            natural_height(&panel, &host, cell_w, &nothing_pushed())
        );
    }

    /// The whole reason the content store reaches the layout: a pushed list's length
    /// is not in the configuration, and a track charged one line for a ten-item list
    /// is a list drawn over the cell beneath it.
    #[test]
    fn a_pushed_lists_track_is_charged_for_what_was_pushed() {
        // No readings declared, so its rows arrive by push.
        let list = widget("items", WidgetKind::List);
        let panel = device(vec![list.clone()]);
        let style = Style::default();

        let waiting = natural_height(&panel, &list, 700.0, &nothing_pushed());
        let three = natural_height(&panel, &list, 700.0, &pushed("items", 3));
        let ten = natural_height(&panel, &list, 700.0, &pushed("items", 10));

        assert!(
            three > waiting,
            "a list that has arrived wants more than the `no data` line it replaced: \
             {three} against {waiting}"
        );
        assert!(
            close(ten - three, readings(7, chrome_type(&panel, style), style)),
            "seven more rows must cost exactly seven rows' pitch, not {}",
            ten - three
        );

        // A record pushed to a list that declared its readings is not charged for.
        // The renderer reads that cell from Home Assistant and ignores the push, and a
        // track sized for rows nothing draws is a strip of blank paper.
        let declared = listed("items", WidgetKind::List, 2);
        assert!(
            close(
                natural_height(&panel, &declared, 700.0, &pushed("items", 10)),
                natural_height(&panel, &declared, 700.0, &nothing_pushed())
            ),
            "a Home Assistant list must be charged for its readings and nothing else"
        );
    }

    /// `row_type` is what makes a ticket's length a function of its content: pinned,
    /// a row costs what it was pinned to whatever panel it is on.
    #[test]
    fn a_pinned_row_is_charged_the_size_it_was_pinned_to() {
        let mut pinned_list = widget("items", WidgetKind::List);
        pinned_list.style.row_type = 20.0;
        let panel = device(vec![pinned_list.clone()]);

        let charged = natural_height(&panel, &pinned_list, 700.0, &pushed("items", 10));
        let column = 10.0 * 20.0 * (LINE_BOX + ROW_GAP_FLOOR);
        assert!(
            close(
                charged,
                header(&pinned_list, chrome_type(&panel, pinned_list.style))
                    + column
                    + panel.chrome.inset()
            ),
            "a pinned list is its header, ten pinned rows and its own chrome, not {charged}"
        );

        // And the pin decides it rather than the panel: unpinned, the same ten rows
        // are charged the chrome's ceiling, which on a panel this size is far larger.
        let fitted = widget("items", WidgetKind::List);
        assert!(
            natural_height(&panel, &fitted, 700.0, &pushed("items", 10)) > charged * 2.0,
            "the shipped ceiling on a 1072px-high panel must dwarf a 20px pin"
        );
    }
}
