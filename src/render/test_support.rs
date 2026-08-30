//! Shared fixtures for the render pipeline's tests: devices, widgets, and the
//! pixel-probing helpers that read a rasterised frame back into assertions.
//!
//! Kept in one file rather than duplicated per module, because a `Device` or a
//! `Widget` built for one module's tests is the same fixture another module's
//! tests need — `panel`, `device` and `record` alone are used by every test
//! module in this tree.

use std::collections::HashMap;
use std::sync::LazyLock;

use time::OffsetDateTime;

use crate::config::{
    Chrome, DEFAULT_MAX_FRAME_BYTES, Device, Dither, Edge, Fit, Grid, Group, Palette, Widget,
    WidgetKind,
};
use crate::content::{ContentRecord, Row};
use crate::ha::{Reading, Reported};
use crate::icon::Icon;
use crate::telemetry::Telemetry;

use super::body::{figure_node, fit_size, intrinsic_width};
use super::resolve::resolve;
use super::types::{Cell, Greys, Ink, Line};
use super::*;

/// One font collection for the whole test module: registration is the
/// expensive part and it is immutable once built.
pub(super) static FONTS: LazyLock<Fonts> =
    LazyLock::new(|| fonts().expect("embedded fonts must load"));

/// The shipped look, which is what almost every test here is about: the point of
/// the style table is that a configuration saying nothing renders what it always
/// did, so the fixtures assert against the defaults.
pub(super) static STYLE: crate::config::Style = crate::config::Style::SHIPPED;

pub(super) const GREYS: Greys = Greys {
    ink: 0,
    muted: 102,
    rule: 170,
};

/// A dashboard nobody has pushed to, for the geometry tests: a rect, a gutter
/// and a rule are what a configuration says they are, and none of them moves
/// because a publisher spoke.
pub(super) fn nothing_pushed() -> HashMap<String, ContentRecord> {
    HashMap::new()
}

pub(super) fn device(widgets: Vec<Widget>) -> Device {
    const GRID: Grid = Grid {
        cols: 2,
        rows: 2,
        fit: Fit::Stretch,
    };
    Device {
        id: "kindle".to_owned(),
        width: 400,
        height: 300,
        palette: Palette::Gray16,
        dither: Dither::Atkinson,
        refresh_rate: 300,
        render_interval: 300,
        max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
        grid: GRID,
        style: crate::config::Style::default(),
        chrome: Chrome::derived(400, 300, GRID),
        status_bar: None,
        widgets,
        sink: None,
    }
}

pub(super) fn widget(id: &str, kind: WidgetKind, col: u32, row: u32) -> Widget {
    Widget {
        id: id.to_owned(),
        kind,
        col,
        row,
        col_span: 1,
        row_span: 1,
        label: Some(id.to_owned()),
        unit: None,
        precision: None,
        state_text: true,
        stale_after: 0,
        entity: None,
        attribute: None,
        on_values: vec!["on".to_owned(), "true".to_owned(), "alert".to_owned()],
        icon: None,
        icon_on: None,
        icon_off: None,
        readings: Vec::new(),
        group: None,
        fill: false,
        style: crate::config::Style::default(),
        tap: None,
    }
}

/// A panel exercising every part of the style table at once: a bar, a figure with
/// a unit, a weather cell with its glyph and its readings, and a list.
///
/// One fixture rather than one per key, because the property being asserted is
/// about the table as a whole — that no key on it is decorative.
pub(super) fn styled_panel(
    style: crate::config::Style,
) -> (
    Device,
    HashMap<String, ContentRecord>,
    HashMap<Reading, Reported>,
) {
    const GRID: Grid = Grid {
        cols: 3,
        rows: 1,
        fit: Fit::Stretch,
    };
    let mut figure = widget("figure", WidgetKind::Value, 0, 0);
    figure.unit = Some("\u{b0}C".to_owned());
    figure.style = style;

    let mut sky = ha_widget("sky", "weather.home", WidgetKind::Weather);
    sky.col = 1;
    sky.readings = vec![reading("Temp", "weather.home", Some("\u{b0}C"))];
    sky.readings[0].attribute = Some("temperature".to_owned());
    sky.style = style;

    let mut list = widget("rooms", WidgetKind::List, 2, 0);
    list.readings = ["Office", "Hall"]
        .iter()
        .map(|label| reading(label, "sensor.room", Some("\u{b0}C")))
        .collect();
    list.style = style;

    let device = Device {
        grid: GRID,
        chrome: Chrome::derived(400, 300, GRID),
        style,
        status_bar: Some(crate::config::StatusBar {
            edge: Edge::Bottom,
            thickness: 40,
            fields: vec![
                crate::config::StatusField::Date,
                crate::config::StatusField::Battery,
            ],
            alerts: Vec::new(),
            timezone: crate::config::Timezone::utc(),
        }),
        ..device(vec![figure, sky, list])
    };
    let states = HashMap::from([
        (
            Reading::state("weather.home"),
            Reported::Fresh("partlycloudy".to_owned()),
        ),
        (
            Reading::attribute("weather.home", "temperature"),
            Reported::Fresh("19.5".to_owned()),
        ),
        (
            Reading::state("sensor.room"),
            Reported::Fresh("21.4".to_owned()),
        ),
    ]);
    // The figure cell needs a figure: with nothing pushed it renders `no data`,
    // and an absence has no unit for `unit_scale` to move.
    let content = HashMap::from([(
        "figure".to_owned(),
        record(serde_json::json!("21.4"), now()),
    )]);
    (device, content, states)
}

/// An `ha_entity` widget reading `entity`'s own state.
pub(super) fn ha_widget(id: &str, entity: &str, kind: WidgetKind) -> Widget {
    Widget {
        entity: Some(entity.to_owned()),
        ..widget(id, kind, 0, 0)
    }
}

pub(super) fn record(value: serde_json::Value, received_at: OffsetDateTime) -> ContentRecord {
    ContentRecord {
        value,
        state: None,
        unit: None,
        rows: None,
        received_at,
    }
}

pub(super) fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

pub(super) fn render(device: &Device, content: &HashMap<String, ContentRecord>) -> Vec<u8> {
    render_with(device, content, &HashMap::new(), &HashMap::new())
}

pub(super) fn render_with(
    device: &Device,
    content: &HashMap<String, ContentRecord>,
    ha_states: &HashMap<Reading, Reported>,
    icons: &HashMap<String, Icon>,
) -> Vec<u8> {
    render_frame(
        &FONTS,
        RenderInputs {
            device,
            content,
            ha_states,
            icons,
            now: now(),
            telemetry: &Telemetry::default(),
        },
    )
    .expect("frame should render")
}

/// The cell one widget resolves to, given only Home Assistant readings.
pub(super) fn resolved(widget: &Widget, ha_states: &HashMap<Reading, Reported>) -> Cell {
    let device = device(vec![widget.clone()]);
    resolve(
        widget,
        &RenderInputs {
            device: &device,
            content: &HashMap::new(),
            ha_states,
            icons: &HashMap::new(),
            now: now(),
            telemetry: &Telemetry::default(),
        },
    )
}

/// The cell one widget resolves to, given pushed content and resolved icons.
pub(super) fn resolved_push(
    widget: &Widget,
    content: &HashMap<String, ContentRecord>,
    icons: &HashMap<String, Icon>,
) -> Cell {
    let device = device(vec![widget.clone()]);
    resolve(
        widget,
        &RenderInputs {
            device: &device,
            content,
            ha_states: &HashMap::new(),
            icons,
            now: now(),
            telemetry: &Telemetry::default(),
        },
    )
}

/// One configured reading, as a `list` or a `weather` cell hangs off it.
pub(super) fn reading(
    label: &str,
    entity: &str,
    attribute: Option<&str>,
) -> crate::config::Reading {
    crate::config::Reading {
        label: Some(label.to_owned()),
        icon: None,
        entity: entity.to_owned(),
        attribute: attribute.map(str::to_owned),
        unit: Some("\u{b0}C".to_owned()),
        precision: Some(1),
    }
}

/// One resolved line, spelt the way `resolve` builds it.
///
/// Not called `line`: the module already has a `line` for grid placement, and a
/// test helper shadowing it is a trap for whoever writes the next placement test.
pub(super) fn resolved_line(
    label: &str,
    value: serde_json::Value,
    unit: Option<&str>,
    ink: Ink,
) -> Line {
    Line {
        row: Row {
            id: None,
            label: Some(label.to_owned()),
            value: Some(value),
            unit: unit.map(str::to_owned),
            state: None,
        },
        icon: None,
        ink,
    }
}

pub(super) fn dimensions(png: &[u8]) -> (u32, u32) {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let reader = decoder.read_info().expect("should be a PNG");
    (reader.info().width, reader.info().height)
}

/// The lowest inked row per column of a rasterised node, split into the run
/// left of the widest internal gap and the run right of it.
///
/// The baseline is taken as the *mode* of those rows rather than the maximum, so
/// a descender, a decimal point or a curve's overshoot does not move it.
pub(super) fn baselines(node: Node, width: u32, height: u32) -> (u32, u32) {
    let raster = rasterise(&FONTS, node, width, height).expect("should rasterise");
    let dark = |x: u32, y: u32| raster[((y * width + x) * 4 + 3) as usize] > 128;

    let mut lowest = Vec::new();
    for x in 0..width {
        let low = (0..height).rfind(|&y| dark(x, y));
        lowest.push(low);
    }

    // The widest run of empty columns between two inked ones is the gap between
    // the figure and its unit.
    let inked: Vec<u32> = (0..width)
        .filter(|&x| lowest[x as usize].is_some())
        .collect();
    assert!(inked.len() > 4, "nothing was drawn");
    let (mut split, mut widest) = (0, 0);
    for pair in inked.windows(2) {
        let gap = pair[1] - pair[0];
        if gap > widest {
            widest = gap;
            split = pair[0] + gap / 2;
        }
    }
    assert!(widest > 1, "the figure and its unit are not separable");

    let mode = |from: u32, to: u32| {
        let mut counts: HashMap<u32, usize> = HashMap::new();
        for x in from..to {
            if let Some(y) = lowest[x as usize] {
                *counts.entry(y).or_default() += 1;
            }
        }
        *counts
            .iter()
            .max_by_key(|(y, n)| (*n, std::cmp::Reverse(**y)))
            .expect("a run must have ink")
            .0
    };
    (mode(0, split), mode(split, width))
}

/// How much of a widget's cell its content actually inks, as fractions of the
/// cell's width and height.
///
/// Measured off the rasterised frame rather than from the style declarations,
/// because the number that matters is the one on the glass: every size in a cell
/// is chosen by measurement and fitting, so nothing short of pixels says whether
/// a reading used the cell it was given.
pub(super) fn cell_fill(
    device: &Device,
    widget_id: &str,
    content: &HashMap<String, ContentRecord>,
    ha: &HashMap<Reading, Reported>,
) -> (f32, f32) {
    let widget = device
        .widgets
        .iter()
        .find(|w| w.id == widget_id)
        .expect("the widget must be on the device");
    let inputs = RenderInputs {
        device,
        content,
        ha_states: ha,
        icons: &HashMap::new(),
        now: now(),
        telemetry: &Telemetry::default(),
    };
    let raster = rasterise(
        &FONTS,
        dashboard_node(&FONTS, &inputs),
        device.width,
        device.height,
    )
    .expect("should rasterise");

    let (x, y, w, h) = Layout::for_device(device, &nothing_pushed()).rect(widget);
    // Inset past the cell's own rule, which would otherwise ink every cell fully.
    let (x0, y0) = ((x + 3.0) as u32, (y + 3.0) as u32);
    let (x1, y1) = ((x + w - 3.0) as u32, (y + h - 3.0) as u32);
    let dark = |x: u32, y: u32| raster[((y * device.width + x) * 4) as usize] < 128;

    let inked_x: Vec<u32> = (x0..x1).filter(|&x| (y0..y1).any(|y| dark(x, y))).collect();
    let inked_y: Vec<u32> = (y0..y1).filter(|&y| (x0..x1).any(|x| dark(x, y))).collect();
    assert!(
        !inked_x.is_empty() && !inked_y.is_empty(),
        "widget `{widget_id}` inked nothing"
    );
    (
        (inked_x[inked_x.len() - 1] - inked_x[0] + 1) as f32 / (x1 - x0) as f32,
        (inked_y[inked_y.len() - 1] - inked_y[0] + 1) as f32 / (y1 - y0) as f32,
    )
}

/// The panel in service: a 6-inch Kindle Paperwhite, landscape, on a 4x3 grid.
///
/// `chrome` is re-derived rather than inherited from [`device`]: it is scaled to
/// the cell, so a device that overrides the dimensions and the grid but keeps the
/// 400x300 spacing is testing a dashboard no configuration can produce.
pub(super) fn panel(widgets: Vec<Widget>) -> Device {
    const GRID: Grid = Grid {
        cols: 4,
        rows: 3,
        fit: Fit::Stretch,
    };
    Device {
        width: 1448,
        height: 1072,
        grid: GRID,
        chrome: Chrome::derived(1448, 1072, GRID),
        ..device(widgets)
    }
}

/// The size a figure settles on inside a 200px-wide box, measured with the real
/// font metrics rather than estimated from a character count.
pub(super) fn figure_px_for(text: &str, unit: Option<&str>) -> f32 {
    const DESIGN: f32 = 96.0;
    let intrinsic = intrinsic_width(
        &FONTS,
        figure_node(text, unit, Ink::Current, DESIGN, &STYLE, GREYS),
    );
    assert!(intrinsic > 0.0, "measuring {text:?} must produce a width");
    fit_size(intrinsic, 200.0, DESIGN)
}

/// The palette's top level: paper, on a 16-level greyscale panel.
pub(super) const PAPER: u8 = 15;

/// The frame's palette levels, one per pixel, read back out of the encoded PNG.
///
/// Read off the frame the panel is actually handed rather than tapped out of the
/// rasteriser, and exact rather than approximate: a 16-level greyscale panel
/// rendered with [`Dither::None`] maps paper onto the top level and a cell's rule
/// onto the level nearest its grey, so a probe distinguishes the two with no
/// tolerance to tune.
pub(super) fn greys(png: &[u8]) -> (u32, u32, Vec<u8>) {
    let decoder = png::Decoder::new(std::io::Cursor::new(png));
    let mut reader = decoder.read_info().expect("should be a PNG");
    assert_eq!(
        reader.info().bit_depth,
        png::BitDepth::Four,
        "these probes read a 16-level greyscale frame"
    );
    let mut packed = vec![0; reader.output_buffer_size().expect("a bounded frame")];
    let info = reader.next_frame(&mut packed).expect("frame should decode");

    // Two pixels per byte, leftmost in the high nibble, every row padded to a byte
    // boundary — so the row stride is read from the decoder rather than computed
    // from the width.
    let mut levels = Vec::with_capacity((info.width * info.height) as usize);
    for y in 0..info.height as usize {
        let row = &packed[y * info.line_size..][..info.line_size];
        for x in 0..info.width as usize {
            levels.push(if x % 2 == 0 {
                row[x / 2] >> 4
            } else {
                row[x / 2] & 0x0F
            });
        }
    }
    (info.width, info.height, levels)
}

/// Whether anything inside a rect is darker than paper.
pub(super) fn inked(levels: &[u8], width: u32, rect: (f32, f32, f32, f32)) -> bool {
    let (x, y, w, h) = rect;
    let (x0, y0) = (x.max(0.0) as u32, y.max(0.0) as u32);
    let (x1, y1) = ((x + w).ceil() as u32, (y + h).ceil() as u32);
    (y0..y1).any(|y| {
        (x0..x1).any(|x| {
            let index = (y * width + x) as usize;
            index < levels.len() && levels[index] < PAPER
        })
    })
}

/// How many separate runs of ink cross the one-pixel band at `y`, between `x0`
/// and `x1`.
///
/// Aimed at a row of cells' top rules, where it counts the cells themselves
/// rather than standing in for them: a cell's content sits a padding inside its
/// own rule, so that row holds the rules and nothing else. The band covers every
/// row the rule can touch, which is two of them when it falls at a fractional
/// offset and is antialiased across both.
pub(super) fn rule_runs(levels: &[u8], width: u32, y: f32, x0: f32, x1: f32) -> usize {
    let band = y.floor() as u32..(y + 1.0).ceil() as u32;
    let mut runs = 0;
    let mut inside = false;
    for x in x0.floor() as u32..x1.ceil() as u32 {
        let ink = band
            .clone()
            .any(|y| levels[(y * width + x) as usize] < PAPER);
        runs += usize::from(ink && !inside);
        inside = ink;
    }
    runs
}

/// A 2x1 group filling the panel's top-left slot, its two children each holding
/// a pushed reading.
///
/// `title` is the *group's* own label. The children always carry theirs, because
/// a child of a group is an ordinary cell and draws its own header.
pub(super) fn grouped_panel(title: Option<&str>) -> (Device, HashMap<String, ContentRecord>) {
    let mut group = widget("indoors", WidgetKind::Group, 0, 0);
    group.label = title.map(str::to_owned);
    group.group = Some(Group {
        grid: Grid {
            cols: 2,
            rows: 1,
            fit: Fit::Stretch,
        },
        widgets: vec![
            widget("left", WidgetKind::Value, 0, 0),
            widget("right", WidgetKind::Value, 1, 0),
        ],
    });
    let device = Device {
        // Both dithering modes texture flat tone, which is exactly what these
        // probes measure. Without dithering paper stays paper and a rule stays a
        // rule, everywhere.
        dither: Dither::None,
        ..panel(vec![group])
    };
    let content = HashMap::from([
        ("left".to_owned(), record(serde_json::json!("21.4"), now())),
        ("right".to_owned(), record(serde_json::json!("48"), now())),
    ]);
    (device, content)
}

/// A band-counting probe: how many separate horizontal strips of ink lie inside
/// a rect.
///
/// One band per line of text, so it counts *lines* — which is what tells a
/// column of three readings apart from three readings where one of them wrapped
/// onto a second line and printed over its neighbour.
pub(super) fn ink_bands(levels: &[u8], width: u32, rect: (f32, f32, f32, f32)) -> usize {
    let (x, y, w, h) = rect;
    let (x0, x1) = (x.max(0.0) as u32, (x + w).ceil() as u32);
    let mut bands = 0;
    let mut inside = false;
    for y in y.max(0.0) as u32..(y + h).ceil() as u32 {
        let ink = (x0..x1).any(|x| {
            let index = (y * width + x) as usize;
            index < levels.len() && levels[index] < PAPER
        });
        bands += usize::from(ink && !inside);
        inside = ink;
    }
    bands
}

/// A panel whose only cell is a `list` of three readings, on a grid dense enough
/// that the readings have to be fitted to the width they are given.
///
/// Sized as the second panel in service is — 800x600 on a 4x3 grid, so a cell is
/// about 184 pixels of content wide. That is the width a reading with no
/// `precision` set overflows even at [`MIN_TYPE_PX`], which is the case worth
/// having a fixture for.
pub(super) fn listed_panel(values: [&str; 3]) -> (Device, HashMap<Reading, Reported>) {
    const GRID: Grid = Grid {
        cols: 4,
        rows: 3,
        fit: Fit::Stretch,
    };
    let mut list = widget("climate", WidgetKind::List, 0, 0);
    list.label = Some("Climate".to_owned());
    // No `precision`, deliberately: rounding is what keeps a reading short, and
    // the case worth a fixture is the author who has not configured any.
    list.readings = ["Office", "Bedroom", "Hall"]
        .iter()
        .zip(["sensor.office", "sensor.bedroom", "sensor.hall"])
        .map(|(label, entity)| crate::config::Reading {
            precision: None,
            ..reading(label, entity, None)
        })
        .collect();
    let device = Device {
        dither: Dither::None,
        width: 800,
        height: 600,
        grid: GRID,
        chrome: Chrome::derived(800, 600, GRID),
        ..panel(vec![list])
    };
    let states = HashMap::from([
        (
            Reading::state("sensor.office"),
            Reported::Fresh(values[0].to_owned()),
        ),
        (
            Reading::state("sensor.bedroom"),
            Reported::Fresh(values[1].to_owned()),
        ),
        (
            Reading::state("sensor.hall"),
            Reported::Fresh(values[2].to_owned()),
        ),
    ]);
    (device, states)
}

/// A panel whose bar carries one alert, rendered with `pushed` in the store.
pub(super) fn alerting_frame(pushed: Option<&str>) -> Vec<u8> {
    let text = r#"
[server]
listen = "0.0.0.0:4444"
public_base_url = "http://192.168.0.50:4444"

[[device]]
id = "kindle"
width = 400
height = 300
palette = "gray16"
dither = "none"
refresh_rate = 300
grid = { cols = 2, rows = 1 }

[device.status_bar]
edge = "bottom"
fields = ["date"]

[[device.status_bar.alert]]
id = "post"
label = "MAIL"

[[device.widget]]
id = "a"
kind = "value"
col = 0
row = 0

[[device.widget]]
id = "b"
kind = "value"
col = 1
row = 0
"#;
    let device = &crate::config::parse(text)
        .expect("the fixture must be valid")
        .devices[0];
    let mut content = HashMap::from([("a".to_owned(), record(serde_json::json!("21.4"), now()))]);
    if let Some(state) = pushed {
        content.insert(
            "post".to_owned(),
            ContentRecord {
                value: serde_json::Value::String(state.to_owned()),
                state: Some(state.to_owned()),
                unit: None,
                rows: None,
                received_at: now(),
            },
        );
    }
    render(device, &content)
}

/// A pushed record of `count` labelled rows: what a publisher sends for a list
/// nobody could have written into a config file.
pub(super) fn rows_record(count: usize) -> ContentRecord {
    ContentRecord {
        rows: Some(
            (0..count)
                .map(|index| Row {
                    id: None,
                    label: Some(format!("item {index}")),
                    value: Some(serde_json::json!(index)),
                    unit: None,
                    state: None,
                })
                .collect(),
        ),
        ..record(serde_json::Value::Null, now())
    }
}

/// A ticket: a heading and a pushed list of `count` rows on a printhead-wide
/// frame `height` long, with the rows pinned to 20px and set edge to edge.
///
/// The palette is the one these probes read rather than the `mono` a printer
/// takes, because the two differ in tone and not in geometry.
pub(super) fn ticket(count: usize, height: u32) -> (Device, HashMap<String, ContentRecord>) {
    const GRID: Grid = Grid {
        cols: 1,
        rows: 2,
        fit: Fit::Content,
    };
    let mut head = widget("head", WidgetKind::Value, 0, 0);
    head.label = Some("Ticket".to_owned());
    let mut items = widget("items", WidgetKind::List, 0, 1);
    // A list that declares no reading is fed by push, and a leaf cell's header is
    // the mark's home rather than a title, so this one has no label to draw.
    items.label = None;
    items.style = crate::config::Style {
        row_type: 20.0,
        row_width: crate::config::RowWidth::Full,
        ..crate::config::Style::SHIPPED
    };
    let device = Device {
        width: 384,
        height,
        grid: GRID,
        chrome: Chrome::derived(384, height, GRID),
        widgets: vec![head, items],
        ..device(Vec::new())
    };
    let content = HashMap::from([
        (
            "head".to_owned(),
            record(serde_json::json!("Groceries"), now()),
        ),
        ("items".to_owned(), rows_record(count)),
    ]);
    (device, content)
}

/// How much paper a frame costs: the row after its last inked one, which is
/// exactly what the sink's trailing-blank trim leaves on the roll.
pub(super) fn paper(png: &[u8]) -> u32 {
    let (width, height, levels) = greys(png);
    (0..height)
        .rev()
        .find(|&y| inked(&levels, width, (0.0, y as f32, width as f32, 1.0)))
        .map_or(0, |y| y + 1)
}
