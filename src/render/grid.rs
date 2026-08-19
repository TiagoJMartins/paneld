//! The dashboard's pixel geometry, in one place.
//!
//! Rendering places a cell and a tap resolves to one, and both need the same
//! arithmetic. Two copies of it would be a correctness bug rather than untidiness:
//! the moment they drifted, a finger on one cell would fire another cell's action.

use crate::config::{Device, Widget};

/// The dashboard's pixel geometry, derived once from a device and used both to
/// lay the frame out and to decide what a finger landed on.
///
/// Extracted rather than recomputed at each call site: a tap resolves to a
/// widget by the same arithmetic that placed it, so the two can never disagree
/// about where a cell is.
#[derive(Debug, Clone, Copy)]
pub struct Layout {
    gutter: f32,
    padding: f32,
    cell_w: f32,
    cell_h: f32,
}

impl Layout {
    /// Derives the geometry from a device's dimensions and grid.
    ///
    /// Gutter and cell padding are scaled to the cell rather than fixed. Fixed
    /// spacing is a trap on a small panel or a dense grid: once padding exceeds the
    /// cell it is inside, the text layout engine is handed a negative content box
    /// and panics. Scaling keeps the content box positive for every grid a
    /// validated config can express.
    pub fn for_device(device: &Device) -> Self {
        let grid = device.grid;
        let smallest_side =
            (device.width as f32 / grid.cols as f32).min(device.height as f32 / grid.rows as f32);

        let gutter = (smallest_side * 0.06).clamp(1.0, 10.0);
        // A tighter ceiling than the gutter's, because padding is charged twice —
        // once on each side — and it comes straight off the width a figure has to
        // fit in. The hairline rule and the gutter already separate two cells; a
        // wider inset only shrinks the number inside one.
        let padding = (smallest_side * 0.10).clamp(2.0, 8.0);

        // Usable cell size: what one `1fr` track resolves to once the frame's own
        // padding and the gaps between tracks are taken out. Needed to scale type
        // to the cell rather than fixing it, and to place a rect.
        let cell_w =
            (device.width as f32 - gutter * (grid.cols + 1) as f32).max(1.0) / grid.cols as f32;
        let cell_h =
            (device.height as f32 - gutter * (grid.rows + 1) as f32).max(1.0) / grid.rows as f32;

        Self {
            gutter,
            padding,
            cell_w,
            cell_h,
        }
    }

    /// Between cells, and around the frame, in pixels.
    pub fn gutter(&self) -> f32 {
        self.gutter
    }

    /// Inside a cell, in pixels.
    pub fn padding(&self) -> f32 {
        self.padding
    }

    /// One grid cell's size, in pixels.
    pub fn cell(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// The rect a widget occupies, as (x, y, w, h) in frame pixels.
    ///
    /// The exact inverse of the layout rather than an approximation of it: the frame
    /// is a CSS grid whose gaps and outer padding are both one gutter, and
    /// [`Self::cell`] is already net of them, so a cell's origin is the frame's
    /// padding plus one cell-and-gutter per column before it.
    ///
    /// A spanning widget swallows the gutters it spans over, because those gaps fall
    /// *inside* such a cell rather than beside it — so a two-column widget is two
    /// cells plus one gutter wide, not two cells.
    pub fn rect(&self, widget: &Widget) -> (f32, f32, f32, f32) {
        let x = self.gutter + widget.col as f32 * (self.cell_w + self.gutter);
        let y = self.gutter + widget.row as f32 * (self.cell_h + self.gutter);
        // `saturating_sub` rather than `- 1`: a span of zero is a config error, and
        // an arithmetic panic inside a request handler is a worse way to report one
        // than a degenerate rect that nothing hits.
        let w = self.cell_w * widget.col_span as f32
            + self.gutter * widget.col_span.saturating_sub(1) as f32;
        let h = self.cell_h * widget.row_span as f32
            + self.gutter * widget.row_span.saturating_sub(1) as f32;
        (x, y, w, h)
    }

    /// The widget a point lands on, or `None` for a gutter or an empty cell.
    ///
    /// Half-open containment, so two adjacent cells never both claim an edge
    /// pixel. A point in a gutter belongs to nobody: nudging it into the nearer
    /// cell would make a miss silently fire whichever action happened to be
    /// closest. On a 300 ppi panel a gutter is about a millimetre wide, so a miss
    /// is a routine outcome rather than an exotic one — which is exactly why it
    /// must not be guessed at.
    ///
    /// The first widget whose rect contains the point wins, which is unambiguous
    /// because config validation rejects two widgets over one cell. A point outside
    /// the frame, or a `NaN` from a client that sent nonsense, is inside no rect and
    /// therefore hits nothing.
    pub fn hit<'a>(&self, device: &'a Device, x: f32, y: f32) -> Option<&'a Widget> {
        device.widgets.iter().find(|widget| {
            let (left, top, width, height) = self.rect(widget);
            x >= left && x < left + width && y >= top && y < top + height
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Dither, Grid, Palette, WidgetKind};

    /// A widget occupying one or more cells, with everything a hit test does not
    /// look at left empty.
    fn widget(id: &str, col: u32, row: u32, col_span: u32, row_span: u32) -> Widget {
        Widget {
            id: id.to_owned(),
            kind: WidgetKind::Value,
            col,
            row,
            col_span,
            row_span,
            label: None,
            unit: None,
            stale_after: 0,
            entity: None,
            attribute: None,
            on_values: Vec::new(),
            icon: None,
            tap: None,
        }
    }

    fn device_sized(cols: u32, rows: u32, width: u32, height: u32, widgets: Vec<Widget>) -> Device {
        Device {
            id: "kindle".to_owned(),
            width,
            height,
            palette: Palette::Gray16,
            dither: Dither::Bayer,
            refresh_rate: 300,
            render_interval: 300,
            max_frame_bytes: 0,
            grid: Grid { cols, rows },
            widgets,
        }
    }

    /// The panel in service: a Kindle Paperwhite in landscape.
    fn device(cols: u32, rows: u32, widgets: Vec<Widget>) -> Device {
        device_sized(cols, rows, 1448, 1072, widgets)
    }

    /// Every cell of a 4x3 grid filled, so a miss is always a gutter or an
    /// out-of-frame point rather than an unoccupied cell.
    fn full_grid() -> Device {
        let widgets = (0..3)
            .flat_map(|row| (0..4).map(move |col| widget(&format!("w{row}_{col}"), col, row, 1, 1)))
            .collect();
        device(4, 3, widgets)
    }

    /// The centre of the cell at `(col, row)`, which is the point a tap on that
    /// cell most plausibly carries.
    fn centre(layout: &Layout, col: u32, row: u32) -> (f32, f32) {
        let (cell_w, cell_h) = layout.cell();
        let gutter = layout.gutter();
        (
            gutter + col as f32 * (cell_w + gutter) + cell_w / 2.0,
            gutter + row as f32 * (cell_h + gutter) + cell_h / 2.0,
        )
    }

    fn hit_id(device: &Device, layout: &Layout, x: f32, y: f32) -> Option<String> {
        layout.hit(device, x, y).map(|widget| widget.id.clone())
    }

    #[test]
    fn a_point_inside_a_cell_resolves_to_that_cells_widget() {
        let device = full_grid();
        let layout = Layout::for_device(&device);

        for row in 0..3 {
            for col in 0..4 {
                let (x, y) = centre(&layout, col, row);
                assert_eq!(
                    hit_id(&device, &layout, x, y).as_deref(),
                    Some(format!("w{row}_{col}").as_str()),
                    "the centre of ({col}, {row}) must resolve to its own widget"
                );
            }
        }
    }

    #[test]
    fn a_rects_own_centre_hits_the_widget_the_rect_came_from() {
        let device = full_grid();
        let layout = Layout::for_device(&device);

        for widget in &device.widgets {
            let (x, y, w, h) = layout.rect(widget);
            let hit = layout
                .hit(&device, x + w / 2.0, y + h / 2.0)
                .expect("a rect's own centre must hit something");
            assert_eq!(hit.id, widget.id, "rect and hit disagree about a cell");
        }
    }

    /// The cases that must all answer "nobody", each for its own reason.
    #[test]
    fn a_point_on_no_cell_resolves_to_nothing() {
        let device = full_grid();
        let layout = Layout::for_device(&device);
        let (cell_w, cell_h) = layout.cell();
        let gutter = layout.gutter();
        let (centre_x, centre_y) = centre(&layout, 0, 0);

        let cases: &[(&str, f32, f32)] = &[
            ("the frame's own left padding", gutter / 2.0, centre_y),
            ("the frame's own top padding", centre_x, gutter / 2.0),
            (
                "the gutter between two columns",
                gutter + cell_w + gutter / 2.0,
                centre_y,
            ),
            (
                "the gutter between two rows",
                centre_x,
                gutter + cell_h + gutter / 2.0,
            ),
            ("left of the frame", -1.0, centre_y),
            ("above the frame", centre_x, -1.0),
            ("right of the frame", device.width as f32 + 1.0, centre_y),
            ("below the frame", centre_x, device.height as f32 + 1.0),
            ("a coordinate that is not a number", f32::NAN, centre_y),
        ];

        for (what, x, y) in cases {
            assert_eq!(
                hit_id(&device, &layout, *x, *y),
                None,
                "{what} must belong to nobody"
            );
        }
    }

    #[test]
    fn an_unoccupied_cell_resolves_to_nothing() {
        // Only (0, 0) is laid out, so the other eleven cells of the 4x3 grid are
        // real cells with nothing in them to fire.
        let device = device(4, 3, vec![widget("only", 0, 0, 1, 1)]);
        let layout = Layout::for_device(&device);

        let (x, y) = centre(&layout, 0, 0);
        assert_eq!(hit_id(&device, &layout, x, y).as_deref(), Some("only"));

        let (x, y) = centre(&layout, 2, 1);
        assert_eq!(
            hit_id(&device, &layout, x, y),
            None,
            "an empty cell has no action to fire"
        );
    }

    #[test]
    fn a_spanning_widget_is_hit_from_every_cell_it_covers() {
        let device = device(
            4,
            3,
            vec![widget("wide", 1, 0, 2, 1), widget("tall", 0, 1, 1, 2)],
        );
        let layout = Layout::for_device(&device);
        let (cell_w, cell_h) = layout.cell();
        let gutter = layout.gutter();

        for col in [1, 2] {
            let (x, y) = centre(&layout, col, 0);
            assert_eq!(
                hit_id(&device, &layout, x, y).as_deref(),
                Some("wide"),
                "column {col} of a col_span = 2 widget must hit it"
            );
        }
        // The gutter a span crosses is inside the spanning widget, not beside it.
        let (_, y) = centre(&layout, 1, 0);
        let seam = gutter + cell_w + gutter / 2.0 + cell_w;
        assert_eq!(
            hit_id(&device, &layout, seam, y).as_deref(),
            Some("wide"),
            "the seam a span swallows belongs to the spanning widget"
        );

        for row in [1, 2] {
            let (x, y) = centre(&layout, 0, row);
            assert_eq!(
                hit_id(&device, &layout, x, y).as_deref(),
                Some("tall"),
                "row {row} of a row_span = 2 widget must hit it"
            );
        }
        let (x, _) = centre(&layout, 0, 1);
        assert_eq!(
            hit_id(&device, &layout, x, gutter + cell_h + gutter / 2.0 + cell_h).as_deref(),
            Some("tall"),
            "the seam a row span swallows belongs to the spanning widget"
        );
    }

    /// Pins the half-open rule: a cell owns its first pixel and not the one past
    /// its last. Adjacent cells are separated by a gutter of at least a pixel, so
    /// the pixel a closed test would let both claim lands between them and is
    /// claimed by neither.
    #[test]
    fn a_cell_owns_its_leading_edge_and_not_its_trailing_one() {
        let device = full_grid();
        let layout = Layout::for_device(&device);
        let (x, y, w, _) = layout.rect(&device.widgets[0]);
        let (next_x, ..) = layout.rect(&device.widgets[1]);

        assert_eq!(
            hit_id(&device, &layout, x, y).as_deref(),
            Some("w0_0"),
            "a cell owns its own top-left pixel"
        );
        assert_eq!(
            hit_id(&device, &layout, x + w, y),
            None,
            "the pixel past a cell's right edge is gutter, not the next cell"
        );
        assert_eq!(
            hit_id(&device, &layout, next_x, y).as_deref(),
            Some("w0_1"),
            "the next cell starts owning at its own left edge"
        );
        assert!(
            next_x > x + w,
            "adjacent cells are separated by a gutter and never abut"
        );
    }

    /// The check that catches the arithmetic disagreeing with the renderer: the
    /// last column's right edge must land exactly on the frame's own right padding,
    /// and the last row's bottom edge on its bottom padding. If either drifts, every
    /// hit test is off by that much.
    #[test]
    fn the_last_cell_ends_where_the_frames_padding_begins() {
        for (cols, rows, width, height) in [
            (4u32, 3u32, 1448u32, 1072u32),
            (2, 2, 400, 300),
            (1, 1, 240, 160),
            (8, 6, 1448, 1072),
        ] {
            let last = widget("last", cols - 1, rows - 1, 1, 1);
            let device = device_sized(cols, rows, width, height, vec![last.clone()]);
            let layout = Layout::for_device(&device);
            let (x, y, w, h) = layout.rect(&last);
            let gutter = layout.gutter();

            assert!(
                ((x + w) - (width as f32 - gutter)).abs() < 0.5,
                "{cols}x{rows} at {width}x{height}: last column ends at {}, \
                 but the frame's right padding begins at {}",
                x + w,
                width as f32 - gutter
            );
            assert!(
                ((y + h) - (height as f32 - gutter)).abs() < 0.5,
                "{cols}x{rows} at {width}x{height}: last row ends at {}, \
                 but the frame's bottom padding begins at {}",
                y + h,
                height as f32 - gutter
            );
        }
    }
}
