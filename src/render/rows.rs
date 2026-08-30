//! A column of labelled rows filling a box: the sizing arithmetic shared by a
//! `list` body and a weather cell's readings, and the row-count bookkeeping
//! [`super::natural`] mirrors when it charges a track for the same column.

use takumi::prelude::*;

use super::body::{fit_size, intrinsic_width};
use super::natural;
use super::paint::{row_node, row_runs, rule_width};
use super::types::{Greys, Line, Space, rule};

/// How much of a cell's width the widest reading is grown to occupy.
///
/// Not all of it: a column set to the full width has its figures hard against the
/// cell's rule, and the eye needs the margin to see the column as a block. Four
/// fifths reads as filled.
const COLUMN_TARGET: f32 = 0.80;

/// How much wider than its widest row a column of readings is drawn.
///
/// A little slack so the figures do not sit hard against the glyphs naming them,
/// and no more: the column is centred, so every pixel of width beyond this is a
/// void down the middle of the cell rather than margin around it.
const COLUMN_SLACK: f32 = 1.15;

/// The most space between two readings, as a multiple of their type size.
///
/// Bounds the air rather than sharing out whatever the type ceiling left over. A
/// tall cell has more height than four readings need, and distributing all of it
/// put the readings so far apart that they stopped reading as one table.
const ROW_GAP_CEILING: f32 = 1.1;

/// A column of labelled rows filling a box, each row inked by its own trust.
///
/// Shared by a `list` body and by a weather cell's readings, because they are one
/// thing: `n` labelled values sized and aligned to a box together.
///
/// Three keys of a cell's [`crate::config::Style`] decide whether that column reads
/// as a block of figures or as a ruled table: `row_width` puts a line's name and its
/// figure at opposite edges of the cell instead of close together, `row_rule` draws a
/// hairline under each one, and `row_fill` carries those rules on to the foot of the
/// cell so a short table reads as a form rather than as a hole. All three default
/// off, which is the block.
pub(super) fn rows_node(fonts: &Fonts, rows: &[Line], space: Space) -> Node {
    let Space {
        width,
        height,
        label_px,
        style,
    } = space;
    let greys = space.greys();
    let pinned = style.row_type > 0.0;
    // Three things bound a reading's size, and the smallest wins.
    //
    // The height, because the rows are laid out down the box. The width, because a
    // column that filled a third of a wide cell and centred itself left the rest as
    // a margin nobody asked for — so the type grows until the widest row occupies
    // `COLUMN_TARGET` of the width, which is what makes a cell look filled rather
    // than furnished. And the ceiling, because past it a table stops being a table.
    let by_height = height / block(rows.len());
    let by_width = width_driven_size(fonts, rows, width, greys);
    let ceiling = natural::row_ceiling(label_px, *style);
    // A pinned row is fitted to nothing, and that is the whole of what pinning
    // means: the box stops being a constraint on the type and becomes a consequence
    // of it. A panel wants the other direction — every pixel of the frame spent, so
    // a short list in a tall cell grows to meet it — but a roll has no height to
    // fill, only a length that is spent, so on paper the type is a decision about
    // the reader and the length is whatever the content came to.
    let design = match pinned {
        true => ceiling,
        false => by_height.min(by_width).min(ceiling).max(style.min_type),
    };
    // Shaved against the real measurement at the size actually chosen: the estimate
    // above is linear in the type size, which is exact for a text run and slightly
    // out once a glyph's own margins are in the row. A pinned row is shaved too — a
    // pin is a request for a size, not a licence to print past the paper's edge.
    let row_px = rows_size(fonts, rows, width, design, greys);

    // The column is as wide as its widest row needs and no wider, then centred.
    //
    // A row is glyph-or-label at one end and figure at the other, and stretching
    // that across the whole content box put a void between them: on a 950 pixel
    // cell the eye had to cross half the panel to join a sofa to its temperature.
    // Giving up the alignment instead — letting each row sit as wide as its own
    // content — would have cost the thing that makes a column of readings
    // scannable, which is that the figures line up. So the column keeps its right
    // edge and loses the emptiness.
    //
    // `row_width = "full"` is the other trade, for an author who wants the table:
    // the rules then reach both edges of the cell and the eye crosses the gap on
    // one of them instead of on nothing.
    let column = match style.row_width {
        crate::config::RowWidth::Full => width,
        crate::config::RowWidth::Content => {
            let widest = rows
                .iter()
                .map(|line| intrinsic_width(fonts, row_runs(line, row_px, greys)))
                .fold(0.0, f32::max);
            (widest * COLUMN_SLACK).clamp(1.0, width)
        }
    };

    let gap = row_gap(row_px, rows.len(), height, pinned);

    let mut children = rows
        .iter()
        // Each line's own ink, not the cell's: one unreachable sensor mutes its own
        // line and leaves the readings around it black.
        .map(|line| row_node(line, row_px, column, style, greys))
        .collect::<Vec<_>>();

    // The rest of the page, ruled and left blank. Only offered with the rules on,
    // because without them it would draw nothing at all: the blank lines *are* their
    // rules.
    if style.row_rule && style.row_fill {
        let pitch = row_px * natural::LINE_BOX + gap;
        let ruled = (height / pitch).floor() as usize;
        children.extend(
            (rows.len()..ruled).map(|_| blank_row_node(row_px * natural::LINE_BOX, column, greys)),
        );
    }

    // Ruled from the top when the rules are on, because a form starts at the top of
    // its page. Pinned rows start there too, and give up the centring across the box
    // as well, for one reason: a pinned column is not fitted to its box, so there is
    // no middle of anything it was trying to fill. Down the box what is left below
    // the last row is paper a ticket declined to spend; across it, a narrow column
    // centred in a cell reads as an indent nobody asked for.
    let (justify, align) = match (pinned, style.row_rule) {
        (true, _) => (JustifyContent::Start, AlignItems::Start),
        (false, true) => (JustifyContent::Start, AlignItems::Center),
        (false, false) => (JustifyContent::Center, AlignItems::Center),
    };

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::justify_content(justify))
            .with(StyleDeclaration::align_items(align))
            .with(StyleDeclaration::height(Length::Px(height)))
            .with(StyleDeclaration::width(Length::Px(width)))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap)))),
    )
}

/// What a column of `count` rows occupies at the tightest pitch it is ever drawn
/// at, as a multiple of one row's type size.
///
/// A line box each and the air between two of them, which is the same sum
/// [`natural`] charges a track for the same column — and it has to be. A column at
/// one pitch and drawn at another is either a reading clipped by the cell's edge or
/// a strip of the cell nothing ever uses.
///
/// The last row's gap is not in it, because a gap is between two rows and the last
/// has nothing under it. The track estimate charges for it anyway; that is the
/// difference between an estimate that must never come out short and an arrangement
/// that must fit exactly what it was given.
fn block(count: usize) -> f32 {
    count.max(1) as f32 * (natural::LINE_BOX + natural::ROW_GAP_FLOOR) - natural::ROW_GAP_FLOOR
}

/// The air between two rows of a column of `count` set at `row_px` in a box
/// `height` tall, in pixels.
///
/// Bounded rather than shared out. Giving the rows whatever the type ceiling left
/// over put a hundred and eighty pixels between two readings, which reads as four
/// unrelated cells rather than one table; a bounded gap leaves the block with air
/// around it instead of inside it, which is what air is for.
///
/// The slack is measured against the line boxes the rows occupy and not against
/// their type size, which is the bug this function exists to have fixed: a column of
/// ten measured on the size alone left a fifth of itself unaccounted for, spread
/// that height into the gaps as well, and drew its last reading past the bottom of
/// the cell.
///
/// A pinned column takes the floor and nothing else. Spreading is how a box gets
/// filled and a pinned column is not filling one — what is under its last row is
/// paper a ticket declined to spend.
fn row_gap(row_px: f32, count: usize, height: f32, pinned: bool) -> f32 {
    let floor = row_px * natural::ROW_GAP_FLOOR;
    if pinned {
        return floor;
    }
    let slack = height - row_px * natural::LINE_BOX * count as f32;
    (slack / (count.max(2) - 1) as f32).clamp(floor, row_px * ROW_GAP_CEILING)
}

/// A ruled line with nothing written on it, at a real line's height.
///
/// Nothing inside it, so unlike a line of type it cannot overflow the height it is
/// given — which is why this one may be handed a fixed height and a reading may not.
fn blank_row_node(height: f32, width: f32, greys: Greys) -> Node {
    Node::container(Vec::new()).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::width(Length::Px(width)))
            .with(StyleDeclaration::height(Length::Px(height)))
            .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0))))
            .with(StyleDeclaration::border_bottom_width(rule_width(1.0)))
            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_bottom_color(rule(greys))),
    )
}

/// The size at which the widest row would occupy [`COLUMN_TARGET`] of `width`.
///
/// Measured at a reference size and scaled, because a row's advance is proportional
/// to its type size: one measurement is exact rather than a search. The reference is
/// large enough that a glyph's fixed margins do not dominate the ratio.
///
/// This is what stops a column of four short readings from sitting in the middle of
/// a 950 pixel cell with a third of the panel blank on either side of it. Filling
/// the space is not vanity: on a wall panel every pixel not spent on a reading is a
/// pixel spent making the reading smaller than it could have been.
fn width_driven_size(fonts: &Fonts, rows: &[Line], width: f32, greys: Greys) -> f32 {
    const REFERENCE: f32 = 100.0;
    let widest = rows
        .iter()
        .map(|line| intrinsic_width(fonts, row_runs(line, REFERENCE, greys)))
        .fold(0.0, f32::max);
    if widest <= 0.0 {
        return REFERENCE;
    }
    REFERENCE * (width * COLUMN_TARGET) / widest
}

/// The one size every row is set at, shaved so the widest of them fits.
///
/// One size for every row rather than one per row, and that is the point of a
/// list: rows sized individually read as unrelated readings that happen to be
/// stacked, where a shared size reads as a table. So the widest row decides for
/// all of them.
///
/// A safety net rather than the sizing rule: [`rows_node`] has already chosen a
/// design size the widest row should fit at, and this shaves it if the measurement
/// disagrees. Without it, a reading long enough to overflow was drawn overflowing —
/// `Office 21.3 °C` at 74px in a 322px cell overprinted its neighbours.
fn rows_size(fonts: &Fonts, rows: &[Line], width: f32, design: f32, greys: Greys) -> f32 {
    let widest = rows
        .iter()
        .map(|line| intrinsic_width(fonts, row_runs(line, design, greys)))
        .fold(0.0, f32::max);
    fit_size(widest, width, design)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::{Chrome, Device, Dither, WidgetKind};
    use crate::content::Row;

    use super::super::body::MIN_TYPE_PX;
    use super::super::grid::Layout;
    use super::super::types::Ink;

    use super::super::test_support::*;
    use super::*;

    #[test]
    fn a_zero_border_draws_no_rules() {
        // A frameless dashboard has to be the *absence* of the border declarations. A
        // zero-width solid edge still costs the layout engine a pass on each of the
        // four sides, and `border = 0` is an author asking for bare readings rather
        // than for a rule nobody can see.
        let framed = Device {
            dither: Dither::None,
            ..device(vec![widget("a", WidgetKind::Value, 0, 0)])
        };
        let frameless = Device {
            chrome: Chrome {
                border: 0.0,
                ..framed.chrome
            },
            ..framed.clone()
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("21.4"), now()))]);

        // The cell's top edge, which a rule traces corner to corner and nothing else
        // reaches: a cell's content sits a padding inside it.
        let (x, y, w, h) = Layout::for_device(&framed, &nothing_pushed()).rect(&framed.widgets[0]);
        let edge = (x + 2.0, y, w - 4.0, 1.0);

        let (width, _, ruled) = greys(&render(&framed, &content));
        assert!(
            inked(&ruled, width, edge),
            "a cell's rule must trace its top edge"
        );

        let (width, _, bare) = greys(&render(&frameless, &content));
        assert!(
            !inked(&bare, width, edge),
            "a zero border must draw nothing at all"
        );
        // And the cell still has its label and its reading in it, so the assertion
        // above cannot be passing on a blank frame.
        assert!(
            inked(&bare, width, (x, y, w, h)),
            "a frameless cell must still draw its contents"
        );
    }

    #[test]
    fn a_column_of_readings_is_fitted_to_its_width_and_not_only_its_height() {
        // Sized by height alone — which is all this did — three readings in a 322x302
        // cell were set at 74px, and `Office 21.3 °C` at 74px is half again as wide as
        // the cell it was drawn in. It overprinted the readings under it.
        let rows = [
            resolved_line(
                "Office",
                serde_json::json!("21.3"),
                Some("\u{b0}C"),
                Ink::Current,
            ),
            resolved_line(
                "Bedroom",
                serde_json::json!("19.1"),
                Some("\u{b0}C"),
                Ink::Current,
            ),
            resolved_line("Hall", serde_json::json!("48"), Some("%"), Ink::Current),
        ];

        let tall = 302.0;
        let by_height = tall / block(3);
        let wide = rows_size(&FONTS, &rows, 900.0, by_height, GREYS);
        let narrow = rows_size(&FONTS, &rows, 322.0, by_height, GREYS);

        assert!(
            (wide - by_height).abs() < 1.0,
            "given width to spare the height decides: {wide} should be about {by_height}"
        );
        assert!(
            narrow < by_height * 0.8,
            "in a 322px box the widest row has to shrink the column: {narrow} vs {by_height}"
        );
        assert!(
            narrow >= MIN_TYPE_PX,
            "the fit still floors at the readable size: {narrow}"
        );
    }

    #[test]
    fn every_reading_in_a_list_keeps_its_own_line() {
        // Three readings, three lines of ink — including the case the fit cannot
        // satisfy. A value too long for the box even at [`MIN_TYPE_PX`] used to wrap,
        // and a wrapped line inside a one-line box prints over the reading beneath it,
        // so the count is what catches it.
        for values in [
            ["21.3", "19.1", "48"],
            // What a publisher sends when nobody configured `precision`.
            ["21.299999237060547", "19.050000190734863", "47.8231"],
        ] {
            let (device, states) = listed_panel(values);
            let png = render_with(&device, &HashMap::new(), &states, &HashMap::new());
            let (width, _, levels) = greys(&png);

            let layout = Layout::for_device(&device, &nothing_pushed());
            let (x, y, w, h) = layout.rect(&device.widgets[0]);
            let inset = layout.inset() / 2.0;
            // Below the header, so the label above the readings is not counted as one.
            let body = (
                x + inset,
                y + inset + h * 0.25,
                w - inset * 2.0,
                h * 0.75 - inset * 2.0,
            );

            assert_eq!(
                ink_bands(&levels, width, body),
                3,
                "three readings must draw three lines, whatever their values ({values:?})"
            );
        }
    }

    #[test]
    fn a_column_grows_to_fill_a_cell_wider_than_its_readings() {
        // The complaint this pins: four short readings in a 950px cell were set at
        // what the height and the ceiling allowed, came out 480px wide, and were
        // centred — leaving a third of the panel blank on either side. Every pixel
        // not spent on a reading is a pixel spent making the reading smaller than it
        // could have been.
        let rows: Vec<Line> = ["21.9", "22.3", "26.1", "53"]
            .iter()
            .map(|v| resolved_line("", serde_json::json!(*v), Some("\u{b0}C"), Ink::Current))
            .collect();

        for width in [400.0_f32, 950.0] {
            let size = width_driven_size(&FONTS, &rows, width, GREYS);
            let widest = rows
                .iter()
                .map(|line| intrinsic_width(&FONTS, row_runs(line, size, GREYS)))
                .fold(0.0_f32, f32::max);
            let filled = widest / width;
            assert!(
                (filled - COLUMN_TARGET).abs() < 0.06,
                "at {width}px the widest row should occupy about {COLUMN_TARGET} of the \
                 cell, got {filled:.2}"
            );
        }
    }

    #[test]
    fn a_column_of_readings_never_outgrows_the_box_it_was_given() {
        // Two bugs in one arrangement, and both of them cost a reading.
        //
        // Sharing out whatever the ceiling left over put 180px between two readings,
        // which reads as four unrelated cells rather than one table. And measuring
        // the slack against the type size rather than the line box left a fifth of
        // the column unaccounted for, spread that into the gaps as well, and drew the
        // last reading past the bottom of the cell — on a ten-item shopping list, off
        // the end of the ticket.
        let row_px = 60.0_f32;
        for (count, height) in [(4, 2000.0_f32), (10, 425.0), (3, 302.0), (1, 500.0)] {
            let gap = row_gap(row_px, count, height, false);
            assert!(
                gap <= row_px * ROW_GAP_CEILING + 0.01,
                "the gap must stay bounded: {count} rows in {height}px got {gap}"
            );
            assert!(
                gap >= row_px * natural::ROW_GAP_FLOOR - 0.01,
                "and never go under the floor: {count} rows in {height}px got {gap}"
            );
        }

        // The column the sizing rule actually produces, in the box that produced it,
        // fits inside it: `block` is what the type is fitted by, so a column set at
        // that size and spaced by `row_gap` cannot come out taller than the box.
        for (count, height) in [(10, 425.0_f32), (4, 200.0), (2, 90.0), (1, 40.0)] {
            let size = height / block(count);
            let column = size * natural::LINE_BOX * count as f32
                + row_gap(size, count, height, false) * (count - 1) as f32;
            assert!(
                column <= height + 0.01,
                "{count} rows fitted to a {height}px box came out {column}px tall"
            );
        }

        // A pinned column is spaced by the floor whatever the box, because the box is
        // not a thing it is trying to fill.
        assert!(
            (row_gap(20.0, 10, 4000.0, true) - 20.0 * natural::ROW_GAP_FLOOR).abs() < 0.01,
            "a pinned column takes the floor and nothing more"
        );
    }

    #[test]
    fn a_pinned_pushed_list_costs_the_paper_its_rows_cost_and_no_more() {
        // The complaint, measured on the device: one cell stretched to the frame, the
        // type sized to fill it, and a one-item list costing the same roll as a
        // ten-item one. Three things have to be true for a ticket to be as long as
        // its content, and each is asserted here: the rows are all drawn, the frame
        // ends after the last of them, and the roll's length does not decide either.
        let (device, content) = ticket(10, 768);
        let png = render(&device, &content);
        let (width, _, levels) = greys(&png);

        let layout = Layout::for_device(&device, &content);
        let (x, y, w, h) = layout.rect(&device.widgets[1]);
        let inset = layout.inset() / 2.0;
        assert_eq!(
            ink_bands(
                &levels,
                width,
                (x + inset, y + inset, w - inset * 2.0, h - inset * 2.0)
            ),
            10,
            "ten pushed rows must draw ten lines: fewer means the column was clipped \
             by its track, more means two of them overprinted"
        );

        // Seven more rows cost seven rows of paper. Bounded rather than pinned to a
        // number, because the engine's own line box is what falls between the two:
        // each row costs at least that box and at most a full pitch, and paper is
        // measured in whole rows, so a pixel of rounding is allowed at each end.
        let (device, content) = ticket(3, 768);
        let short = paper(&render(&device, &content));
        let long = paper(&png);
        let per_row = 20.0 * natural::LINE_BOX - 1.0
            ..=20.0 * (natural::LINE_BOX + natural::ROW_GAP_FLOOR) + 1.0;
        let grown = (long - short) as f32 / 7.0;
        assert!(
            per_row.contains(&grown),
            "seven more items must cost seven rows of paper: {short}px against \
             {long}px is {grown:.1}px each, outside {:.1}..={:.1}",
            per_row.start(),
            per_row.end()
        );

        // And the roll is not what decides it. This is the whole complaint: the frame
        // height set both the type size and the paper cost, so the same list on a
        // longer roll printed a longer ticket.
        let (longer_roll, content) = ticket(10, 1200);
        assert_eq!(
            paper(&render(&longer_roll, &content)),
            long,
            "the same list on a longer roll must cost the same paper"
        );
        assert!(
            long < 768,
            "and the ticket must end before the frame does, or there is nothing for \
             the sink to trim: got {long}px of 768"
        );
    }

    #[test]
    fn a_column_of_readings_is_capped_below_what_a_tall_cell_would_allow() {
        // The ceiling, in the shape the complaint arrived in: four readings in a cell
        // half the panel high were set at what the height afforded and shouted.
        let rows: Vec<Line> = ["21.9", "22.3", "26.1", "53"]
            .iter()
            .map(|v| resolved_line("Room", serde_json::json!(*v), Some("\u{b0}C"), Ink::Current))
            .collect();

        let label_px = 32.0;
        let tall = rows_size(
            &FONTS,
            &rows,
            691.0,
            {
                let by_height = 991.0_f32 / block(4);
                by_height
                    .min(label_px * STYLE.reading_ceiling)
                    .max(STYLE.min_type)
            },
            GREYS,
        );
        assert!(
            tall <= label_px * STYLE.reading_ceiling + 0.5,
            "a tall cell must not set its rows past the ceiling: got {tall}"
        );
        assert!(
            tall > label_px,
            "the reading still has to outweigh the label naming it: got {tall}"
        );
    }

    #[test]
    fn width_driven_size_falls_back_to_the_reference_when_every_row_is_empty() {
        // A column with nothing in it — no label, no value, no icon on its one
        // row — must not divide by a zero-width measurement.
        let rows = vec![Line {
            row: Row {
                id: None,
                label: None,
                value: None,
                unit: None,
                state: None,
            },
            icon: None,
            trend: None,
            ink: Ink::Current,
        }];
        assert_eq!(
            width_driven_size(&FONTS, &rows, 400.0, GREYS),
            100.0,
            "an empty column falls back to width_driven_size's reference size"
        );
    }
}
