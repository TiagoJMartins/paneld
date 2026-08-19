//! The render pipeline: a device's configuration plus whatever data is available
//! in, encoded PNG frame bytes out.
//!
//! [`render_frame`] is a pure synchronous function, which is the whole point of
//! its shape. Home Assistant states are fetched by the caller and handed in
//! already resolved, so nothing here awaits, nothing here reads a clock, and the
//! same inputs always produce the same bytes. That determinism is load-bearing
//! rather than tidy: the device caches frames by filename, the filename is a hash
//! of these bytes, and a non-deterministic encoder would make the panel repaint
//! on every poll.

mod encode;

pub use encode::{MAX_FRAME_BYTES, frame_hash, quantise_and_encode};

use std::collections::HashMap;

use anyhow::{Context, Result, anyhow};
use takumi::prelude::*;
use time::OffsetDateTime;

use crate::config::{Device, Dither, Palette, Widget, WidgetKind};
use crate::content::{ContentRecord, Row};

/// Family name the UI text is registered and requested under.
///
/// Pinned by us rather than taken from the font file, so a vendor renaming their
/// embedded family string cannot silently change what we render with.
pub const UI_FAMILY: &str = "Panel UI";

/// Family name for numeric readouts, where tabular figures stop a changing value
/// from shifting its neighbours around.
pub const NUMERIC_FAMILY: &str = "Panel Numeric";

const INTER_REGULAR: &[u8] = include_bytes!("../assets/fonts/Inter-Regular.ttf");
const INTER_BOLD: &[u8] = include_bytes!("../assets/fonts/Inter-Bold.ttf");
const MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Regular.ttf");
const MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/IBMPlexMono-Bold.ttf");

/// Gutter and cell padding, scaled to the cell rather than fixed.
///
/// Fixed spacing is a trap on a small panel or a dense grid: once padding exceeds
/// the cell it is inside, the text layout engine is handed a negative content box
/// and panics. Scaling keeps the content box positive for every grid a validated
/// config can express.
#[derive(Debug, Clone, Copy)]
struct Spacing {
    /// Between cells, and around the frame.
    gutter: f32,
    /// Inside a cell.
    padding: f32,
}

impl Spacing {
    fn for_cell(smallest_side: f32) -> Self {
        Self {
            gutter: (smallest_side * 0.06).clamp(1.0, 10.0),
            padding: (smallest_side * 0.10).clamp(2.0, 12.0),
        }
    }
}

/// Everything one frame is rendered from.
pub struct RenderInputs<'a> {
    pub device: &'a Device,
    /// Pushed content, keyed by widget id.
    pub content: &'a HashMap<String, ContentRecord>,
    /// Home Assistant entity states, keyed by entity id, fetched by the caller so
    /// that rendering itself stays pure and synchronous. A per-entity `Err`
    /// degrades that one cell.
    pub ha_states: &'a HashMap<String, Result<String, String>>,
    /// The instant staleness is measured against. A parameter rather than a call
    /// to `now_utc()` so a frame is reproducible.
    pub now: OffsetDateTime,
}

/// Builds the embedded font collection.
///
/// Fonts are compiled into the binary and never read from the filesystem, so
/// rendering does not depend on system font configuration. takumi constructs its
/// font collection with system font discovery disabled, so there is no host
/// fallback to accidentally rely on.
pub fn fonts() -> Result<Fonts> {
    let mut fonts = Fonts::default();

    // Registration order matters and is fixed here on purpose: takumi builds its
    // fallback bucket from registration order precisely because iterating a
    // hash map would make font selection vary between renders.
    for (bytes, family, weight) in [
        (INTER_REGULAR, UI_FAMILY, 400.0),
        (INTER_BOLD, UI_FAMILY, 700.0),
        (MONO_REGULAR, NUMERIC_FAMILY, 400.0),
        (MONO_BOLD, NUMERIC_FAMILY, 700.0),
    ] {
        fonts
            .register(
                FontResource::new(FontSource::from_static(bytes)).override_info(FontOverride {
                    family_name: Some(family.into()),
                    weight: Some(weight),
                    ..Default::default()
                }),
            )
            .map_err(|e| anyhow!("registering the embedded {family} {weight} face: {e}"))?;
    }
    Ok(fonts)
}

/// Renders one device's dashboard to encoded PNG bytes.
pub fn render_frame(fonts: &Fonts, inputs: RenderInputs<'_>) -> Result<Vec<u8>> {
    let device = inputs.device;
    let node = dashboard_node(&inputs);
    let raster = rasterise(fonts, node, device.width, device.height)?;
    quantise_and_encode(
        &raster,
        device.width,
        device.height,
        device.palette,
        device.dither,
    )
    .with_context(|| format!("encoding the frame for device `{}`", device.id))
}

/// Renders the frame served when a poll names a device that is not configured.
///
/// Because every configured device is rendered at startup, a known device always
/// has a real frame; reaching this means the base URL is wrong. So the frame's
/// whole job is to make that diagnosable on the panel itself, which is the only
/// screen the owner is looking at.
pub fn render_placeholder(
    fonts: &Fonts,
    requested: &str,
    configured: &[String],
    width: u32,
    height: u32,
    palette: Palette,
    dither: Dither,
) -> Result<Vec<u8>> {
    // The requested dimensions come from device-supplied headers, so they are
    // clamped: a malformed poll must not be able to ask for an enormous buffer.
    let width = width.clamp(1, crate::config::MAX_DIMENSION);
    let height = height.clamp(1, crate::config::MAX_DIMENSION);

    let body = if configured.is_empty() {
        "No devices are configured on this server.".to_owned()
    } else {
        format!("Configured device ids: {}", configured.join(", "))
    };
    let scale = (height as f32 / 480.0).clamp(0.5, 2.5);

    let node = Node::container(vec![
        text_node("Unknown device", text_style(28.0 * scale, 700.0, UI_FAMILY)),
        text_node(
            &truncate(requested, 64),
            text_style(40.0 * scale, 700.0, NUMERIC_FAMILY),
        ),
        text_node(&body, text_style(20.0 * scale, 400.0, UI_FAMILY)),
        text_node(
            "This server does not serve that id. Check the base URL on the device.",
            text_style(17.0 * scale, 400.0, UI_FAMILY),
        ),
    ])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::justify_content(JustifyContent::Center))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::width(Length::Px(width as f32)))
            .with(StyleDeclaration::height(Length::Px(height as f32)))
            .with(StyleDeclaration::padding_left(Length::Px(32.0)))
            .with(StyleDeclaration::padding_right(Length::Px(32.0)))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                14.0 * scale,
            ))))
            .with(StyleDeclaration::background_color(paper()))
            .with(StyleDeclaration::color(ink()))
            .with(StyleDeclaration::text_align(TextAlign::Center))
            .with(StyleDeclaration::font_family(family(UI_FAMILY))),
    );

    let raster = rasterise(fonts, node, width, height)?;
    quantise_and_encode(&raster, width, height, palette, dither)
        .context("encoding the unknown-device placeholder")
}

/// Rasterises a node tree to straight-alpha RGBA bytes.
fn rasterise(fonts: &Fonts, node: Node, width: u32, height: u32) -> Result<Vec<u8>> {
    let options = RenderOptions::builder()
        // `device_pixel_ratio` is left at its default of 1.0: it multiplies into
        // the output size, and the frame must be exactly the panel's resolution
        // so it is neither letterboxed nor scaled.
        .viewport(Viewport::new((width, height)))
        .node(node)
        .fonts(fonts)
        .build();

    let bitmap = takumi::render(options).map_err(|e| anyhow!("rasterising: {e}"))?;
    anyhow::ensure!(
        (bitmap.width(), bitmap.height()) == (width, height),
        "rasteriser produced {}x{}, expected {width}x{height}",
        bitmap.width(),
        bitmap.height()
    );
    Ok(bitmap.into_raw())
}

/// The whole dashboard: a CSS grid container with one child per widget.
fn dashboard_node(inputs: &RenderInputs<'_>) -> Node {
    let device = inputs.device;
    let grid = device.grid;

    let spacing = Spacing::for_cell(
        (device.width as f32 / grid.cols as f32).min(device.height as f32 / grid.rows as f32),
    );

    // Usable cell size, needed to scale type to the cell rather than fixing it.
    let cell_w =
        (device.width as f32 - spacing.gutter * (grid.cols + 1) as f32).max(1.0) / grid.cols as f32;
    let cell_h =
        (device.height as f32 - spacing.gutter * (grid.rows + 1) as f32).max(1.0) / grid.rows as f32;

    let children = device
        .widgets
        .iter()
        .map(|widget| {
            let body = resolve(widget, inputs);
            cell_node(widget, &body, cell_w, cell_h, spacing)
        })
        .collect::<Vec<_>>();

    Node::container(children).with_style(
        Style::default()
            // `Display`'s default is `Inline`, so a container that should lay out
            // as a grid must say so or its children are laid out inline and every
            // grid placement is silently ignored.
            .with(StyleDeclaration::display(Display::Grid))
            .with(StyleDeclaration::width(Length::Px(device.width as f32)))
            .with(StyleDeclaration::height(Length::Px(device.height as f32)))
            .with(StyleDeclaration::grid_template_columns(Some(equal_tracks(
                grid.cols,
            ))))
            .with(StyleDeclaration::grid_template_rows(Some(equal_tracks(
                grid.rows,
            ))))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                spacing.gutter,
            ))))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                spacing.gutter,
            ))))
            .with(StyleDeclaration::padding_top(Length::Px(spacing.gutter)))
            .with(StyleDeclaration::padding_right(Length::Px(spacing.gutter)))
            .with(StyleDeclaration::padding_bottom(Length::Px(spacing.gutter)))
            .with(StyleDeclaration::padding_left(Length::Px(spacing.gutter)))
            .with(StyleDeclaration::background_color(paper()))
            .with(StyleDeclaration::color(ink()))
            .with(StyleDeclaration::font_family(family(UI_FAMILY))),
    )
}

/// `n` equal-width tracks, i.e. `repeat(n, 1fr)`.
fn equal_tracks(n: u32) -> GridTemplateComponents {
    (0..n)
        .map(|_| GridTemplateComponent::Single(GridTrackSize::Fixed(GridLength::Fr(1.0))))
        .collect()
}

/// One widget's cell, placed explicitly on the grid.
fn cell_node(widget: &Widget, body: &Body, cell_w: f32, cell_h: f32, spacing: Spacing) -> Node {
    let span_w = cell_w * widget.col_span as f32;
    let span_h = cell_h * widget.row_span as f32;
    let label_px = (span_h * 0.13).clamp(11.0, 24.0);

    let mut children = Vec::new();
    if let Some(label) = &widget.label {
        children.push(text_node(
            label,
            one_line(
                text_style(label_px, 700.0, UI_FAMILY)
                    .with(StyleDeclaration::color(muted()))
                    .with(StyleDeclaration::letter_spacing(Length::Px(label_px * 0.06)))
                    .with(StyleDeclaration::text_transform(TextTransform::Uppercase)),
            ),
        ));
    }
    // The body sits in its own growing box so that the label is pinned to the top
    // of every cell while the body is centred in whatever space is left. Laying
    // both out in one column instead would centre them as a group, which makes a
    // label's height depend on how tall its neighbour's content happens to be.
    children.push(
        Node::container(body_nodes(body, span_w, span_h, label_px)).with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(FlexDirection::Column))
                .with(StyleDeclaration::justify_content(JustifyContent::Center))
                .with(StyleDeclaration::align_items(AlignItems::Start))
                .with(StyleDeclaration::flex_grow(Some(FlexGrow(1.0))))
                .with(StyleDeclaration::width(Length::Percentage(100.0)))
                .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                    (span_h * 0.02).clamp(1.0, 6.0),
                )))),
        ),
    );

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::justify_content(JustifyContent::Start))
            .with(StyleDeclaration::align_items(AlignItems::Start))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                (span_h * 0.03).clamp(2.0, 8.0),
            ))))
            .with(StyleDeclaration::padding_top(Length::Px(spacing.padding)))
            .with(StyleDeclaration::padding_right(Length::Px(spacing.padding)))
            .with(StyleDeclaration::padding_bottom(Length::Px(spacing.padding)))
            .with(StyleDeclaration::padding_left(Length::Px(spacing.padding)))
            .with(StyleDeclaration::border_top_width(hairline()))
            .with(StyleDeclaration::border_right_width(hairline()))
            .with(StyleDeclaration::border_bottom_width(hairline()))
            .with(StyleDeclaration::border_left_width(hairline()))
            .with(StyleDeclaration::border_top_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_right_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_left_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_top_color(rule()))
            .with(StyleDeclaration::border_right_color(rule()))
            .with(StyleDeclaration::border_bottom_color(rule()))
            .with(StyleDeclaration::border_left_color(rule()))
            .with(StyleDeclaration::grid_column_start(line(widget.col)))
            .with(StyleDeclaration::grid_column_end(line(
                widget.col + widget.col_span,
            )))
            .with(StyleDeclaration::grid_row_start(line(widget.row)))
            .with(StyleDeclaration::grid_row_end(line(
                widget.row + widget.row_span,
            ))),
    )
}

/// A zero-based grid coordinate as a CSS grid line.
///
/// Grid lines are 1-based, so line `n` is the start edge of cell `n`. Config
/// bounds the grid well inside `i16`, and named placement is deliberately not
/// used: takumi lowers a named placement to `Auto`, so it would be silently
/// ignored.
fn line(coordinate: u32) -> GridPlacement {
    GridPlacement::Line(coordinate as i16 + 1)
}

/// The nodes that make up a cell below its label.
fn body_nodes(body: &Body, span_w: f32, span_h: f32, label_px: f32) -> Vec<Node> {
    let figure_px = (span_h * 0.40).clamp(18.0, 108.0);
    let prose_px = (span_h * 0.11).clamp(12.0, 26.0);

    match body {
        Body::Figure { text, unit } => {
            // The figure is sized down as it gets longer so a five-digit reading
            // still fits the cell it was laid out for.
            let width_limited = span_w * 1.55 / text.chars().count().max(1) as f32;
            let size = figure_px.min(width_limited).max(14.0);
            let mut nodes = vec![text_node(
                text,
                one_line(text_style(size, 700.0, NUMERIC_FAMILY)),
            )];
            if let Some(unit) = unit {
                nodes.push(text_node(
                    unit,
                    one_line(
                        text_style((size * 0.30).max(11.0), 400.0, UI_FAMILY)
                            .with(StyleDeclaration::color(muted())),
                    ),
                ));
            }
            nodes
        }

        Body::Beacon { on } => {
            let dot = (span_h * 0.20).clamp(14.0, 64.0);
            vec![Node::container(vec![
                // Drawn as a shape rather than a glyph so the indicator does not
                // depend on the embedded faces covering any particular symbol.
                Node::container(Vec::new()).with_style(
                    Style::default()
                        .with(StyleDeclaration::width(Length::Px(dot)))
                        .with(StyleDeclaration::height(Length::Px(dot)))
                        .with(StyleDeclaration::border_top_left_radius(radius(dot)))
                        .with(StyleDeclaration::border_top_right_radius(radius(dot)))
                        .with(StyleDeclaration::border_bottom_right_radius(radius(dot)))
                        .with(StyleDeclaration::border_bottom_left_radius(radius(dot)))
                        .with(StyleDeclaration::background_color(if *on {
                            ink()
                        } else {
                            paper()
                        }))
                        .with(StyleDeclaration::border_top_width(LineWidth::Length(
                            Length::Px(2.0),
                        )))
                        .with(StyleDeclaration::border_right_width(LineWidth::Length(
                            Length::Px(2.0),
                        )))
                        .with(StyleDeclaration::border_bottom_width(LineWidth::Length(
                            Length::Px(2.0),
                        )))
                        .with(StyleDeclaration::border_left_width(LineWidth::Length(
                            Length::Px(2.0),
                        )))
                        .with(StyleDeclaration::border_top_style(BorderStyle::Solid))
                        .with(StyleDeclaration::border_right_style(BorderStyle::Solid))
                        .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
                        .with(StyleDeclaration::border_left_style(BorderStyle::Solid))
                        .with(StyleDeclaration::border_top_color(ink()))
                        .with(StyleDeclaration::border_right_color(ink()))
                        .with(StyleDeclaration::border_bottom_color(ink()))
                        .with(StyleDeclaration::border_left_color(ink())),
                ),
                text_node(
                    if *on { "ON" } else { "OFF" },
                    one_line(text_style((dot * 0.78).max(13.0), 700.0, UI_FAMILY)),
                ),
            ])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                    .with(StyleDeclaration::align_items(AlignItems::Center))
                    .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                        dot * 0.5,
                    )))),
            )]
        }

        Body::Prose(text) => {
            let lines = ((span_h - label_px * 2.0) / (prose_px * 1.35)).floor();
            vec![text_node(
                text,
                text_style(prose_px, 400.0, UI_FAMILY)
                    .with(StyleDeclaration::line_height(LineHeight::Unitless(1.3)))
                    // Bounded so a long push is clipped to the cell instead of
                    // pushing the layout around.
                    .with(StyleDeclaration::max_lines(Some(
                        (lines as u32).clamp(1, 12),
                    )))
                    .with(StyleDeclaration::text_overflow(TextOverflow::Ellipsis)),
            )]
        }

        Body::Rows(rows) => {
            let row_px = (span_h / (rows.len().max(1) as f32 + 1.6)).clamp(11.0, 30.0);
            let children = rows
                .iter()
                .map(|row| row_node(row, row_px))
                .collect::<Vec<_>>();
            vec![
                Node::container(children).with_style(
                    Style::default()
                        .with(StyleDeclaration::display(Display::Flex))
                        .with(StyleDeclaration::flex_direction(FlexDirection::Column))
                        .with(StyleDeclaration::width(Length::Percentage(100.0)))
                        .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                            row_px * 0.28,
                        )))),
                ),
            ]
        }

        Body::Stale { since } => vec![
            text_node(
                "last seen",
                one_line(
                    text_style((span_h * 0.11).clamp(11.0, 20.0), 400.0, UI_FAMILY)
                        .with(StyleDeclaration::color(muted())),
                ),
            ),
            text_node(
                since,
                one_line(
                    text_style((span_h * 0.20).clamp(14.0, 40.0), 700.0, UI_FAMILY)
                        .with(StyleDeclaration::color(muted())),
                ),
            ),
        ],

        Body::Absent(reason) => vec![text_node(
            reason,
            text_style((span_h * 0.14).clamp(12.0, 26.0), 400.0, UI_FAMILY)
                .with(StyleDeclaration::color(muted()))
                .with(StyleDeclaration::max_lines(Some(2))),
        )],
    }
}

/// One line of a multi-reading widget: label on the left, value on the right.
fn row_node(row: &Row, size: f32) -> Node {
    let label = row
        .label
        .clone()
        .or_else(|| row.id.clone())
        .unwrap_or_default();
    let mut value = row.value.as_ref().map(value_text).unwrap_or_default();
    if let Some(unit) = &row.unit {
        value.push(' ');
        value.push_str(unit);
    }

    Node::container(vec![
        text_node(
            &label,
            one_line(text_style(size, 400.0, UI_FAMILY).with(StyleDeclaration::color(muted()))),
        ),
        text_node(&value, one_line(text_style(size, 700.0, NUMERIC_FAMILY))),
    ])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::justify_content(
                JustifyContent::SpaceBetween,
            ))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                size * 0.5,
            ))))
            .with(StyleDeclaration::width(Length::Percentage(100.0))),
    )
}

/// A run of text.
///
/// Takes its complete style, because `Node::with_style` *replaces* a node's style
/// rather than merging into it: chaining a second call silently discards the first,
/// which is how a 96px figure ends up rendering at the inherited 16px.
fn text_node(text: &str, style: Style) -> Node {
    Node::text(text.to_owned()).with_style(style)
}

/// The base style for a run of text, to be extended with `.with(..)`.
fn text_style(size: f32, weight: f32, family_name: &str) -> Style {
    Style::default()
        .with(StyleDeclaration::font_size(FontSize::Length(Length::Px(
            size,
        ))))
        .with(StyleDeclaration::font_weight(FontWeight::Absolute(weight)))
        .with(StyleDeclaration::font_family(family(family_name)))
}

/// Text that must stay on one line rather than wrap out of its cell.
fn one_line(style: Style) -> Style {
    style.with(StyleDeclaration::max_lines(Some(1)))
}

fn family(name: &str) -> FontFamily {
    FontFamily::from_names([name.to_owned()])
}

fn hairline() -> LineWidth {
    LineWidth::Length(Length::Px(1.0))
}

fn radius(diameter: f32) -> SpacePair<Length> {
    SpacePair::from_single(Length::Px(diameter / 2.0))
}

fn paper() -> ColorInput {
    ColorInput::Value(Color([255, 255, 255, 255]))
}

fn ink() -> ColorInput {
    ColorInput::Value(Color([0, 0, 0, 255]))
}

/// Secondary text. Mid grey reads as secondary on a 16-level panel and still
/// resolves to something legible when quantised to fewer levels.
fn muted() -> ColorInput {
    ColorInput::Value(Color([102, 102, 102, 255]))
}

/// Cell rules, light enough not to compete with the content.
fn rule() -> ColorInput {
    ColorInput::Value(Color([170, 170, 170, 255]))
}

/// What a cell shows, resolved from configuration plus whatever data exists.
///
/// Extracted from node building so that "what should this cell say" is decided
/// once, in one readable place, rather than tangled through style declarations.
#[derive(Debug, Clone, PartialEq)]
enum Body {
    /// A large figure with an optional unit.
    Figure { text: String, unit: Option<String> },
    /// A two-state indicator.
    Beacon { on: bool },
    /// Free text, wrapped to the cell.
    Prose(String),
    /// A small group of related readings.
    Rows(Vec<Row>),
    /// The publisher has gone quiet: how long ago it was last seen, never the
    /// last value styled as though it were current.
    Stale { since: String },
    /// Nothing has ever been pushed, or the integration could not be reached.
    Absent(&'static str),
}

fn resolve(widget: &Widget, inputs: &RenderInputs<'_>) -> Body {
    if widget.kind == WidgetKind::HaEntity {
        return resolve_ha(widget, inputs);
    }

    let Some(record) = inputs.content.get(&widget.id) else {
        return Body::Absent("no data");
    };

    if let Some(since) = staleness(widget, record, inputs.now) {
        return Body::Stale { since };
    }

    // `rows` is a presentation override available to any kind: when it is present
    // the scalar `value` is ignored, which is what lets one widget show a small
    // group of related readings.
    if let Some(rows) = &record.rows {
        return Body::Rows(rows.clone());
    }

    match widget.kind {
        WidgetKind::Value => Body::Figure {
            text: value_text(&record.value),
            unit: record.unit.clone().or_else(|| widget.unit.clone()),
        },
        WidgetKind::Beacon => Body::Beacon {
            on: beacon_is_on(record, &widget.on_values),
        },
        WidgetKind::Text => Body::Prose(value_text(&record.value)),
        WidgetKind::HaEntity => unreachable!("handled above"),
    }
}

/// An `ha_entity` cell. A fetch failure degrades this cell only: the frame still
/// renders, because one unreachable integration must not blank the dashboard.
fn resolve_ha(widget: &Widget, inputs: &RenderInputs<'_>) -> Body {
    let Some(entity) = &widget.entity else {
        // Config validation rejects this, so it cannot happen from a config file.
        return Body::Absent("no entity");
    };
    match inputs.ha_states.get(entity) {
        Some(Ok(state)) => Body::Figure {
            text: state.clone(),
            unit: widget.unit.clone(),
        },
        Some(Err(_)) | None => Body::Absent("unavailable"),
    }
}

/// How long ago the record was received, if that exceeds the widget's
/// `stale_after`.
///
/// Computed at render time rather than stamped at push time, so raising or
/// lowering `stale_after` takes effect on the next frame.
fn staleness(widget: &Widget, record: &ContentRecord, now: OffsetDateTime) -> Option<String> {
    if widget.stale_after == 0 {
        return None;
    }
    let age = now - record.received_at;
    if age.whole_seconds() < 0 {
        return None;
    }
    let age = age.unsigned_abs().as_secs();
    (age > widget.stale_after).then(|| format!("{} ago", humanise(age)))
}

/// A duration as a short human-readable string.
fn humanise(seconds: u64) -> String {
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3_600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3_600),
        s => format!("{}d", s / 86_400),
    }
}

/// Whether a beacon reads as "on".
///
/// `state` takes precedence: when a publisher sends one it is the authoritative
/// signal, so a non-matching `state` means off rather than falling through to
/// `value`. Falling through would make `{"state":"idle","value":"on"}` read as on,
/// which is not what the publisher said.
fn beacon_is_on(record: &ContentRecord, on_values: &[String]) -> bool {
    let candidate = match &record.state {
        Some(state) => state.clone(),
        None => value_text(&record.value),
    };
    on_values
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(candidate.trim()))
}

/// Renders a pushed JSON value as display text.
fn value_text(value: &serde_json::Value) -> String {
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
fn truncate(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    text.chars().take(chars).collect::<String>() + "\u{2026}"
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Grid, Palette};
    use std::sync::LazyLock;
    use time::Duration;

    /// One font collection for the whole test module: registration is the
    /// expensive part and it is immutable once built.
    static FONTS: LazyLock<Fonts> = LazyLock::new(|| fonts().expect("embedded fonts must load"));

    fn device(widgets: Vec<Widget>) -> Device {
        Device {
            id: "kindle".to_owned(),
            width: 400,
            height: 300,
            palette: Palette::Gray16,
            dither: Dither::Atkinson,
            refresh_rate: 300,
            render_interval: 300,
            grid: Grid { cols: 2, rows: 2 },
            widgets,
        }
    }

    fn widget(id: &str, kind: WidgetKind, col: u32, row: u32) -> Widget {
        Widget {
            id: id.to_owned(),
            kind,
            col,
            row,
            col_span: 1,
            row_span: 1,
            label: Some(id.to_owned()),
            unit: None,
            stale_after: 0,
            entity: None,
            on_values: vec!["on".to_owned(), "true".to_owned(), "alert".to_owned()],
        }
    }

    fn record(value: serde_json::Value, received_at: OffsetDateTime) -> ContentRecord {
        ContentRecord {
            value,
            state: None,
            unit: None,
            rows: None,
            received_at,
        }
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn render(device: &Device, content: &HashMap<String, ContentRecord>) -> Vec<u8> {
        let ha = HashMap::new();
        render_frame(
            &FONTS,
            RenderInputs {
                device,
                content,
                ha_states: &ha,
                now: now(),
            },
        )
        .expect("frame should render")
    }

    fn dimensions(png: &[u8]) -> (u32, u32) {
        let decoder = png::Decoder::new(std::io::Cursor::new(png));
        let reader = decoder.read_info().expect("should be a PNG");
        (reader.info().width, reader.info().height)
    }

    #[test]
    fn embedded_fonts_register() {
        // Guards against a vendored face being replaced with something takumi
        // cannot parse; without fonts every frame would be blank.
        LazyLock::force(&FONTS);
    }

    #[test]
    fn output_is_a_png_at_the_configured_dimensions() {
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let bytes = render(&device, &HashMap::new());
        assert_eq!(dimensions(&bytes), (device.width, device.height));
    }

    #[test]
    fn a_widget_with_no_stored_content_renders_without_error() {
        let device = device(vec![
            widget("a", WidgetKind::Value, 0, 0),
            widget("b", WidgetKind::Beacon, 1, 0),
            widget("c", WidgetKind::Text, 0, 1),
        ]);
        let bytes = render(&device, &HashMap::new());
        assert_eq!(dimensions(&bytes), (400, 300));
    }

    #[test]
    fn rendering_the_same_inputs_twice_is_byte_identical() {
        // The filename-stability behaviour is only correct if this holds.
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let mut content = HashMap::new();
        content.insert("a".to_owned(), record(serde_json::json!(42), now()));
        assert_eq!(render(&device, &content), render(&device, &content));
    }

    #[test]
    fn a_changed_value_changes_the_bytes() {
        let device = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let mut first = HashMap::new();
        first.insert("a".to_owned(), record(serde_json::json!(1), now()));
        let mut second = HashMap::new();
        second.insert("a".to_owned(), record(serde_json::json!(2), now()));
        assert_ne!(render(&device, &first), render(&device, &second));
    }

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
    fn stale_after_zero_disables_the_staleness_timer() {
        let w = widget("a", WidgetKind::Value, 0, 0);
        assert_eq!(w.stale_after, 0);
        let ancient = record(serde_json::json!(1), now() - Duration::days(400));
        assert_eq!(staleness(&w, &ancient, now()), None);
    }

    #[test]
    fn staleness_triggers_strictly_after_the_window() {
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let at_limit = record(serde_json::json!(1), now() - Duration::seconds(60));
        assert_eq!(
            staleness(&w, &at_limit, now()),
            None,
            "exactly at the window is still fresh"
        );
        let past = record(serde_json::json!(1), now() - Duration::seconds(61));
        assert_eq!(staleness(&w, &past, now()), Some("1m ago".to_owned()));
    }

    #[test]
    fn staleness_reports_the_age_at_a_seconds_scale_for_a_short_window() {
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 10;
        let past = record(serde_json::json!(1), now() - Duration::seconds(45));
        assert_eq!(staleness(&w, &past, now()), Some("45s ago".to_owned()));
    }

    #[test]
    fn a_record_from_the_future_is_not_stale() {
        // A publisher's clock is irrelevant here because we stamp receipt
        // ourselves, but a clock step backwards on this host must not read as a
        // negative age.
        let mut w = widget("a", WidgetKind::Value, 0, 0);
        w.stale_after = 60;
        let ahead = record(serde_json::json!(1), now() + Duration::seconds(500));
        assert_eq!(staleness(&w, &ahead, now()), None);
    }

    #[test]
    fn humanises_each_duration_scale() {
        assert_eq!(humanise(0), "0s");
        assert_eq!(humanise(59), "59s");
        assert_eq!(humanise(60), "1m");
        assert_eq!(humanise(3_599), "59m");
        assert_eq!(humanise(3_600), "1h");
        assert_eq!(humanise(86_399), "23h");
        assert_eq!(humanise(86_400), "1d");
        assert_eq!(humanise(86_400 * 9), "9d");
    }

    #[test]
    fn a_beacon_matches_state_before_value() {
        let on_values = vec!["on".to_owned(), "alert".to_owned()];

        let mut alerting = record(serde_json::json!("off"), now());
        alerting.state = Some("alert".to_owned());
        assert!(beacon_is_on(&alerting, &on_values), "state decides when present");

        // A non-matching state means off. Falling through to `value` would report
        // on for a publisher that explicitly said it was idle.
        let mut contradictory = record(serde_json::json!("on"), now());
        contradictory.state = Some("idle".to_owned());
        assert!(!beacon_is_on(&contradictory, &on_values));
    }

    #[test]
    fn a_beacon_falls_back_to_value_when_no_state_is_pushed() {
        let on_values = vec!["on".to_owned(), "true".to_owned()];
        assert!(beacon_is_on(&record(serde_json::json!("on"), now()), &on_values));
        assert!(beacon_is_on(&record(serde_json::json!(true), now()), &on_values));
        assert!(!beacon_is_on(&record(serde_json::json!("off"), now()), &on_values));
        assert!(!beacon_is_on(&record(serde_json::json!(false), now()), &on_values));
    }

    #[test]
    fn beacon_matching_ignores_case_and_surrounding_space() {
        let on_values = vec!["on".to_owned()];
        assert!(beacon_is_on(&record(serde_json::json!(" ON "), now()), &on_values));
    }

    #[test]
    fn beacon_states_render_differently() {
        let device = device(vec![widget("a", WidgetKind::Beacon, 0, 0)]);
        let mut on = HashMap::new();
        on.insert("a".to_owned(), record(serde_json::json!("on"), now()));
        let mut off = HashMap::new();
        off.insert("a".to_owned(), record(serde_json::json!("off"), now()));
        assert_ne!(render(&device, &on), render(&device, &off));
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
    fn a_home_assistant_failure_renders_the_frame_with_that_cell_unavailable() {
        let mut w = widget("temp", WidgetKind::HaEntity, 0, 0);
        w.entity = Some("sensor.office".to_owned());
        let device = device(vec![w]);
        let content = HashMap::new();

        let mut failed = HashMap::new();
        failed.insert(
            "sensor.office".to_owned(),
            Err("connection refused".to_owned()),
        );
        let mut ok = HashMap::new();
        ok.insert("sensor.office".to_owned(), Ok("21.4".to_owned()));

        let broken = render_frame(
            &FONTS,
            RenderInputs {
                device: &device,
                content: &content,
                ha_states: &failed,
                now: now(),
            },
        )
        .expect("a Home Assistant failure must not fail the frame");

        let healthy = render_frame(
            &FONTS,
            RenderInputs {
                device: &device,
                content: &content,
                ha_states: &ok,
                now: now(),
            },
        )
        .unwrap();

        assert_eq!(dimensions(&broken), (400, 300));
        assert_ne!(broken, healthy, "unavailable must look different from a value");
    }

    #[test]
    fn a_home_assistant_entity_that_was_never_fetched_reads_as_unavailable() {
        let mut w = widget("temp", WidgetKind::HaEntity, 0, 0);
        w.entity = Some("sensor.office".to_owned());
        let inputs_content = HashMap::new();
        let empty = HashMap::new();
        assert_eq!(
            resolve(
                &w,
                &RenderInputs {
                    device: &device(vec![]),
                    content: &inputs_content,
                    ha_states: &empty,
                    now: now(),
                }
            ),
            Body::Absent("unavailable")
        );
    }

    #[test]
    fn an_ha_entity_ignores_pushed_content() {
        // The kind reads from Home Assistant, so a push to the same id must not
        // masquerade as the entity's state.
        let mut w = widget("temp", WidgetKind::HaEntity, 0, 0);
        w.entity = Some("sensor.office".to_owned());
        let mut content = HashMap::new();
        content.insert("temp".to_owned(), record(serde_json::json!("99"), now()));
        let mut ha = HashMap::new();
        ha.insert("sensor.office".to_owned(), Ok("21.4".to_owned()));

        assert_eq!(
            resolve(
                &w,
                &RenderInputs {
                    device: &device(vec![]),
                    content: &content,
                    ha_states: &ha,
                    now: now(),
                }
            ),
            Body::Figure {
                text: "21.4".to_owned(),
                unit: None
            }
        );
    }

    #[test]
    fn a_spanning_widget_renders_and_differs_from_a_single_cell() {
        let mut wide = widget("a", WidgetKind::Value, 0, 0);
        wide.col_span = 2;
        let spanning = device(vec![wide]);
        let single = device(vec![widget("a", WidgetKind::Value, 0, 0)]);

        let mut content = HashMap::new();
        content.insert("a".to_owned(), record(serde_json::json!(42), now()));

        assert_ne!(
            render(&spanning, &content),
            render(&single, &content),
            "grid placement must actually affect the raster"
        );
    }

    #[test]
    fn widget_position_affects_the_raster() {
        // Guards the single biggest layout footgun: if grid placement were being
        // ignored, these two would be identical.
        let mut content = HashMap::new();
        content.insert("a".to_owned(), record(serde_json::json!(42), now()));
        let top_left = device(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let bottom_right = device(vec![widget("a", WidgetKind::Value, 1, 1)]);
        assert_ne!(render(&top_left, &content), render(&bottom_right, &content));
    }

    #[test]
    fn a_full_kindle_dashboard_renders_under_the_fetch_ceiling() {
        let mut widgets = Vec::new();
        for row in 0..3 {
            for col in 0..4 {
                let mut w = widget(&format!("w{row}{col}"), WidgetKind::Value, col, row);
                w.unit = Some("C".to_owned());
                widgets.push(w);
            }
        }
        let device = Device {
            width: 1024,
            height: 758,
            grid: Grid { cols: 4, rows: 3 },
            ..device(widgets)
        };
        let mut content = HashMap::new();
        for row in 0..3 {
            for col in 0..4 {
                content.insert(
                    format!("w{row}{col}"),
                    record(serde_json::json!(20 + row * 4 + col), now()),
                );
            }
        }

        let bytes = render(&device, &content);
        assert_eq!(dimensions(&bytes), (1024, 758));
        assert!(
            bytes.len() < MAX_FRAME_BYTES,
            "a full dashboard encoded to {} bytes, over the {MAX_FRAME_BYTES} ceiling",
            bytes.len()
        );
    }

    #[test]
    fn the_placeholder_names_the_requested_and_configured_ids() {
        let configured = vec!["kindle".to_owned(), "kitchen".to_owned()];
        let bytes = render_placeholder(
            &FONTS,
            "kindel",
            &configured,
            1024,
            758,
            Palette::Gray16,
            Dither::Bayer,
        )
        .unwrap();
        assert_eq!(dimensions(&bytes), (1024, 758));

        // Different requested ids must produce different pixels, which is what
        // makes the frame diagnostic rather than decorative.
        let other = render_placeholder(
            &FONTS,
            "kitchn",
            &configured,
            1024,
            758,
            Palette::Gray16,
            Dither::Bayer,
        )
        .unwrap();
        assert_ne!(bytes, other);
    }

    #[test]
    fn the_placeholder_clamps_absurd_dimensions() {
        let bytes = render_placeholder(
            &FONTS,
            "x",
            &["kindle".to_owned()],
            u32::MAX,
            u32::MAX,
            Palette::Mono,
            Dither::Bayer,
        )
        .unwrap();
        let (w, h) = dimensions(&bytes);
        assert_eq!((w, h), (crate::config::MAX_DIMENSION, crate::config::MAX_DIMENSION));
    }

    #[test]
    fn the_placeholder_renders_with_no_configured_devices() {
        let bytes =
            render_placeholder(&FONTS, "x", &[], 800, 480, Palette::Mono, Dither::None).unwrap();
        assert_eq!(dimensions(&bytes), (800, 480));
    }

    #[test]
    fn truncates_an_over_long_device_id() {
        assert_eq!(truncate("short", 64), "short");
        assert_eq!(truncate("abcdef", 3), "abc\u{2026}");
    }
}

