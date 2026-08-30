//! A cell's body: the figure, weather, beacon, prose, and row-list drawings that
//! fill the box below a cell's header, and the fitting arithmetic that sizes them
//! to it.

use takumi::prelude::*;

use crate::icon::Icon;

use super::paint::{icon_node, one_line, radius, text_node, text_style};
use super::rows::rows_node;
use super::types::{Body, Greys, Ink, Line, Space, muted, paper};
use super::{NUMERIC_FAMILY, UI_FAMILY};

/// The nodes that make up a cell below its header.
///
/// Every size here comes out of the content box rather than out of a fraction of
/// the cell capped at some pixel count. Those caps were set against a 400x300 test
/// device and silently bound everything on the 1448x1072 panel in service: a label
/// asked for 45px and got 32, a weather caption asked for 82px and got 34.
///
/// The one exception is a column of readings, which is capped — see
/// [`ROW_TYPE_CEILING`] for why a table is not a figure.
pub(super) fn body_nodes(fonts: &Fonts, body: &Body, ink: Ink, space: Space) -> Vec<Node> {
    let Space {
        width: content_w,
        height: content_h,
        label_px,
        style,
    } = space;
    let greys = space.greys();
    match body {
        Body::Figure { text, unit } => {
            // The design size *is* the box's height: a run set at that size overflows
            // it by exactly its line height, so fitting both axes lands the figure on
            // whichever limit actually binds. Fitting width alone, as this did, left
            // the height unused — which on a nearly square cell is most of the cell.
            vec![fitted_box(fonts, content_w, content_h, content_h, |size| {
                figure_node(text, unit.as_deref(), ink, size, style, greys)
            })]
        }

        Body::Sky {
            svg,
            condition,
            rows,
        } => sky_nodes(fonts, svg, condition.as_deref(), rows, ink, space),

        Body::Beacon { on, icon, text } => {
            beacon_nodes(fonts, *on, icon.as_ref(), *text, ink, space)
        }

        Body::Prose(text) => {
            let prose_px = (content_h * 0.14).max(style.min_type);
            let lines = (content_h / (prose_px * 1.35)).floor();
            vec![text_node(
                text,
                text_style(prose_px, 400.0, UI_FAMILY)
                    .with(StyleDeclaration::color(ink.colour(greys)))
                    .with(StyleDeclaration::line_height(LineHeight::Unitless(1.3)))
                    // The width the run wraps at, stated rather than inherited. A
                    // bare text node is block-level, so left alone it takes its
                    // container's width — and inside a group that container is a
                    // grid item of a grid item, where the width resolved during
                    // measurement and the width resolved during line breaking
                    // disagree by more than a pixel and the text engine asserts.
                    // Every other length in this module comes out of the content
                    // box; this one has to as well.
                    .with(StyleDeclaration::width(Length::Px(content_w)))
                    // Bounded so a long push is clipped to the cell instead of
                    // pushing the layout around.
                    .with(StyleDeclaration::max_lines(Some(
                        (lines as u32).clamp(1, 12),
                    )))
                    .with(StyleDeclaration::text_overflow(TextOverflow::Ellipsis)),
            )]
        }

        Body::Rows(rows) => vec![rows_node(fonts, rows, space)],

        // A group's children are cells in their own right, drawn by `cell_node`.
        // There is nothing to add here, and an empty container would still be a box
        // — one taking a share of the very space those children are laid out in.
        Body::Group => Vec::new(),

        // An absence is not a reading, so it is set at chrome size rather than filling
        // the cell a value would have filled. Scaling it to the box put `no data`
        // across a 2x2 in 97px type, which shouts about the one cell with nothing to
        // say, and made the same words two sizes on one dashboard.
        //
        // Wrapped in a box of a definite width, and the wrapper is not cosmetic: the
        // run inside it is block-level, so on its own it takes its container's width
        // — and inside a group that container is a grid item of a grid item, where
        // the width resolved while measuring and the width resolved while breaking
        // lines disagree by more than a pixel and the text engine asserts. The width
        // goes on the wrapper rather than on the run because [`fitted`] measures the
        // run, and a run already told how wide to be measures as exactly that.
        Body::Absent(reason) => vec![
            Node::container(vec![fitted(
                fonts,
                content_w,
                (label_px * 1.1).max(style.min_type),
                |size| {
                    text_node(
                        reason,
                        one_line(
                            text_style(size, 400.0, UI_FAMILY)
                                .with(StyleDeclaration::color(muted(greys))),
                        ),
                    )
                },
            )])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                    .with(StyleDeclaration::width(Length::Px(content_w))),
            ),
        ],
    }
}

/// The weather body: the condition's glyph, its caption when the cell asked for
/// one, and any readings hung beside or beneath it.
///
/// With no readings the glyph block gets the whole content box, which is what a
/// weather cell has always done.
///
/// With readings the box is split along its *long* axis — the glyph beside the rows
/// in a wide cell, above them in a tall or square one. Splitting the short axis
/// instead would hand both halves a letterbox, and neither survives one: a glyph is
/// square, and labelled rows need width for their labels before they need anything
/// else. Splitting the long axis keeps both halves as close to square as the cell
/// allows.
fn sky_nodes(
    fonts: &Fonts,
    svg: &str,
    condition: Option<&str>,
    rows: &[Line],
    ink: Ink,
    space: Space,
) -> Vec<Node> {
    let Space {
        width: content_w,
        height: content_h,
        ..
    } = space;
    if rows.is_empty() {
        // Sized as one block, not as a glyph and a caption that each guessed at
        // the cell: the glyph is the reading and the words are its caption, so
        // their ratio belongs in [`sky_node`] and the pair is fitted together.
        // Sizing them apart is what put a 316px glyph and 34px words in a 699x688
        // cell.
        //
        // The centring wrapper is outside the fit because `width: 100%` inside a
        // measured node measures as the measuring viewport, not as the cell.
        return vec![
            Node::container(vec![fitted_box(
                fonts,
                content_w,
                content_h,
                content_h,
                |size| sky_node(svg, condition, ink, size, space.greys()),
            )])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::justify_content(JustifyContent::Center))
                    .with(StyleDeclaration::width(Length::Percentage(100.0))),
            ),
        ];
    }

    let beside = content_w > content_h;
    let gap = (content_w.min(content_h) * 0.06).clamp(2.0, 12.0);
    // `glyph_share` of a split height, and four fifths of that share when the two sit
    // side by side instead: there the readings carry far more glyphs than the picture
    // does and their labels are what runs out of room first, where stacked the two are
    // equals. One key rather than two, because an author moving the picture's weight
    // means it in both orientations.
    let share = space.style.glyph_share;
    let (glyph_w, glyph_h) = match beside {
        true => ((content_w - gap) * share * 0.8, content_h),
        false => (content_w, (content_h - gap) * share),
    };
    let (rows_w, rows_h) = match beside {
        true => (content_w - gap - glyph_w, content_h),
        false => (content_w, content_h - gap - glyph_h),
    };

    let glyph = Node::container(vec![fitted_box(fonts, glyph_w, glyph_h, glyph_h, |size| {
        sky_node(svg, condition, ink, size, space.greys())
    })])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::justify_content(JustifyContent::Center))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::width(Length::Px(glyph_w)))
            .with(StyleDeclaration::height(Length::Px(glyph_h)))
            // Never gives its half back: the picture is the reading, and a row of
            // long labels would otherwise squeeze it to nothing.
            .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0)))),
    );

    vec![
        Node::container(vec![
            glyph,
            rows_node(fonts, rows, space.sized(rows_w, rows_h)),
        ])
        .with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(match beside {
                    true => FlexDirection::Row,
                    false => FlexDirection::Column,
                }))
                .with(StyleDeclaration::align_items(AlignItems::Center))
                // Only the gap on the main axis applies, and which axis that is has
                // just been decided, so both are set rather than branched over again.
                .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(gap))))
                .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap))))
                .with(StyleDeclaration::width(Length::Percentage(100.0))),
        ),
    ]
}

/// The beacon body: an indicator, and `ON`/`OFF` beside it when the cell asked for
/// words.
///
/// Without the caption the indicator is sized against the whole content box rather
/// than against a word's share of the width. Merely dropping the text node would
/// leave the indicator at the size it had while it was sharing the cell, and a third
/// of the cell blank beside a dot that could have been half again as large.
fn beacon_nodes(
    fonts: &Fonts,
    on: bool,
    icon: Option<&Icon>,
    text: bool,
    ink: Ink,
    space: Space<'_>,
) -> Vec<Node> {
    let (content_w, content_h) = (space.width, space.height);
    let greys = space.greys();
    // The indicator is the reading, so it takes the height it is given, bounded by
    // whatever width the caption leaves it. With no caption it is bounded by the box
    // itself, just short of it so that it never touches the cell's rule.
    let size = match text {
        true => (content_h * 0.42).min(content_w * 0.35),
        false => (content_h * 0.9).min(content_w * 0.9),
    }
    .max(14.0);

    let indicator = match icon {
        // At the dot's size and in the dot's place, so configuring an icon is a
        // change of picture rather than a change of layout.
        Some(icon) => icon_node(icon, size, ink, greys),
        None => dot_node(size, on, ink, greys),
    };
    let mut children = vec![indicator];
    if text {
        children.push(fitted(
            fonts,
            (content_w - size * 1.5).max(1.0),
            (size * 0.78).max(13.0),
            |px| {
                text_node(
                    if on { "ON" } else { "OFF" },
                    one_line(
                        text_style(px, 700.0, UI_FAMILY)
                            .with(StyleDeclaration::color(ink.colour(greys))),
                    ),
                )
            },
        ));
    }

    vec![
        Node::container(children).with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                .with(StyleDeclaration::align_items(AlignItems::Center))
                .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                    size * 0.5,
                )))),
        ),
    ]
}

/// A beacon's indicator as a filled or hollow circle.
///
/// Drawn as a shape rather than a glyph so the indicator does not depend on the
/// embedded faces covering any particular symbol.
fn dot_node(size: f32, on: bool, ink: Ink, greys: Greys) -> Node {
    Node::container(Vec::new()).with_style(
        Style::default()
            .with(StyleDeclaration::width(Length::Px(size)))
            .with(StyleDeclaration::height(Length::Px(size)))
            .with(StyleDeclaration::border_top_left_radius(radius(size)))
            .with(StyleDeclaration::border_top_right_radius(radius(size)))
            .with(StyleDeclaration::border_bottom_right_radius(radius(size)))
            .with(StyleDeclaration::border_bottom_left_radius(radius(size)))
            .with(StyleDeclaration::background_color(match on {
                true => ink.colour(greys),
                false => paper(),
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
            .with(StyleDeclaration::border_top_color(ink.colour(greys)))
            .with(StyleDeclaration::border_right_color(ink.colour(greys)))
            .with(StyleDeclaration::border_bottom_color(ink.colour(greys)))
            .with(StyleDeclaration::border_left_color(ink.colour(greys))),
    )
}

/// A large figure with its unit set beside it.
///
/// The unit shares the figure's line and sits on its baseline, at a little over
/// half its size. Stacking it underneath, as this once did, reads as a caption
/// about the number rather than as part of it — `23.4` and `°C` are one reading,
/// and a panel should say so the way a thermometer does.
///
/// Takes its size rather than choosing one: [`fitted`] decides that, so the unit
/// is measured into the fit rather than estimated around it.
pub(super) fn figure_node(
    text: &str,
    unit: Option<&str>,
    ink: Ink,
    size: f32,
    style: &crate::config::Style,
    greys: Greys,
) -> Node {
    let mut children = vec![text_node(
        text,
        one_line(
            text_style(size, 700.0, NUMERIC_FAMILY)
                .with(StyleDeclaration::color(ink.colour(greys))),
        ),
    )];
    if let Some(unit) = unit {
        children.push(
            // Nudged up by the difference in descender depth. `align_items: End`
            // aligns the two runs' *boxes*, and each run's baseline sits its own
            // descender above its box bottom — so the smaller run lands low by
            // exactly the difference. `AlignItems::Baseline` is the property that
            // should do this, but it leaves the unit sitting visibly below the
            // figure, so the correction is applied here where it can be seen and
            // checked.
            Node::container(vec![text_node(
                unit,
                one_line(
                    text_style(size * style.unit_scale, 400.0, UI_FAMILY)
                        .with(StyleDeclaration::color(ink.colour(greys))),
                ),
            )])
            .with_style(
                Style::default()
                    .with(StyleDeclaration::display(Display::Flex))
                    .with(StyleDeclaration::padding_bottom(Length::Px(
                        size * (1.0 - style.unit_scale) * UNIT_BASELINE_LIFT,
                    ))),
            ),
        );
    }

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::align_items(AlignItems::End))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                size * 0.06,
            )))),
    )
}

/// The sky block: the condition as a glyph, with the condition in words beneath it.
///
/// `size` is the glyph's side and the caption is a fixed fraction of it, so one
/// number sizes the pair and [`fitted_box`] can fit them to a cell together. The
/// two are centred as one block: a big glyph with a short caption under one corner
/// of it reads as two unrelated things sharing a box.
///
/// `condition` is `None` when the cell asked for the glyph alone, and then the glyph
/// *is* the block: it is returned bare rather than wrapped in a one-child column, so
/// [`fitted_box`] fits the picture to the whole box instead of to a box with a
/// caption's worth of height and a row gap still reserved inside it.
fn sky_node(svg: &str, condition: Option<&str>, ink: Ink, size: f32, greys: Greys) -> Node {
    let glyph = icon_node(
        &Icon::Svg {
            markup: svg.to_owned(),
            ink: None,
        },
        size,
        ink,
        greys,
    );
    let Some(condition) = condition else {
        return glyph;
    };
    let caption_px = size * SKY_CAPTION_SCALE;
    Node::container(vec![
        glyph,
        text_node(
            condition,
            one_line(
                text_style(caption_px, 400.0, UI_FAMILY)
                    .with(StyleDeclaration::color(ink.colour(greys))),
            ),
        ),
    ])
    .with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Column))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::justify_content(JustifyContent::Center))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(
                caption_px * 0.5,
            )))),
    )
}

/// The sky caption's size, as a fraction of the glyph's side.
///
/// The glyph is the reading and the words underneath confirm it, so the caption is
/// deliberately a sixth of the glyph: large enough to read across a room, small
/// enough that it never competes with the picture for the eye.
///
/// Applies only where there is a caption. A cell with `state_text = false` has no
/// words to size, and its glyph takes the box whole.
const SKY_CAPTION_SCALE: f32 = 0.16;

/// How far the unit must be lifted to share the figure's baseline, as a fraction
/// of the size difference between the two runs.
///
/// Two runs in a row are laid out with their boxes bottom-aligned, and each run's
/// baseline sits a fixed fraction of its own font size above its box bottom — so
/// the smaller run lands low in proportion to how much smaller it is. That makes
/// the correction exactly linear in `size * (1 - UNIT_SCALE)`, which is why one
/// coefficient covers every size.
///
/// Measured rather than derived from the faces' metrics: the value that matters is
/// the layout engine's line box, not the font's declared descender, and the two
/// are not the same number. `a_unit_sits_on_the_figures_baseline` is what pins it.
const UNIT_BASELINE_LIFT: f32 = 0.279;

/// Smallest type this will shrink to, in pixels.
///
/// A floor rather than an assertion that everything fits: past this the glyphs
/// stop being readable on the glass, and a reading nobody can read is no better
/// than one that overflows. A value long enough to hit this is a config or
/// publisher problem, and it should look like one.
pub(super) const MIN_TYPE_PX: f32 = 12.0;

/// Builds a run at the largest size, up to `design`, at which it fits `available`
/// pixels wide.
///
/// Measured, not estimated. The estimate this replaces assumed a fixed advance per
/// character, which in a proportional face is wrong by more than a factor of two
/// between `1` and `W`: it shrank readings that would have fitted and let wide ones
/// overflow, where the one-line bound then cut them off mid-glyph. A clipped
/// reading is the worst failure a panel has, because it looks like a value rather
/// than like an error.
pub(super) fn fitted(
    fonts: &Fonts,
    available: f32,
    design: f32,
    build: impl Fn(f32) -> Node,
) -> Node {
    let intrinsic = intrinsic_width(fonts, build(design));
    let size = fit_size(intrinsic, available, design);
    if size == design {
        return build(design);
    }
    build(size)
}

/// Builds a node at the largest size, up to `design`, at which it fits inside
/// `available_w` by `available_h`.
///
/// Two axes because a cell is a box and not a line. Fitting width alone sized a
/// figure by how many glyphs it happened to have and left the rest of the cell
/// empty; the height is a real constraint too, and on the panel in service it is
/// often the looser of the two, which is where the space went. Whichever axis
/// overflows by more decides, and one corrective step is exact for both because
/// every length in these nodes is proportional to `size`.
fn fitted_box(
    fonts: &Fonts,
    available_w: f32,
    available_h: f32,
    design: f32,
    build: impl Fn(f32) -> Node,
) -> Node {
    let (width, height) = intrinsic_size(fonts, build(design));
    let size = fit_size(width, available_w, design).min(fit_size(height, available_h, design));
    if size == design {
        return build(design);
    }
    build(size)
}

/// The size a run measuring `intrinsic` pixels wide at `design` should be set at
/// to fit `available`.
///
/// One corrective step is exact rather than iterative, because a text run's
/// advance is proportional to its font size: the ratio of overflow *is* the ratio
/// to shrink by. The result is shaved slightly, because a run set to exactly the
/// space it has is a rounding error away from wrapping.
///
/// The readable floor is itself bounded by `design`. A grid dense enough to put a
/// cell's design size under [`MIN_TYPE_PX`] is legal configuration, and a floor
/// above the ceiling is a panic rather than a small glyph.
pub(super) fn fit_size(intrinsic: f32, available: f32, design: f32) -> f32 {
    if intrinsic <= available || intrinsic <= 0.0 {
        return design;
    }
    (design * available / intrinsic * 0.99).clamp(MIN_TYPE_PX.min(design), design)
}

/// How wide a node wants to be, with nothing constraining it.
pub(super) fn intrinsic_width(fonts: &Fonts, node: Node) -> f32 {
    intrinsic_size(fonts, node).0
}

/// How large a node wants to be, with nothing constraining it, as `(width,
/// height)`.
///
/// Measured in a viewport far larger than any panel so that no wrapping or
/// shrink-to-fit has kicked in: the answer wanted here is the node's natural size,
/// not what it would do in a box. Returns `(0.0, 0.0)` if measurement fails, which
/// leaves the design size in place — better than refusing to draw the cell.
///
/// **The node is wrapped in a flex row before measuring, and that wrapper is not
/// cosmetic.** A bare text node is block-level, so it reports its *container's*
/// width — against this viewport, 32768px — and every run measured that way was
/// shrunk to [`MIN_TYPE_PX`] regardless of how short it was. That is why a label
/// asking for 45px was set at 12. A flex container is sized to its content, so
/// wrapping asks the question that was meant: how wide is this run?
///
/// Nothing measured here may size itself as a percentage of its container, because
/// against this viewport that resolves to a number with no relation to the cell.
pub(super) fn intrinsic_size(fonts: &Fonts, node: Node) -> (f32, f32) {
    let probe = Node::container(vec![node]).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::align_items(AlignItems::Start)),
    );
    let options = RenderOptions::builder()
        .viewport(Viewport::new((
            crate::config::MAX_DIMENSION * 8,
            crate::config::MAX_DIMENSION * 8,
        )))
        .node(probe)
        .fonts(fonts)
        .build();
    takumi::measure(options)
        .map(|measured| (measured.width, measured.height))
        .unwrap_or((0.0, 0.0))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::{Chrome, Device, Fit, Grid, Widget, WidgetKind};
    use crate::ha::{Reading, Reported};

    use super::super::grid::Layout;

    use super::super::test_support::*;
    use super::*;

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
    fn every_weather_condition_renders_a_frame() {
        // The rasteriser silently draws nothing for markup usvg rejects, so the
        // only assertion worth making is that a real frame comes out with ink in it.
        let mut w = ha_widget("sky", "weather.braga", WidgetKind::Weather);
        w.col_span = 2;
        w.row_span = 2;
        let device = device(vec![w]);
        let reading = Reading::state("weather.braga");

        let mut frames = std::collections::HashSet::new();
        for slug in [
            "clear-night",
            "cloudy",
            "exceptional",
            "fog",
            "hail",
            "lightning",
            "lightning-rainy",
            "partlycloudy",
            "pouring",
            "rainy",
            "snowy",
            "snowy-rainy",
            "sunny",
            "windy",
            "windy-variant",
        ] {
            let ha = HashMap::from([(reading.clone(), Reported::Fresh(slug.to_owned()))]);
            let frame = render_with(&device, &HashMap::new(), &ha, &HashMap::new());
            assert_eq!(dimensions(&frame), (400, 300), "rendering {slug}");
            frames.insert(frame);
        }
        assert_eq!(
            frames.len(),
            15,
            "every condition must be visually distinguishable, so no two frames may match"
        );
    }

    #[test]
    fn a_unit_sits_on_the_figures_baseline() {
        // Measured in pixels because this is the kind of thing that looks right in
        // the style declarations and wrong on the glass: `align_items: Baseline`
        // does not give two runs at different sizes a shared baseline here, and the
        // unit ended up sitting a quarter of its height below the number it belongs
        // to.
        //
        // Both runs are the same digits on purpose. The question is where the layout
        // puts a smaller run, and a real unit string would confound the measurement:
        // the lowest inked row under `°` is the degree sign's bottom, nowhere near
        // the baseline, and the `p` of `hPa` has a descender below it.
        for size in [40.0, 60.0, 100.0, 143.0, 168.0] {
            let node = figure_node("88", Some("88"), Ink::Current, size, &STYLE, GREYS);
            let (value, unit) = baselines(node, 1_400, 500);
            let delta = unit as i64 - value as i64;
            assert!(
                delta.abs() <= 2,
                "at {size}px the unit's baseline is {delta:+}px off the figure's \
                 ({value} vs {unit})"
            );
        }
    }

    #[test]
    fn a_long_figure_scales_down_rather_than_being_clipped() {
        // Measured fitting, not a character-count estimate: the point is that the
        // run's real advance decides the size, so nothing is ever cut mid-glyph.
        // A figure that already fits keeps the design size.
        let short = figure_px_for("7", None);
        assert_eq!(short, 96.0);
        let long = figure_px_for("123456789", None);
        assert!(
            long < short,
            "a wide reading must shrink: {long} should be under {short}"
        );

        // And the unit is counted in, so adding one shrinks the figure further.
        let bare = figure_px_for("1234", None);
        let with_unit = figure_px_for("1234", Some("kWh"));
        assert!(
            with_unit <= bare,
            "a unit competes for the same width: {with_unit} should not exceed {bare}"
        );
    }

    #[test]
    fn fitting_never_clamps_to_a_floor_above_its_ceiling() {
        // A 40x40 cell is legal configuration and puts the design size under the
        // readable floor. Clamping to a floor above the ceiling panics, and a panic
        // in the render task is the one failure mode that leaves a stale frame on
        // the glass with nothing to say why.
        //
        // Below the floor the answer is the design size: shrinking an 11px reading
        // to fit would take it to a fraction of a pixel, so it is better to set it
        // as designed and let the one-line bound clip what will not fit.
        assert_eq!(fit_size(500.0, 10.0, 11.0), 11.0);

        // Above the floor, shrinking happens and stops at the floor.
        assert_eq!(fit_size(1_000.0, 10.0, 96.0), MIN_TYPE_PX);
        assert!(fit_size(200.0, 100.0, 96.0) < 96.0);
    }

    #[test]
    fn a_beacon_without_state_text_keeps_the_indicator_and_drops_the_word() {
        let captioned = widget("a", WidgetKind::Beacon, 0, 0);
        let bare = Widget {
            state_text: false,
            ..captioned.clone()
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("on"), now()))]);

        assert_eq!(
            resolved_push(&captioned, &content, &HashMap::new()).body,
            Body::Beacon {
                on: true,
                icon: None,
                text: true,
            }
        );
        assert_eq!(
            resolved_push(&bare, &content, &HashMap::new()).body,
            Body::Beacon {
                on: true,
                icon: None,
                text: false,
            }
        );
    }

    #[test]
    fn an_uncaptioned_beacon_takes_the_cell_the_caption_would_have_had() {
        // Measured in pixels because the resolved body cannot say it: dropping the
        // word alone would leave the indicator at the size it had while it was
        // sharing the width, and a third of the cell blank beside it.
        let captioned = widget("a", WidgetKind::Beacon, 0, 0);
        let bare = Widget {
            state_text: false,
            ..captioned.clone()
        };
        let content = HashMap::from([("a".to_owned(), record(serde_json::json!("on"), now()))]);
        let with_word = panel(vec![captioned]);
        let without = panel(vec![bare]);

        let (_, shorter) = cell_fill(&with_word, "a", &content, &HashMap::new());
        let (_, taller) = cell_fill(&without, "a", &content, &HashMap::new());
        assert!(
            taller > shorter * 1.1,
            "the uncaptioned indicator must grow into the space the word had: \
             {taller:.2} of the height against {shorter:.2}"
        );
    }

    #[test]
    fn a_wide_weather_cell_sets_its_readings_beside_the_glyph_not_below() {
        // `sky_nodes`' side-by-side layout, only reachable from a cell wider than
        // it is tall — every other weather fixture used a taller cell, leaving
        // this whole branch unrasterised. The regression this pins is a panic,
        // not a misdraw.
        let w = Widget {
            readings: vec![reading("Temp", "weather.braga", Some("temperature"))],
            ..ha_widget("sky", "weather.braga", WidgetKind::Weather)
        };
        const GRID: Grid = Grid {
            cols: 1,
            rows: 1,
            fit: Fit::Stretch,
        };
        let device = Device {
            width: 800,
            height: 200,
            grid: GRID,
            chrome: Chrome::derived(800, 200, GRID),
            ..device(vec![w])
        };
        let ha = HashMap::from([
            (
                Reading::state("weather.braga"),
                Reported::Fresh("sunny".to_owned()),
            ),
            (
                Reading::attribute("weather.braga", "temperature"),
                Reported::Fresh("23.4".to_owned()),
            ),
        ]);
        let png = render_with(&device, &HashMap::new(), &ha, &HashMap::new());
        assert_eq!(dimensions(&png), (800, 200));
    }

    #[test]
    fn a_bare_weather_glyph_with_no_words_still_draws() {
        // `sky_node`'s empty-condition path: a cell that asked for the icon alone
        // must still put ink on the glass, not just skip the caption.
        let w = Widget {
            state_text: false,
            ..ha_widget("sky", "weather.braga", WidgetKind::Weather)
        };
        let device = device(vec![w]);
        let ha = HashMap::from([(
            Reading::state("weather.braga"),
            Reported::Fresh("sunny".to_owned()),
        )]);
        let png = render_with(&device, &HashMap::new(), &ha, &HashMap::new());
        let (width, _, levels) = greys(&png);
        let layout = Layout::for_device(&device, &nothing_pushed());
        let rect = layout.rect(&device.widgets[0]);
        assert!(
            inked(&levels, width, rect),
            "a bare weather glyph must still draw its icon"
        );
    }
}
