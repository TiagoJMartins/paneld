//! The widget grid: the CSS grid container, one cell per widget, and the header
//! and group scaffolding a cell is built from before `body` draws what is inside
//! it.

use takumi::prelude::*;

use crate::config::{Fit, Widget};
use crate::icon::Icon;

use super::body::{body_nodes, fitted, intrinsic_size};
use super::grid::Layout;
use super::paint::{icon_node, one_line, rule_width, text_node, text_style};
use super::resolve::resolve;
use super::types::{Cell, Greys, Ink, Space, muted, rule};
use super::{RenderInputs, UI_FAMILY, icon, natural};

/// The widget grid: a CSS grid container with one child per widget, sized to the
/// area [`Device::grid_area`] left it rather than to the frame.
///
/// Sized from the area and never from the device, because those differ by exactly
/// the strip a status bar took. Deriving the size here instead would be a second
/// copy of that arithmetic, and the copy that drifted would put every cell
/// somewhere the tap hit test does not look.
pub(super) fn grid_node(fonts: &Fonts, inputs: &RenderInputs<'_>) -> Node {
    let device = inputs.device;
    let grid = device.grid;
    let layout = Layout::for_device(device, inputs.content);
    let gutter = layout.gutter();
    let (_, _, area_w, area_h) = device.grid_area();

    let children = device
        .widgets
        .iter()
        .map(|widget| cell_node(fonts, widget, &resolve(widget, inputs), inputs, &layout))
        .collect::<Vec<_>>();

    Node::container(children).with_style(
        Style::default()
            // `Display`'s default is `Inline`, so a container that should lay out
            // as a grid must say so or its children are laid out inline and every
            // grid placement is silently ignored.
            .with(StyleDeclaration::display(Display::Grid))
            .with(StyleDeclaration::width(Length::Px(area_w as f32)))
            .with(StyleDeclaration::height(Length::Px(area_h as f32)))
            .with(StyleDeclaration::grid_template_columns(Some(equal_tracks(
                grid.cols,
            ))))
            // The row tracks come from the layout rather than being restated here,
            // and that is the whole of `fit = "content"` on the render side: the
            // layout is what the tap hit test resolves against, so a track it did not
            // compute is a cell a finger cannot land on.
            .with(StyleDeclaration::grid_template_rows(Some(pixel_tracks(
                layout.row_tracks(),
            ))))
            // A content-fitted grid whose tracks do not fill the area leaves the
            // remainder at the bottom rather than sharing it out. That is the point of
            // it: a cell is as tall as what is in it, and the slack is margin.
            .with(StyleDeclaration::align_content(JustifyContent::Start))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                gutter,
            ))))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gutter))))
            .with(StyleDeclaration::padding_top(Length::Px(gutter)))
            .with(StyleDeclaration::padding_right(Length::Px(gutter)))
            .with(StyleDeclaration::padding_bottom(Length::Px(gutter)))
            .with(StyleDeclaration::padding_left(Length::Px(gutter)))
            // Never shrunk to make room for the bar beside it. The two are already
            // sized to partition the frame exactly, so any shrinking here would be
            // the flex algorithm moving cells away from where `Layout::rect` — and
            // therefore the tap hit test — says they are.
            .with(StyleDeclaration::flex_shrink(Some(FlexGrow(0.0)))),
    )
}

/// `n` equal-width tracks, i.e. `repeat(n, 1fr)`.
fn equal_tracks(n: u32) -> GridTemplateComponents {
    (0..n)
        .map(|_| GridTemplateComponent::Single(GridTrackSize::Fixed(GridLength::Fr(1.0))))
        .collect()
}

/// Tracks at exactly the pixel heights the layout computed.
///
/// Stated in pixels rather than as fractions even under `fit = "stretch"`, where
/// they are all equal: the layout has already divided the area, and having the grid
/// divide it a second time is how a rounding difference between the two puts a cell
/// half a pixel from where a tap resolves it.
fn pixel_tracks(heights: &[f32]) -> GridTemplateComponents {
    heights
        .iter()
        .map(|&h| {
            GridTemplateComponent::Single(GridTrackSize::Fixed(GridLength::Unit(Length::Px(h))))
        })
        .collect()
}

/// One widget's cell, placed explicitly on the grid.
///
/// Recursive, and terminating without a depth guard of any kind: a group's
/// children are ordinary widgets drawn by this same function, and config
/// validation rejects a group inside a group, so the recursion is exactly one
/// level deep by construction.
fn cell_node(
    fonts: &Fonts,
    widget: &Widget,
    cell: &Cell,
    inputs: &RenderInputs<'_>,
    layout: &Layout,
) -> Node {
    // The rect the grid will actually hand this widget, rather than cells times
    // span: a spanning cell swallows the gutters it spans over, so computing the
    // box here as `cell * span` measured a 2x2 ten pixels short of where
    // `Layout::rect` places it — the exact disagreement `rect` exists to prevent.
    let (_, _, span_w, span_h) = layout.rect(widget);
    let padding = layout.padding();
    let border = layout.border();
    // Padding on both sides, plus a rule on each. Read from the layout rather than
    // added up here, so the box a reading is fitted into is the same box validation
    // held to its floor.
    let chrome = layout.inset();
    // What a cell's contents actually have to fit inside.
    let content_w = (span_w - chrome).max(1.0);
    let style = &widget.style;
    let greys = Greys::of(style);
    // Chrome is sized to the *panel*, bounded by what a cell can hold — never to
    // the span, and never to the cell alone.
    //
    // Two things went wrong when this was a fraction of the cell's height. A label
    // that grew with its widget's span set the same word at two sizes on one
    // dashboard, which is what the cell rather than the span fixed. But a cell is
    // itself unbounded: a two-cell grid on this panel gave cells 1008 pixels tall
    // and set `BRAGA` at 110px, the same size as the reading underneath it. Chrome
    // that competes with content is not chrome. A label's job is to name a cell at
    // a glance and then get out of the way, and the size that does that is a
    // property of how far away the glass is — which is the frame, not the grid.
    //
    // Under `fit = "content"` the cell bound is dropped rather than tightened: a
    // track there is as tall as its content, its content is sized from the chrome,
    // and the chrome cannot be sized from the track without the three of them
    // chasing each other. The panel alone decides it.
    let chrome_px = natural::chrome_type(inputs.device, *style);
    let label_px = match inputs.device.grid.fit {
        Fit::Content => chrome_px,
        Fit::Stretch => chrome_px.min(layout.cell().1 * 0.11).max(style.min_type),
    };
    let gap = (span_h * 0.03).clamp(2.0, 8.0);

    let mut children = Vec::new();
    let mut content_h = span_h - chrome;
    // A group's header is optional where a leaf cell's is not. A leaf always emits
    // one because the not-confirmed mark has to have somewhere to sit; a group reads
    // nothing of its own, so it has no value to mark and no reason to spend any of
    // its box unless it was given a title. That is what lets an untitled group's
    // children fill exactly the content box `Layout::sub_layout` measures them
    // against, and so what keeps a tap landing on the child a finger was over.
    if widget.group.is_none() || widget.label.is_some() || widget.icon.is_some() {
        let header = header_node(fonts, widget, cell, inputs, content_w, label_px);
        // Measured rather than derived from `label_px`: a line box is the layout
        // engine's answer and not the font size, and the body is sized from whatever
        // the header leaves — so an estimate here shows up either as a body that
        // overflows its cell or as a strip of the cell nothing ever uses.
        content_h -= intrinsic_size(fonts, header.clone()).1 + gap;
        children.push(header);
    }
    let content_h = content_h.max(1.0);

    children.push(match &widget.group {
        Some(group) => group_node(fonts, widget, group, inputs, layout),
        // The body sits in its own growing box so that the header is pinned to the
        // top of every cell while the body is centred in whatever space is left.
        // Laying both out in one column instead would centre them as a group, which
        // makes a label's height depend on how tall its neighbour's content happens
        // to be.
        None => Node::container(body_nodes(
            fonts,
            &cell.body,
            cell.ink,
            Space {
                width: content_w,
                height: content_h,
                label_px,
                style: &widget.style,
            },
        ))
        .with_style(
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
    });

    let mut style = Style::default()
        .with(StyleDeclaration::display(Display::Flex))
        .with(StyleDeclaration::flex_direction(FlexDirection::Column))
        .with(StyleDeclaration::justify_content(JustifyContent::Start))
        .with(StyleDeclaration::align_items(AlignItems::Start))
        .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gap))))
        .with(StyleDeclaration::padding_top(Length::Px(padding)))
        .with(StyleDeclaration::padding_right(Length::Px(padding)))
        .with(StyleDeclaration::padding_bottom(Length::Px(padding)))
        .with(StyleDeclaration::padding_left(Length::Px(padding)));
    // A frameless cell is the *absence* of these declarations, not a width of
    // nothing. A zero-width solid edge still costs the layout engine a pass on each
    // of the four sides, and `border = 0` is an author asking for a dashboard of
    // bare readings — which a rule drawn at zero width is only accidentally.
    if border > 0.0 {
        style = style
            .with(StyleDeclaration::border_top_width(rule_width(border)))
            .with(StyleDeclaration::border_right_width(rule_width(border)))
            .with(StyleDeclaration::border_bottom_width(rule_width(border)))
            .with(StyleDeclaration::border_left_width(rule_width(border)))
            .with(StyleDeclaration::border_top_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_right_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_bottom_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_left_style(BorderStyle::Solid))
            .with(StyleDeclaration::border_top_color(rule(greys)))
            .with(StyleDeclaration::border_right_color(rule(greys)))
            .with(StyleDeclaration::border_bottom_color(rule(greys)))
            .with(StyleDeclaration::border_left_color(rule(greys)));
    }

    Node::container(children).with_style(
        style
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

/// A group's body: a grid of its children, filling the group's content box.
///
/// The children are laid out with one gutter between them and no margin of their
/// own, because the group's own padding already *is* that margin. That is the
/// arrangement [`crate::config::sub_cell_size`] computes and the tap hit test
/// resolves against, so the grid is told to take the whole box rather than left to
/// size itself to its content: all three have to agree to the pixel or a finger on
/// one child fires another child's action.
///
/// Each child places itself with the same `grid_column_start` and `grid_row_start`
/// styles a top-level cell writes, because a child's `col` and `row` are already
/// coordinates on this grid rather than on the device's.
fn group_node(
    fonts: &Fonts,
    widget: &Widget,
    group: &crate::config::Group,
    inputs: &RenderInputs<'_>,
    layout: &Layout,
) -> Node {
    let sub = layout
        .sub_layout(widget)
        .expect("a widget carrying a group has a sub-layout");
    let gutter = layout.gutter();
    let children = group
        .widgets
        .iter()
        .map(|child| cell_node(fonts, child, &resolve(child, inputs), inputs, &sub))
        .collect::<Vec<_>>();

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Grid))
            .with(StyleDeclaration::grid_template_columns(Some(equal_tracks(
                group.grid.cols,
            ))))
            .with(StyleDeclaration::grid_template_rows(Some(equal_tracks(
                group.grid.rows,
            ))))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                gutter,
            ))))
            .with(StyleDeclaration::row_gap(Gap::Length(Length::Px(gutter))))
            .with(StyleDeclaration::width(Length::Percentage(100.0)))
            .with(StyleDeclaration::height(Length::Percentage(100.0))),
    )
}

/// The strip across the top of a cell: its icon and label on the left, and on
/// the right the mark that says the value below is not confirmed current.
///
/// Emitted for every leaf cell, even one with no label and no icon, because the
/// mark has to have somewhere to sit. An empty header collapses to nothing taller
/// than its own zero-height children, so it costs an unlabelled cell no space.
///
/// A group is the exception, and [`cell_node`] is where that is decided: a group
/// reads nothing of its own, so it has no unconfirmed value to mark and is given a
/// header only when it was given a title to put in one. The label and icon drawn
/// here are then the group's own — each child resolves and draws its own header
/// inside its own sub-cell.
fn header_node(
    fonts: &Fonts,
    widget: &Widget,
    cell: &Cell,
    inputs: &RenderInputs<'_>,
    content_w: f32,
    label_px: f32,
) -> Node {
    let icon = widget.icon.as_ref().and_then(|spec| inputs.icons.get(spec));
    // Kept inside the label's own line box, so a cell going stale does not grow its
    // header and shove the value down. A dashboard where cells shift as sensors
    // come and go reads as broken even when every number on it is right.
    let mark_w = match cell.ink {
        Ink::Held => (label_px * 1.15).min(content_w * 0.2),
        Ink::Current => 0.0,
    };
    // Sized to the label rather than to the cell, so an icon and its label read as
    // one line of chrome however tall the cell is.
    let icon_w = icon.map_or(0.0, |_| label_px * 1.15 + label_px * 0.35);
    let label_w = (content_w - mark_w - icon_w - label_px * 0.3).max(1.0);

    let mut left = Vec::new();
    if let Some(icon) = icon {
        left.push(icon_node(
            icon,
            label_px * 1.15,
            cell.ink,
            Greys::of(&widget.style),
        ));
    }
    if let Some(label) = &widget.label {
        left.push(fitted(fonts, label_w, label_px, |size| {
            text_node(
                label,
                one_line(
                    text_style(size, 700.0, UI_FAMILY)
                        .with(StyleDeclaration::color(muted(Greys::of(&widget.style))))
                        .with(StyleDeclaration::letter_spacing(Length::Px(size * 0.06)))
                        .with(StyleDeclaration::text_transform(TextTransform::Uppercase)),
                ),
            )
        }));
    }

    let mut children = vec![
        Node::container(left).with_style(
            Style::default()
                .with(StyleDeclaration::display(Display::Flex))
                .with(StyleDeclaration::flex_direction(FlexDirection::Row))
                .with(StyleDeclaration::align_items(AlignItems::Center))
                .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                    label_px * 0.35,
                ))))
                // Shrinks before the mark does: the label has already been sized to
                // fit, so if anything still has to give it should not be the mark,
                // which is the only thing saying the value cannot be trusted.
                .with(StyleDeclaration::flex_shrink(Some(FlexGrow(1.0)))),
        ),
    ];
    if cell.ink == Ink::Held {
        children.push(icon_node(
            &Icon::Svg {
                markup: icon::NOT_CONFIRMED.to_owned(),
                ink: None,
            },
            mark_w,
            Ink::Held,
            Greys::of(&widget.style),
        ));
    }

    Node::container(children).with_style(
        Style::default()
            .with(StyleDeclaration::display(Display::Flex))
            .with(StyleDeclaration::flex_direction(FlexDirection::Row))
            .with(StyleDeclaration::justify_content(
                JustifyContent::SpaceBetween,
            ))
            .with(StyleDeclaration::align_items(AlignItems::Center))
            .with(StyleDeclaration::column_gap(Gap::Length(Length::Px(
                label_px * 0.3,
            ))))
            .with(StyleDeclaration::width(Length::Percentage(100.0))),
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use crate::config::{Chrome, Device, Dither, Grid, Group, WidgetKind};

    use super::super::test_support::*;
    use super::*;

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
    fn a_group_draws_one_cell_per_child_where_sub_layout_puts_it() {
        // Asserted against `sub_layout` because the renderer and the tap hit test read
        // it as one answer: if a child's cell were drawn anywhere other than the rect
        // `sub_layout` reports, a finger over one reading would fire another's action.
        let (device, content) = grouped_panel(None);
        let (width, _, levels) = greys(&render(&device, &content));

        let group = &device.widgets[0];
        let sub = Layout::for_device(&device, &nothing_pushed())
            .sub_layout(group)
            .expect("a group has a sub-layout");
        let children = &group.group.as_ref().expect("the group is set").widgets;

        // Each child's own content box, not merely its rect: a rect holds the child's
        // rule whether or not anything was drawn inside it, so probing the rect would
        // pass on a row of empty boxes.
        let inset = sub.padding() + sub.border();
        for child in children {
            let (x, y, w, h) = sub.rect(child);
            let content = (x + inset, y + inset, w - inset * 2.0, h - inset * 2.0);
            assert!(
                inked(&levels, width, content),
                "child `{}` drew nothing inside {content:?}",
                child.id
            );
        }

        // Two cells side by side rather than one box holding two readings: the gutter
        // between them is untouched, and the row of their top rules crosses ink
        // exactly twice.
        let (x, y, w, h) = sub.rect(&children[0]);
        let gutter = sub.gutter();
        assert!(
            !inked(&levels, width, (x + w + 2.0, y, gutter - 4.0, h)),
            "the gutter between two children must stay paper"
        );
        assert_eq!(
            rule_runs(&levels, width, y, x, x + w + gutter + w),
            2,
            "a 2x1 group must draw two child cells, edge to edge across its content box"
        );
    }

    #[test]
    fn a_titled_group_draws_its_own_header_across_its_children() {
        // A group is a cell, so it can be titled like one. The gutter between its
        // children is what proves the header is the group's own and not a child's: the
        // title spans the whole content box and crosses that gutter, which nothing
        // inside a child can reach.
        let (untitled, content) = grouped_panel(None);
        let (titled, _) = grouped_panel(Some("INDOORS AND OUT"));

        let group = &untitled.widgets[0];
        let sub = Layout::for_device(&untitled, &nothing_pushed())
            .sub_layout(group)
            .expect("a group has a sub-layout");
        let children = &group.group.as_ref().expect("the group is set").widgets;
        let (x, y, w, h) = sub.rect(&children[0]);
        // The gutter between the two children, clear of both their rules. Nothing
        // inside a child can reach it, so ink here is the group's own.
        let between = (x + w + 2.0, y, sub.gutter() - 4.0, h);

        let (width, _, plain) = greys(&render(&untitled, &content));
        assert!(
            !inked(&plain, width, between),
            "an untitled group draws no header, so its gutter is paper"
        );

        let (_, _, headed) = greys(&render(&titled, &content));
        assert!(
            inked(&headed, width, between),
            "a titled group must draw its own header, spanning its children"
        );
    }

    #[test]
    fn a_nested_cell_with_nothing_to_say_still_renders() {
        // The regression this pins is a panic, not a misdraw. A run that inherits its
        // width from its container measures one width and breaks lines at another once
        // that container is a grid item of a grid item, and the text engine asserts on
        // the disagreement — so a group holding a `text` cell, or any cell with no data
        // yet, killed the whole frame. Both bodies are bare runs; every other body
        // wraps its text in a box and was never affected.
        let mut group = widget("utility", WidgetKind::Group, 0, 0);
        group.group = Some(Group {
            grid: Grid {
                cols: 2,
                rows: 2,
                fit: Fit::Stretch,
            },
            widgets: vec![
                widget("prose", WidgetKind::Text, 0, 0),
                widget("absent", WidgetKind::Value, 1, 0),
                widget("figure", WidgetKind::Value, 0, 1),
            ],
        });
        let device = panel(vec![group]);
        let content = HashMap::from([
            (
                "prose".to_owned(),
                record(
                    serde_json::json!("Bin day Tuesday, and the boiler is booked"),
                    now(),
                ),
            ),
            (
                "figure".to_owned(),
                record(serde_json::json!("21.4"), now()),
            ),
        ]);

        let png = render(&device, &content);
        assert_eq!(dimensions(&png), (1448, 1072));

        // And the absence really was drawn, rather than skipped along with the panic.
        let (width, _, levels) = greys(&render(
            &Device {
                dither: Dither::None,
                ..device.clone()
            },
            &content,
        ));
        let sub = Layout::for_device(&device, &nothing_pushed())
            .sub_layout(&device.widgets[0])
            .expect("a group has a sub-layout");
        let children = &device.widgets[0].group.as_ref().expect("set").widgets;
        for child in children {
            assert!(
                inked(&levels, width, sub.rect(child)),
                "nested cell `{}` drew nothing at all",
                child.id
            );
        }
    }

    #[test]
    fn chrome_is_sized_to_the_panel_and_not_to_the_cell() {
        // A two-cell grid on the panel in service gives cells 1008 pixels tall. Sized
        // off the cell, a label came out at 110px — the same size as the reading it
        // was introducing, which is not chrome. Both grids here are the same panel,
        // so both must set their labels the same.
        let dense = panel(vec![widget("a", WidgetKind::Value, 0, 0)]);
        let sparse = Device {
            grid: Grid {
                cols: 2,
                rows: 1,
                fit: Fit::Stretch,
            },
            chrome: Chrome::derived(
                1448,
                1072,
                Grid {
                    cols: 2,
                    rows: 1,
                    fit: Fit::Stretch,
                },
            ),
            ..panel(vec![widget("a", WidgetKind::Value, 0, 0)])
        };

        let label_px = |device: &Device| {
            let layout = Layout::for_device(device, &nothing_pushed());
            (device.width.min(device.height) as f32 * STYLE.chrome_scale * STYLE.type_scale)
                .min(layout.cell().1 * 0.11)
                .max(STYLE.min_type)
        };

        assert_eq!(
            label_px(&dense),
            label_px(&sparse),
            "one panel, one chrome size, whatever the grid"
        );
        assert!(
            label_px(&sparse) < 40.0,
            "chrome must stay chrome: got {}",
            label_px(&sparse)
        );
    }
}
