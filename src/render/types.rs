//! The shared vocabulary the render pipeline draws in: the box a body is laid out
//! in, the greys a frame's ink comes from, and what a cell resolves to before any
//! of that drawing happens.

use takumi::prelude::*;

use crate::content::Row;
use crate::icon::Icon;
use crate::state::Trend;

/// The box a body is drawn in, and the chrome it is drawn against.
///
/// One type because the three always travel together: every size in a body comes
/// out of the box, and the one size that does not — a column of readings, which is
/// capped — is measured against the chrome that names it. Passing them separately
/// was six arguments deep by the time a weather cell had split its box in two.
#[derive(Debug, Clone, Copy)]
pub(super) struct Space<'a> {
    pub(super) width: f32,
    pub(super) height: f32,
    /// The cell's label size: chrome, and the yardstick a reading is held to.
    pub(super) label_px: f32,
    /// Every size and grey this cell is drawn with, already resolved from the
    /// device's style and the widget's own.
    pub(super) style: &'a crate::config::Style,
}

impl<'a> Space<'a> {
    /// The same chrome and style, over a smaller box.
    pub(super) fn sized(&self, width: f32, height: f32) -> Self {
        Self {
            width,
            height,
            label_px: self.label_px,
            style: self.style,
        }
    }

    pub(super) fn greys(&self) -> Greys {
        Greys::of(self.style)
    }
}

/// The three greys a frame is drawn in, lifted out of a [`crate::config::Style`].
///
/// A small copy rather than a borrow of the whole style, because these travel with
/// an [`Ink`] into functions that have no business knowing a cell's type scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Greys {
    pub(super) ink: u8,
    pub(super) muted: u8,
    pub(super) rule: u8,
}

impl Greys {
    pub(super) fn of(style: &crate::config::Style) -> Self {
        Self {
            ink: style.ink,
            muted: style.muted,
            rule: style.rule,
        }
    }
}

/// A grey level as a colour, for an icon that asked for one.
pub(super) fn grey_ink(level: u8) -> ColorInput {
    ColorInput::Value(Color([level, level, level, 255]))
}

pub(super) fn paper() -> ColorInput {
    ColorInput::Value(Color([255, 255, 255, 255]))
}

pub(super) fn ink(greys: Greys) -> ColorInput {
    grey_ink(greys.ink)
}

/// Secondary text. Mid grey reads as secondary on a 16-level panel and still
/// resolves to something legible when quantised to fewer levels.
pub(super) fn muted(greys: Greys) -> ColorInput {
    grey_ink(greys.muted)
}

/// Cell rules, light enough not to compete with the content.
pub(super) fn rule(greys: Greys) -> ColorInput {
    grey_ink(greys.rule)
}

/// What a cell shows, resolved from configuration plus whatever data exists.
///
/// Extracted from node building so that "what should this cell say" is decided
/// once, in one readable place, rather than tangled through style declarations.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Cell {
    pub(super) body: Body,
    pub(super) ink: Ink,
}

/// How much a cell's contents can be trusted.
///
/// A cell renders its last known value either way; this is what stops that being
/// a lie. A held value is drawn in the secondary grey and its cell carries a mark,
/// so "21.4, as of the last time we could ask" is visibly not "21.4, now".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ink {
    /// Confirmed by the request or push that produced this frame.
    Current,
    /// The last value that was confirmed, kept because the newest attempt to
    /// confirm it failed.
    Held,
}

impl Ink {
    pub(super) fn colour(self, greys: Greys) -> ColorInput {
        match self {
            Self::Current => ink(greys),
            Self::Held => muted(greys),
        }
    }
}

/// One line of a multi-reading body, with how far its own value can be trusted.
///
/// The ink is per line rather than per cell, and that is the whole reason the type
/// exists: a `list` is `n` independent sensors sharing a box, so one unreachable
/// sensor mutes its own line and the lines around it stay black. A single cell-wide
/// ink would throw away `n - 1` good readings in order to report one bad one.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Line {
    pub(super) row: Row,
    /// Drawn where the label would go. Resolved here rather than looked up while
    /// building nodes, so that a row's layout takes no map with it.
    pub(super) icon: Option<Icon>,
    /// The arrow drawn after the value, when this reading asked for a trend.
    /// Resolved here for the same reason the icon is.
    pub(super) trend: Option<Trend>,
    pub(super) ink: Ink,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) enum Body {
    /// A large figure with an optional unit.
    Figure {
        text: String,
        unit: Option<String>,
        /// The arrow drawn after the unit, when the widget asked for a trend.
        trend: Option<Trend>,
    },
    /// A weather condition: its glyph, the condition in words when the cell asked
    /// for words, and any readings hung off the same entity.
    Sky {
        svg: &'static str,
        condition: Option<String>,
        rows: Vec<Line>,
    },
    /// A two-state indicator: the icon configured for the state it is in, or a dot
    /// when that state named none, captioned `ON`/`OFF` unless the cell asked for
    /// the indicator alone.
    Beacon {
        on: bool,
        icon: Option<Icon>,
        text: bool,
    },
    /// Free text, wrapped to the cell.
    Prose(String),
    /// A small group of related readings, each trusted on its own.
    Rows(Vec<Line>),
    /// A sub-grid of widgets, and a marker only: a group's children are cells in
    /// their own right, resolved and drawn by [`super::scaffold::cell_node`], so
    /// there is nothing here for [`super::body::body_nodes`] to build.
    Group,
    /// Nothing has ever been pushed, or nothing has ever been read.
    ///
    /// Distinct from a held value, and the distinction is the point: "no data"
    /// says a publisher has never spoken, which is a wiring problem, whereas a
    /// muted value with a mark says the source is known but currently unreachable.
    Absent(&'static str),
}
