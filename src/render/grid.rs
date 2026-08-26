//! The dashboard's pixel geometry, in one place.
//!
//! Rendering places a cell and a tap resolves to one, and both need the same
//! arithmetic. Two copies of it would be a correctness bug rather than untidiness:
//! the moment they drifted, a finger on one cell would fire another cell's action.
//!
//! Which is why the numbers are now *read* rather than worked out here.
//! [`crate::config::Chrome`] resolves a dashboard's spacing to pixels and
//! [`crate::config::cell_size`] turns that into a track, so the validation that
//! rejects an unrenderable cell, this module, and the renderer all measure a cell
//! by one copy of the arithmetic. This module only places what those two decided.

use super::natural;
use crate::config::{self, Chrome, Device, Fit, Group, Widget};

/// The dashboard's pixel geometry, read once from a device and used both to lay
/// the frame out and to decide what a finger landed on.
///
/// Extracted rather than recomputed at each call site: a tap resolves to a
/// widget by the same arithmetic that placed it, so the two can never disagree
/// about where a cell is.
///
/// Describes a group's sub-grid as well as a device's own grid, because a group
/// *is* a grid inside one cell and its children have to be placed and hit exactly
/// like any other cell. [`Self::sub_layout`] is the whole difference between the
/// two, and it differs only in where the tracks start.
#[derive(Debug, Clone)]
pub struct Layout {
    /// The top-left of the track area, in frame pixels. Not the origin of the
    /// frame: a status bar takes an edge, and a group's tracks start inside its
    /// own cell.
    origin: (f32, f32),
    /// Between [`Self::origin`] and the first track. One gap for a device's own
    /// grid, which is held off its area on every side; `0.0` inside a group,
    /// whose padding already is that inset — a margin there would inset the
    /// children twice.
    margin: f32,
    gutter: f32,
    padding: f32,
    border: f32,
    cell_w: f32,
    /// The shortest row track, which under [`Fit::Stretch`] is the height every
    /// track has. Under [`Fit::Content`] deliberately the shortest of unequal
    /// tracks: a caller sizing chrome against "a cell" must not be handed a height
    /// that overflows the tightest track its cell could have landed in.
    cell_h: f32,
    /// How the row tracks were sized, and so whether [`Self::rect`] multiplies out
    /// one track height or walks a list of them.
    fit: Fit,
    /// Every row track's height, top to bottom, in pixels. One entry per row.
    tracks: Vec<f32>,
}

impl Layout {
    /// Reads the geometry of a device's own widget grid.
    ///
    /// Every number comes off the [`Device`]. The track area is
    /// [`Device::grid_area`], so a status bar moves and shrinks the grid without
    /// this module having to know which edge it took; the spacing is the device's
    /// already-resolved [`Chrome`], so an author who spelt out a gap gets it here
    /// unaltered.
    ///
    /// The row tracks are the one thing not simply read off. Under [`Fit::Content`]
    /// each is sized from what the widgets on it want, which is what
    /// [`super::natural`] answers; under [`Fit::Stretch`] every track is the cell
    /// height the shared arithmetic already gave.
    pub fn for_device(device: &Device) -> Self {
        let (area_x, area_y, area_w, area_h) = device.grid_area();
        let chrome = device.chrome;
        let (cell_w, cell_h) = config::cell_size(area_w, area_h, device.grid, chrome);
        let tracks = match device.grid.fit {
            Fit::Stretch => vec![cell_h; device.grid.rows.max(1) as usize],
            Fit::Content => content_tracks(device, cell_w, cell_h),
        };

        Self {
            origin: (area_x as f32, area_y as f32),
            margin: chrome.gap,
            gutter: chrome.gap,
            padding: chrome.padding,
            border: chrome.border,
            cell_w,
            cell_h: shortest(&tracks),
            fit: device.grid.fit,
            tracks,
        }
    }

    /// Between cells, and around a device's grid area.
    pub fn gutter(&self) -> f32 {
        self.gutter
    }

    /// Inside a cell, between its rule and its content.
    pub fn padding(&self) -> f32 {
        self.padding
    }

    /// A cell's rule, in pixels. `0.0` on a dashboard that draws no frames.
    pub fn border(&self) -> f32 {
        self.border
    }

    /// What a cell's content box loses to its own chrome, on each axis: padding
    /// and rule on both sides.
    pub fn inset(&self) -> f32 {
        self.chrome().inset()
    }

    /// One grid cell's size, in pixels: a column's width, and the shortest row
    /// track.
    ///
    /// The shortest rather than the tallest or an average, because a caller sizes
    /// chrome against this: type fitted to a track taller than the one its cell
    /// landed in overflows that cell. Under [`Fit::Stretch`] every track is that
    /// height anyway, so the choice only shows itself on a content-fit grid.
    pub fn cell(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// Every row track's height, top to bottom, in pixels: one entry per row of the
    /// grid this layout describes.
    ///
    /// Read by the renderer, which writes them into the grid's row template. A
    /// content-fit dashboard has to be *drawn* on the tracks this module placed it
    /// on — equal tracks in the template against unequal ones here would put every
    /// cell somewhere the tap hit test does not look.
    pub fn row_tracks(&self) -> &[f32] {
        &self.tracks
    }

    /// The rect a widget occupies, as (x, y, w, h) in frame pixels.
    ///
    /// The exact inverse of the layout rather than an approximation of it: the
    /// tracks begin one margin in from the track area's origin, a column is
    /// [`Self::cell`] across, a row is as tall as its own track, and one gutter
    /// separates each track from the next.
    ///
    /// A spanning widget swallows the gutters it spans over, because those gaps
    /// fall *inside* such a cell rather than beside it — so a two-column widget is
    /// two cells plus one gutter wide, not two cells.
    ///
    /// Absolute for a group's children too, not relative to the group: a
    /// [`Self::sub_layout`]'s origin is already the group's content box in frame
    /// pixels, so a caller never has to remember which layout a rect came out of.
    pub fn rect(&self, widget: &Widget) -> (f32, f32, f32, f32) {
        let (origin_x, origin_y) = self.origin;
        let x = origin_x + self.margin + widget.col as f32 * (self.cell_w + self.gutter);
        // `saturating_sub` rather than `- 1`: a span of zero is a config error, and
        // an arithmetic panic inside a request handler is a worse way to report one
        // than a degenerate rect that nothing hits.
        let w = self.cell_w * widget.col_span as f32
            + self.gutter * widget.col_span.saturating_sub(1) as f32;
        let (top, h) = self.rows(widget.row, widget.row_span);
        (x, origin_y + self.margin + top, w, h)
    }

    /// How far row `row`'s top edge sits below the first track's, and how tall
    /// `span` tracks from there are — the gutters they swallow included.
    ///
    /// Equal tracks multiply out where unequal ones are summed, and that branch is
    /// not an optimisation. `k` copies of a float added together is not always the
    /// same float as `k` times one of them, and every frame this dashboard has ever
    /// drawn was placed by the multiplication: a cell moved by a ten-thousandth of a
    /// pixel is enough to resize a fitted run, and so to change the bytes of a frame
    /// on a grid where nothing was meant to change at all.
    fn rows(&self, row: u32, span: u32) -> (f32, f32) {
        let gutters = self.gutter * span.saturating_sub(1) as f32;
        match self.fit {
            Fit::Stretch => (
                row as f32 * (self.cell_h + self.gutter),
                self.cell_h * span as f32 + gutters,
            ),
            // Clamped rather than indexed, for the reason the width above saturates:
            // a widget placed off its grid is a config error validation rejects, and
            // a rect nothing hits reports one better than a panic in a handler.
            Fit::Content => {
                let start = (row as usize).min(self.tracks.len());
                let end = start.saturating_add(span as usize).min(self.tracks.len());
                (
                    self.tracks[..start].iter().sum::<f32>() + row as f32 * self.gutter,
                    self.tracks[start..end].iter().sum::<f32>() + gutters,
                )
            }
        }
    }

    /// The geometry of a group's sub-grid, or `None` when `group` is not a group.
    ///
    /// The children share the group's *content box* — its rect less padding and
    /// rule on every side — so the group's own padding is what holds them off its
    /// frame. That is why they take no margin of their own, and why
    /// [`config::sub_cell_size`] charges `n - 1` gaps for `n` tracks where an
    /// outer grid is charged `n + 1`.
    ///
    /// Spacing is inherited whole rather than scaled down. A group's children are
    /// already the smallest cells on the panel, and halving their padding to gain
    /// four pixels of content would make one dashboard read as two.
    pub fn sub_layout(&self, group: &Widget) -> Option<Layout> {
        Some(self.sub(group, group.group.as_ref()?))
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
    /// the frame, in a status bar's strip, or a `NaN` from a client that sent
    /// nonsense, is inside no rect and therefore hits nothing.
    ///
    /// A point inside a group descends into it and resolves to the child cell it
    /// landed on. Landing between two children resolves to the group itself, which
    /// is the opposite of what a point in an outer gutter does, and deliberately:
    /// an outer gutter is between cells and belongs to none of them, whereas a
    /// group is *itself* a cell, so every point inside its rect is inside a widget
    /// and reporting a miss there would drop a tap that plainly hit something. The
    /// group's own action is the honest answer, and a group without one falls
    /// through to no action just as any other untapped cell does.
    pub fn hit<'a>(&self, device: &'a Device, x: f32, y: f32) -> Option<&'a Widget> {
        let widget = device
            .widgets
            .iter()
            .find(|widget| self.contains(widget, x, y))?;
        let Some(group) = &widget.group else {
            return Some(widget);
        };
        let sub = self.sub(widget, group);
        Some(
            group
                .widgets
                .iter()
                .find(|child| sub.contains(child, x, y))
                .unwrap_or(widget),
        )
    }

    /// [`Self::sub_layout`] with the group already in hand, so the hit test does
    /// not have to unwrap an `Option` it just proved is `Some`.
    fn sub(&self, host: &Widget, group: &Group) -> Self {
        let (x, y, w, h) = self.rect(host);
        let chrome = self.chrome();
        // One side's worth of inset: `Chrome::inset` is what an axis loses, which
        // is both sides together.
        let edge = chrome.padding + chrome.border;
        let (cell_w, cell_h) =
            config::sub_cell_size(w - chrome.inset(), h - chrome.inset(), group.grid, chrome);

        Self {
            origin: (x + edge, y + edge),
            margin: 0.0,
            gutter: chrome.gap,
            padding: chrome.padding,
            border: chrome.border,
            cell_w,
            cell_h,
            // A group's sub-grid stretches whatever the device's grid does, because
            // the renderer fills a group's content box with equal tracks. Sizing
            // these to their children instead would place the children where nothing
            // draws them, and a finger on one child would fire another's action.
            fit: Fit::Stretch,
            tracks: vec![cell_h; group.grid.rows.max(1) as usize],
        }
    }

    /// This layout's spacing back as a [`Chrome`], which is the shape the shared
    /// cell-size arithmetic takes.
    fn chrome(&self) -> Chrome {
        Chrome {
            gap: self.gutter,
            padding: self.padding,
            border: self.border,
        }
    }

    /// Whether a point lands on a widget's rect, half-open on both axes.
    fn contains(&self, widget: &Widget, x: f32, y: f32) -> bool {
        let (left, top, width, height) = self.rect(widget);
        x >= left && x < left + width && y >= top && y < top + height
    }
}

/// The row tracks of a content-fit grid, top to bottom, in pixels.
///
/// Three passes, and their order is the whole design. Each track starts at the floor
/// a cell has to clear to render at all and is raised to what the widgets on it
/// want. Then, if those widgets want more of the frame than there is, every track is
/// scaled down together — a dashboard that is uniformly a little tight can still be
/// read, where one whose last row is drawn past the bottom of the glass cannot. Only
/// if height is left over does anything get it, and then only the one widget that
/// asked.
fn content_tracks(device: &Device, cell_w: f32, cell_h: f32) -> Vec<f32> {
    let rows = device.grid.rows.max(1) as usize;
    let mut tracks = vec![config::MIN_CELL as f32; rows];
    natural::raise_tracks(device, &device.widgets, cell_w, &mut tracks);

    // What the tracks have to share: the grid area less its own margin and the gaps
    // between tracks. Read back off the stretched cell rather than restated here, so
    // that both fits partition exactly the same height.
    let capacity = cell_h * rows as f32;
    let wanted = tracks.iter().sum::<f32>();
    if wanted > capacity {
        // Proportionally, rather than shaved off the tallest: every track on an
        // over-subscribed grid is one somebody asked for, and taking the whole
        // overflow out of the largest leaves a dashboard with one unreadable cell
        // instead of one that is slightly tight throughout. This can push a track
        // under [`config::MIN_CELL`], and that is the honest outcome — the
        // alternative is a row drawn off the frame.
        let scale = capacity / wanted;
        for track in &mut tracks {
            *track *= scale;
        }
        return tracks;
    }

    give_leftover(device, capacity - wanted, &mut tracks);
    tracks
}

/// Gives the height the content-sized tracks left over to the widget that asked.
///
/// At most one can ask: validation rejects a second, and rejects a `fill` on a
/// group's child. So the leftover goes to the one that did, split evenly across the
/// tracks it covers. A dashboard where nobody asked simply ends short of the frame's
/// bottom margin, and that unused strip is the point of `fit = "content"` rather than
/// an oversight in it — the alternative is inflating whichever cell happens to be
/// last until the frame is full, which is what stretching already does to all of
/// them.
fn give_leftover(device: &Device, leftover: f32, tracks: &mut [f32]) {
    let Some(widget) = device.widgets.iter().find(|widget| widget.fill) else {
        return;
    };
    let start = (widget.row as usize).min(tracks.len());
    let end = start
        .saturating_add(widget.row_span.max(1) as usize)
        .min(tracks.len());
    let covered = &mut tracks[start..end];
    let share = leftover / covered.len().max(1) as f32;
    for track in covered {
        *track += share;
    }
}

/// The shortest of a grid's row tracks, which is the height anything sized against
/// "a cell" has to survive.
///
/// Exactly the track height on a stretched grid, to the bit, because the minimum of
/// `n` copies of a float is that float. Never asked of an empty grid: every layout
/// has at least one row by construction.
fn shortest(tracks: &[f32]) -> f32 {
    tracks.iter().copied().fold(f32::INFINITY, f32::min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Dither, Edge, Grid, Palette, Reading, StatusBar, Style, Timezone, WidgetKind,
    };

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
            precision: None,
            state_text: true,
            stale_after: 0,
            entity: None,
            attribute: None,
            on_values: Vec::new(),
            icon: None,
            icon_on: None,
            icon_off: None,
            readings: Vec::new(),
            group: None,
            fill: false,
            style: Style::default(),
            tap: None,
        }
    }

    fn device_sized(cols: u32, rows: u32, width: u32, height: u32, widgets: Vec<Widget>) -> Device {
        let grid = Grid {
            cols,
            rows,
            fit: Fit::Stretch,
        };
        Device {
            id: "kindle".to_owned(),
            width,
            height,
            palette: Palette::Gray16,
            dither: Dither::Bayer,
            refresh_rate: 300,
            render_interval: 300,
            max_frame_bytes: 0,
            grid,
            style: Style::default(),
            chrome: Chrome::derived(width, height, grid),
            status_bar: None,
            widgets,
            sink: None,
        }
    }

    /// The panel in service: a Kindle Paperwhite in landscape.
    fn device(cols: u32, rows: u32, widgets: Vec<Widget>) -> Device {
        device_sized(cols, rows, 1448, 1072, widgets)
    }

    /// The same panel with its spacing spelt out rather than derived from the
    /// cell, which is what a `[devices.chrome]` table produces.
    fn device_with_chrome(cols: u32, rows: u32, chrome: Chrome, widgets: Vec<Widget>) -> Device {
        Device {
            chrome,
            ..device(cols, rows, widgets)
        }
    }

    /// The same panel with its rows sized to their content, which is all
    /// `fit = "content"` changes about a device.
    fn content_device(cols: u32, rows: u32, widgets: Vec<Widget>) -> Device {
        let mut device = device(cols, rows, widgets);
        device.grid.fit = Fit::Content;
        device
    }

    /// A cell of `count` readings, which is how a fixture asks for a taller track
    /// than the cell beside it: a column of readings is charged per reading.
    fn listing(id: &str, col: u32, row: u32, row_span: u32, count: usize) -> Widget {
        Widget {
            kind: WidgetKind::List,
            readings: (0..count)
                .map(|index| Reading {
                    label: Some(format!("reading {index}")),
                    icon: None,
                    entity: format!("sensor.reading_{index}"),
                    attribute: None,
                    unit: None,
                    precision: None,
                })
                .collect(),
            ..widget(id, col, row, 1, row_span)
        }
    }

    /// The same panel with a status bar along `edge`.
    ///
    /// Chrome is re-derived from the area the grid is left with, exactly as
    /// [`crate::config::parse`] derives it, so the fixture is the device a real
    /// configuration would have produced rather than one only this module makes.
    fn device_with_bar(
        cols: u32,
        rows: u32,
        edge: Edge,
        thickness: u32,
        widgets: Vec<Widget>,
    ) -> Device {
        let mut device = device(cols, rows, widgets);
        device.status_bar = Some(StatusBar {
            edge,
            thickness,
            fields: Vec::new(),
            timezone: Timezone::utc(),
            alerts: Vec::new(),
        });
        let (_, _, area_w, area_h) = device.grid_area();
        device.chrome = Chrome::derived(area_w, area_h, device.grid);
        device
    }

    /// Every cell of a 4x3 grid filled, so a miss is always a gutter or an
    /// out-of-frame point rather than an unoccupied cell.
    fn full_grid() -> Device {
        let widgets = (0..3)
            .flat_map(|row| (0..4).map(move |col| widget(&format!("w{row}_{col}"), col, row, 1, 1)))
            .collect();
        device(4, 3, widgets)
    }

    /// A 2x2 group in the top-left corner holding two children side by side, and
    /// one ordinary cell elsewhere to prove the descent is not unconditional.
    fn grouped() -> Device {
        let mut host = widget("group", 0, 0, 2, 2);
        host.kind = WidgetKind::Group;
        host.group = Some(Group {
            grid: Grid {
                cols: 2,
                rows: 1,
                fit: Fit::Stretch,
            },
            widgets: vec![widget("left", 0, 0, 1, 1), widget("right", 1, 0, 1, 1)],
        });
        device(4, 3, vec![host, widget("plain", 3, 0, 1, 1)])
    }

    /// The centre of the cell at `(col, row)`, which is the point a tap on that
    /// cell most plausibly carries.
    fn centre(layout: &Layout, col: u32, row: u32) -> (f32, f32) {
        let (cell_w, cell_h) = layout.cell();
        let gutter = layout.gutter();
        let (origin_x, origin_y) = layout.origin;
        (
            origin_x + layout.margin + col as f32 * (cell_w + gutter) + cell_w / 2.0,
            origin_y + layout.margin + row as f32 * (cell_h + gutter) + cell_h / 2.0,
        )
    }

    fn hit_id(device: &Device, layout: &Layout, x: f32, y: f32) -> Option<String> {
        layout.hit(device, x, y).map(|widget| widget.id.clone())
    }

    /// Sub-pixel agreement, which is all a hit test needs: half a pixel of drift
    /// cannot move a point out of a cell that is at least forty wide.
    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 0.01
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

    /// An author who spells out `chrome` gets those numbers, not the derived ones,
    /// and they move both the cell's size and where the cell lands. The pair is
    /// the point: a gap that shrank the cell without shifting the next one along
    /// would leave the grid overlapping itself.
    #[test]
    fn an_explicit_chrome_resizes_and_reseats_every_cell() {
        let chrome = Chrome {
            gap: 20.0,
            padding: 6.0,
            border: 2.0,
        };
        let panel = device_with_chrome(4, 3, chrome, vec![widget("mid", 1, 1, 1, 1)]);
        let layout = Layout::for_device(&panel);

        assert_eq!(
            layout.gutter(),
            20.0,
            "the gap is the author's, not derived"
        );
        assert_eq!(layout.padding(), 6.0);
        assert_eq!(layout.border(), 2.0);
        assert_eq!(
            layout.inset(),
            16.0,
            "padding and rule, charged on both sides"
        );

        // Five 20-pixel gaps across four columns, four down three rows.
        let (cell_w, cell_h) = layout.cell();
        assert!(close(cell_w, (1448.0 - 20.0 * 5.0) / 4.0));
        assert!(close(cell_h, (1072.0 - 20.0 * 4.0) / 3.0));

        let derived = Layout::for_device(&device(4, 3, Vec::new()));
        assert!(
            cell_w < derived.cell().0 && cell_h < derived.cell().1,
            "a 20 pixel gap is wider than the 10 this grid derives, so the cell \
             it leaves must be smaller"
        );

        let (x, y, w, h) = layout.rect(&panel.widgets[0]);
        assert!(
            close(x, 20.0 + cell_w + 20.0),
            "cell (1, 1) starts one margin, one cell and one gutter in"
        );
        assert!(close(y, 20.0 + cell_h + 20.0));
        assert!(close(w, cell_w) && close(h, cell_h));
    }

    /// A status bar takes a strip off one edge, and the grid is what is left. Both
    /// halves matter and neither implies the other: a grid that moved without
    /// shrinking would run off the far edge, and one that shrank without moving
    /// would be drawn under the bar.
    #[test]
    fn a_status_bar_moves_and_shrinks_the_grid_it_leaves() {
        const THICKNESS: u32 = 40;
        let bare = device(4, 3, Vec::new());

        for (edge, shift_x, shift_y) in [
            (Edge::Top, 0.0, THICKNESS as f32),
            (Edge::Bottom, 0.0, 0.0),
            (Edge::Left, THICKNESS as f32, 0.0),
            (Edge::Right, 0.0, 0.0),
        ] {
            let panel = device_with_bar(
                4,
                3,
                edge,
                THICKNESS,
                vec![widget("first", 0, 0, 1, 1), widget("last", 3, 2, 1, 1)],
            );
            let layout = Layout::for_device(&panel);
            let gutter = layout.gutter();
            let (area_x, area_y, area_w, area_h) = panel.grid_area();

            match edge {
                Edge::Top | Edge::Bottom => assert_eq!(
                    (area_w, area_h),
                    (bare.width, bare.height - THICKNESS),
                    "a {edge} bar takes its thickness off the height"
                ),
                Edge::Left | Edge::Right => assert_eq!(
                    (area_w, area_h),
                    (bare.width - THICKNESS, bare.height),
                    "a {edge} bar takes its thickness off the width"
                ),
            }
            assert_eq!(
                (area_x as f32, area_y as f32),
                (shift_x, shift_y),
                "a {edge} bar puts the grid's origin here"
            );

            let (x, y, ..) = layout.rect(&panel.widgets[0]);
            assert!(
                close(x, shift_x + gutter) && close(y, shift_y + gutter),
                "a {edge} bar must move the first cell to ({}, {}), not ({x}, {y})",
                shift_x + gutter,
                shift_y + gutter
            );

            // And the far corner still lands on the grid area's own margin, which
            // is what proves the grid shrank rather than merely slid.
            let (lx, ly, lw, lh) = layout.rect(&panel.widgets[1]);
            assert!(
                ((lx + lw) - ((area_x + area_w) as f32 - gutter)).abs() < 0.5,
                "a {edge} bar leaves the last column ending at {}, not {}",
                lx + lw,
                (area_x + area_w) as f32 - gutter
            );
            assert!(
                ((ly + lh) - ((area_y + area_h) as f32 - gutter)).abs() < 0.5,
                "a {edge} bar leaves the last row ending at {}, not {}",
                ly + lh,
                (area_y + area_h) as f32 - gutter
            );

            // A finger on the bar itself is on no cell. The bar is chrome, and a
            // tap there firing the nearest widget's action would be the worst
            // kind of surprise.
            let (bar_x, bar_y, bar_w, bar_h) = panel
                .status_bar_area()
                .expect("a device with a bar has a bar rect");
            assert_eq!(
                hit_id(
                    &panel,
                    &layout,
                    bar_x as f32 + bar_w as f32 / 2.0,
                    bar_y as f32 + bar_h as f32 / 2.0
                ),
                None,
                "a tap on a {edge} status bar must fire nothing"
            );
        }
    }

    /// A group's children sit in its content box, share one gutter, and take no
    /// margin of their own — the group's padding already is that margin.
    #[test]
    fn a_groups_children_fill_its_content_box() {
        let panel = grouped();
        let layout = Layout::for_device(&panel);
        let host = &panel.widgets[0];
        let group = host
            .group
            .as_ref()
            .expect("the fixture's first cell is a group");
        let sub = layout
            .sub_layout(host)
            .expect("a group has a sub-layout to place its children in");

        let (gx, gy, gw, gh) = layout.rect(host);
        let edge = layout.inset() / 2.0;

        let (first_x, first_y, ..) = sub.rect(&group.widgets[0]);
        assert!(
            close(first_x, gx + edge) && close(first_y, gy + edge),
            "the first child starts at the content box's own top-left"
        );

        let (last_x, last_y, last_w, last_h) = sub.rect(&group.widgets[1]);
        assert!(
            close(last_x + last_w, gx + gw - edge),
            "the last child ends at the content box's right edge, with no margin \
             of its own to hold it off"
        );
        assert!(close(last_y + last_h, gy + gh - edge));

        // Exactly one gutter between the two, not two and not none.
        let (_, _, first_w, _) = sub.rect(&group.widgets[0]);
        assert!(close(last_x - (first_x + first_w), layout.gutter()));

        assert!(
            layout.sub_layout(&panel.widgets[1]).is_none(),
            "an ordinary cell has no sub-grid to descend into"
        );
    }

    #[test]
    fn a_tap_inside_a_group_resolves_to_the_child_it_landed_on() {
        let panel = grouped();
        let layout = Layout::for_device(&panel);
        let host = &panel.widgets[0];
        let group = host
            .group
            .as_ref()
            .expect("the fixture's first cell is a group");
        let sub = layout.sub_layout(host).expect("a group has a sub-layout");

        for child in &group.widgets {
            let (x, y, w, h) = sub.rect(child);
            assert_eq!(
                hit_id(&panel, &layout, x + w / 2.0, y + h / 2.0).as_deref(),
                Some(child.id.as_str()),
                "the centre of child `{}` must resolve to it and not to the group",
                child.id
            );
        }

        // The cell beside the group is untouched by the descent.
        let (x, y) = centre(&layout, 3, 0);
        assert_eq!(hit_id(&panel, &layout, x, y).as_deref(), Some("plain"));
    }

    /// The fallback that distinguishes a group's inside from an outer gutter: a
    /// group is itself a cell, so a point in it that hit no child is still a point
    /// on a widget, and the group's own action is the honest answer.
    #[test]
    fn a_tap_between_a_groups_children_falls_back_to_the_group() {
        let panel = grouped();
        let layout = Layout::for_device(&panel);
        let host = &panel.widgets[0];
        let group = host
            .group
            .as_ref()
            .expect("the fixture's first cell is a group");
        let sub = layout.sub_layout(host).expect("a group has a sub-layout");

        let (gx, gy, _, gh) = layout.rect(host);
        let (first_x, first_y, first_w, first_h) = sub.rect(&group.widgets[0]);

        let cases: &[(&str, f32, f32)] = &[
            (
                "the gutter between two children",
                first_x + first_w + layout.gutter() / 2.0,
                first_y + first_h / 2.0,
            ),
            (
                "the group's own padding, inside its rule",
                gx + 1.0,
                gy + gh / 2.0,
            ),
            (
                "below a single-row sub-grid's children",
                first_x + first_w / 2.0,
                gy + gh - 1.0,
            ),
        ];

        for (what, x, y) in cases {
            assert_eq!(
                hit_id(&panel, &layout, *x, *y).as_deref(),
                Some("group"),
                "{what} is still inside the group, so it must resolve to the group"
            );
        }
    }

    /// The whole point of `fit = "content"`: a row of four readings and a row
    /// holding one figure are not the same height, and are no longer drawn as
    /// though they were.
    #[test]
    fn a_content_fit_grid_sizes_each_row_to_what_it_holds() {
        let panel = content_device(
            1,
            2,
            vec![listing("many", 0, 0, 1, 4), widget("one", 0, 1, 1, 1)],
        );
        let layout = Layout::for_device(&panel);
        let tracks = layout.row_tracks().to_vec();

        assert_eq!(tracks.len(), 2, "one track per row of the grid");
        assert!(
            tracks[0] > tracks[1],
            "four readings want more height than one figure, but the tracks are \
             {tracks:?}"
        );

        // The rects follow those tracks rather than an average of them, with one
        // gutter between the two.
        let (_, first_y, _, first_h) = layout.rect(&panel.widgets[0]);
        let (_, second_y, _, second_h) = layout.rect(&panel.widgets[1]);
        assert!(close(first_h, tracks[0]) && close(second_h, tracks[1]));
        assert!(
            close(second_y - (first_y + first_h), layout.gutter()),
            "one gutter between two tracks, no more and no less"
        );

        // The same widgets on a stretched grid are still given equal tracks, which
        // is the geometry every other test in this module pins.
        let stretched = Layout::for_device(&device(1, 2, panel.widgets.clone()));
        assert!(
            close(stretched.row_tracks()[0], stretched.row_tracks()[1])
                && close(stretched.row_tracks()[0], stretched.cell().1),
            "a stretched grid's tracks are equal, and each is the cell height"
        );
    }

    /// An over-subscribed grid is scaled to fit rather than allowed to run off the
    /// glass. A dashboard that is uniformly a little tight can still be read; one
    /// whose last row is drawn past the bottom of the frame cannot be.
    #[test]
    fn content_tracks_never_sum_past_the_grid_area() {
        // Eight rows of eight readings on a panel with room for nothing like that
        // much: every track asks for several times its share.
        let widgets = (0..8u32)
            .map(|row| listing(&format!("row{row}"), 0, row, 1, 8))
            .collect::<Vec<_>>();
        let panel = content_device(1, 8, widgets);
        let layout = Layout::for_device(&panel);
        let tracks = layout.row_tracks();
        let gutter = layout.gutter();
        let (_, _, _, area_h) = panel.grid_area();

        let capacity = area_h as f32 - gutter * (tracks.len() + 1) as f32;
        let wanted = tracks.iter().sum::<f32>();
        assert!(
            wanted <= capacity + 0.01,
            "eight over-subscribed tracks sum to {wanted}, past the {capacity} the \
             grid area has to give"
        );

        let last = panel.widgets.last().expect("the fixture places eight rows");
        let (_, y, _, h) = layout.rect(last);
        assert!(
            y + h <= panel.height as f32 - gutter + 0.01,
            "the last row ends at {}, past the frame's own bottom margin at {}",
            y + h,
            panel.height as f32 - gutter
        );
    }

    /// A `fill` widget takes the height the content left over, and without one that
    /// height stays where it is: unused, at the foot of the grid. A dashboard that
    /// silently inflated its last row to fill the frame would be stretching under
    /// another name.
    #[test]
    fn the_leftover_goes_to_the_filling_widget_or_to_nobody() {
        let widgets = vec![widget("top", 0, 0, 1, 1), widget("bottom", 0, 1, 1, 1)];
        let bare = content_device(1, 2, widgets.clone());
        let unclaimed = Layout::for_device(&bare);
        let tracks = unclaimed.row_tracks().to_vec();
        let gutter = unclaimed.gutter();
        let (_, _, _, area_h) = bare.grid_area();
        let leftover = area_h as f32 - gutter * 3.0 - tracks.iter().sum::<f32>();
        assert!(
            leftover > 1.0,
            "two figures on a 1072 pixel panel must leave something over, not \
             {leftover}"
        );

        let (_, y, _, h) = unclaimed.rect(&bare.widgets[1]);
        assert!(
            close(bare.height as f32 - gutter - (y + h), leftover),
            "an unclaimed leftover stays as margin at the foot of the grid"
        );

        let mut filling = widgets;
        filling[0].fill = true;
        let panel = content_device(1, 2, filling);
        let layout = Layout::for_device(&panel);
        let filled = layout.row_tracks();
        assert!(
            close(filled[0], tracks[0] + leftover) && close(filled[1], tracks[1]),
            "the whole leftover goes to the row the filling widget covers: {filled:?} \
             against {tracks:?} plus {leftover}"
        );

        let (_, y, _, h) = layout.rect(&panel.widgets[1]);
        assert!(
            close(y + h, panel.height as f32 - gutter),
            "with the leftover claimed, the last row ends on the frame's own bottom \
             margin"
        );
    }

    /// The agreement `rect` exists for, on a grid where no two tracks are the same
    /// height: every widget's own rect resolves to that widget, edges included. A
    /// cumulative sum is exactly where an off-by-one-track error hides, and such an
    /// error is a finger on one reading firing another cell's action.
    #[test]
    fn rect_and_hit_agree_on_a_content_fit_grid() {
        let panel = content_device(
            2,
            3,
            vec![
                listing("many", 0, 0, 1, 4),
                widget("figure", 1, 0, 1, 1),
                listing("tall", 0, 1, 2, 2),
                listing("mid", 1, 1, 1, 3),
                widget("corner", 1, 2, 1, 1),
            ],
        );
        let layout = Layout::for_device(&panel);
        let tracks = layout.row_tracks();
        assert!(
            tracks[0] > tracks[1] && tracks[1] > tracks[2],
            "the fixture must give all three rows different heights or it proves \
             nothing: {tracks:?}"
        );

        for widget in &panel.widgets {
            let (x, y, w, h) = layout.rect(widget);
            for (what, at_y) in [
                ("its own centre", y + h / 2.0),
                ("its top edge", y),
                ("the pixel inside its bottom edge", y + h - 0.01),
            ] {
                assert_eq!(
                    hit_id(&panel, &layout, x + w / 2.0, at_y).as_deref(),
                    Some(widget.id.as_str()),
                    "{what} must resolve to `{}`",
                    widget.id
                );
            }
        }
    }
}
