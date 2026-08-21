//! Battery percentage history, and the charge rate and ETA read off it.
//!
//! A poll reports a level, never a rate: the panel says `72%` and nothing about
//! where it is heading. The rate has to be inferred from how the level moved
//! over time, which means keeping samples — and keeping them across restarts.
//! This is the one thing a panel reports that a deploy must not forget: at 1%
//! resolution on a device that discharges over days, an emptied history means no
//! rate for hours, and those are exactly the hours after a deploy, when someone
//! is looking. So [`BatteryStore`] is persisted, unlike [`crate::status`], whose
//! every field describes this process's own lifetime.
//!
//! What the panel actually sends is an **integer** percentage (see
//! [`crate::telemetry`]), at the device's own `refresh_rate`. Two consequences
//! shape everything here.
//!
//! **Samples are run-length encoded.** Consecutive polls are usually identical,
//! so storing every one would spend the whole retention window on repeats. One
//! [`Reading`] per distinct level holds when that level was first and last seen,
//! plus the power state it was seen under. Nothing is lost: a level that did not
//! move carries no information beyond how long it held.
//!
//! **The rate is measured crossing to crossing.** `since` on a reading is when
//! the level was first observed at that value — an estimate of the moment the
//! true level crossed it, good to one poll interval. A rate is therefore the
//! percentage between two crossings over the time between them. Two details
//! follow from that and are the whole subtlety of this module:
//!
//! - A rate cannot be read from two adjacent samples. At 1% resolution the last
//!   step is either `0%` or a whole point of quantisation error, which on a panel
//!   that discharges over days is an error of hundreds of percent. Measuring
//!   across every crossing in the current trend amortises it.
//! - The **oldest** reading of a trend is not a crossing of that trend. It is
//!   either where the process started watching, or the level a charge topped out
//!   at, and in both cases its `since` predates the trend by however long the
//!   level sat there. Anchoring on it is what makes a panel look like it is
//!   discharging at a third of its real rate for the first hours after being
//!   unplugged. So it is excluded from the measurement, and a trend needs two
//!   crossings before it has a rate at all.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use time::{Duration, OffsetDateTime};

use crate::jsonfile;

/// Distinct readings retained per device.
///
/// Bounded because this grows once per level change, forever. Sixty-four is well
/// over a full discharge at 1% resolution, so the cap is reached only by a client
/// whose reading oscillates — and for that client the oldest readings are the
/// ones worth dropping.
const MAX_READINGS: usize = 64;

/// How many expected steps the newest level may hold for before the trend is
/// called stale.
///
/// A plateau of roughly one step is normal: that is what quantisation looks like
/// between two crossings. Three steps' worth of silence means the process that
/// produced the earlier steps has stopped — the charger came out, or the panel
/// stopped drawing. Reporting "full in twenty minutes" for a device unplugged an
/// hour ago is worse than reporting nothing.
const STALE_STEP_MULTIPLE: f64 = 3.0;

/// Seconds per hour, as a float, because every rate here is per hour.
const SECONDS_PER_HOUR: f64 = 3_600.0;

/// What the device said about its power source.
///
/// Both fields are three-state on purpose: no KOReader client reports either
/// today, and the TRMNL firmware omits each header rather than sending a false
/// one when its board cannot read the charger. "Not reported" and "not charging"
/// have to stay distinguishable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Power {
    /// The charger's own view. `Some(false)` covers charge-complete and charging
    /// disabled as well as unplugged, so it is not the opposite of `Some(true)`.
    pub charging: Option<bool>,
    /// Whether USB power is present. This is the one that separates
    /// plugged-and-full from running on the cell.
    pub usb_connected: Option<bool>,
}

/// One distinct reading, and the window over which it was reported.
///
/// A new reading starts when the level changes *or* the power state does: a
/// charger going in is a new regime even at an unchanged percentage, and the
/// trend must not be measured across it.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    /// The level as the device reported it.
    pub percent: f64,
    pub power: Power,
    /// The poll that first reported this level.
    #[serde(with = "time::serde::rfc3339")]
    pub since: OffsetDateTime,
    /// The most recent poll that still reported it.
    #[serde(with = "time::serde::rfc3339")]
    pub until: OffsetDateTime,
    /// How many polls reported it, `since` and `until` included.
    pub polls: u32,
}

/// Which way the level is moving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// The level is rising, or the device says it is charging.
    Charging,
    /// The level is falling.
    Discharging,
    /// It moved once, but not lately: the trend is stale.
    Steady,
    /// Nothing has moved yet and the device did not say.
    Unknown,
}

/// What the retained history implies about where the level is heading.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Trend {
    /// A reported `charging: true` wins here, because it is an observation where
    /// everything else in this struct is an inference. It can therefore read
    /// `charging` alongside a negative rate, which is a real state: a panel
    /// drawing more than its charger supplies.
    pub direction: Direction,
    /// Signed: negative while discharging. `None` until two crossings are known.
    pub percent_per_hour: Option<f64>,
    /// The crossing-to-crossing window the rate was measured over, in hours.
    pub observed_hours: Option<f64>,
    /// Level changes observed in this trend. The rate needs two of them, and its
    /// error falls off with each further one.
    pub steps: usize,
    /// The newest level has held far longer than the trend predicts, so the rate
    /// describes the past rather than the present.
    pub stale: bool,
    /// When the level is projected to reach 100% (rising) or 0% (falling).
    /// `None` when there is no rate, the trend is stale, the level is already at
    /// the bound, or the projection runs off the end of the calendar.
    #[serde(with = "time::serde::rfc3339::option")]
    pub eta_at: Option<OffsetDateTime>,
    /// The same projection as a remaining duration, in seconds.
    pub eta_seconds: Option<i64>,
}

impl Trend {
    /// A trend with no measurement in it, carrying only what the device said.
    fn unmeasured(direction: Direction) -> Self {
        Self {
            direction,
            percent_per_hour: None,
            observed_hours: None,
            steps: 0,
            stale: false,
            eta_at: None,
            eta_seconds: None,
        }
    }
}

/// One device's percentage history, plus what it implies. The debug endpoint's
/// per-device body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// The level as of the most recent poll that reported one.
    pub percent: Option<f64>,
    /// When that level was last confirmed.
    #[serde(with = "time::serde::rfc3339::option")]
    pub reported_at: Option<OffsetDateTime>,
    /// The power state that came with the newest reading.
    pub power: Power,
    pub trend: Trend,
    /// Every retained reading, oldest first.
    pub readings: Vec<Reading>,
}

/// One device's retained readings, oldest first, run-length encoded.
///
/// Serialises as the bare array of readings: the file is the history, and a
/// wrapper object around it would be a field name to keep honest for nothing.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(transparent)]
pub struct History {
    readings: VecDeque<Reading>,
}

impl History {
    /// Folds one poll's reported level and power state in.
    ///
    /// A poll matching the newest reading in both extends it rather than adding
    /// another, which is what keeps the retention window measured in days of
    /// history instead of hours of repeats.
    pub fn record(&mut self, percent: f64, power: Power, at: OffsetDateTime) {
        if let Some(newest) = self.readings.back_mut()
            && newest.percent == percent
            && newest.power == power
        {
            newest.until = at;
            newest.polls += 1;
            return;
        }
        if self.readings.len() == MAX_READINGS {
            self.readings.pop_front();
        }
        self.readings.push_back(Reading {
            percent,
            power,
            since: at,
            until: at,
            polls: 1,
        });
    }

    /// Drops all but the newest [`MAX_READINGS`].
    ///
    /// Applied to whatever a load found on disk, because that file may have been
    /// hand-edited or written by a build with a larger cap, and the bound exists
    /// to be a bound. Readings that break this module's other invariants — a
    /// backwards clock, two adjacent readings at one level — cost precision in
    /// the estimate and nothing more, so they are left alone.
    fn truncate(&mut self) {
        while self.readings.len() > MAX_READINGS {
            self.readings.pop_front();
        }
    }

    /// The history and the estimate drawn from it.
    pub fn report(&self) -> Report {
        let newest = self.readings.back();
        Report {
            percent: newest.map(|reading| reading.percent),
            reported_at: newest.map(|reading| reading.until),
            power: newest.map(|reading| reading.power).unwrap_or_default(),
            trend: self.trend(),
            readings: self.readings.iter().cloned().collect(),
        }
    }

    /// The rate across the current trend, and where it lands.
    fn trend(&self) -> Trend {
        let Some(newest) = self.readings.back() else {
            return Trend::unmeasured(Direction::Unknown);
        };
        let reported = Direction::reported(newest.power);

        let Some(start) = self.trend_start() else {
            return Trend::unmeasured(reported);
        };
        let last = self.readings.len() - 1;
        let steps = last - start;

        // The last step, which is zero only where the power state changed at an
        // unchanged level. Nothing has been observed to move across that poll, so
        // it falls back to whatever the device claimed rather than reading a
        // charger coming out as a level going down. Compared against zero rather
        // than through `signum`, which calls +0.0 positive.
        let observed = match self.step(last - 1) {
            delta if delta > 0.0 => Direction::Charging,
            delta if delta < 0.0 => Direction::Discharging,
            _ => reported,
        };

        // The trend's first reading is excluded: its `since` is not a crossing of
        // this trend. See the module note.
        let measured = (steps >= 2).then(|| self.rate(start + 1, last)).flatten();

        // Compared against the mean step of *this* trend rather than against the
        // device's `refresh_rate`: a panel discharging 1% a day and one charging
        // 1% a minute are both normal, and only their own history says which.
        let stale = measured.is_some_and(|(_, observed_hours)| {
            let held = hours(newest.since, newest.until).unwrap_or(0.0);
            held > STALE_STEP_MULTIPLE * (observed_hours / (steps - 1) as f64)
        });

        let projection = measured
            .filter(|_| !stale)
            .and_then(|(rate, _)| eta(newest, rate));

        Trend {
            direction: match (reported, stale) {
                (Direction::Charging, _) => Direction::Charging,
                (_, true) => Direction::Steady,
                (_, false) => observed,
            },
            percent_per_hour: measured.map(|(rate, _)| round(rate)),
            observed_hours: measured.map(|(_, hours)| round(hours)),
            steps,
            stale,
            eta_at: projection.map(|(at, _)| at),
            eta_seconds: projection.map(|(_, seconds)| seconds),
        }
    }

    /// Percent per hour between the crossings into readings `from` and `to`, with
    /// the window it was measured over.
    fn rate(&self, from: usize, to: usize) -> Option<(f64, f64)> {
        let hours = hours(self.readings[from].since, self.readings[to].since)?;
        let delta = self.readings[to].percent - self.readings[from].percent;
        Some((delta / hours, hours))
    }

    /// Index of the oldest reading belonging to the current trend, or `None` when
    /// no level change has been seen at all.
    ///
    /// A trend ends where the level turned over, or where the power state
    /// changed. The second is the sharper signal and the reason [`Power`] is part
    /// of a reading: a charger going in is a new regime from that poll, not from
    /// whenever the level next happens to move.
    fn trend_start(&self) -> Option<usize> {
        let power = self.readings.back()?.power;
        let last = self.readings.len().checked_sub(2)?;
        if self.readings[last].power != power {
            return Some(last + 1);
        }
        let rising = self.step(last) > 0.0;

        let mut start = last;
        while start > 0
            && self.readings[start - 1].power == power
            && (self.step(start - 1) > 0.0) == rising
        {
            start -= 1;
        }
        Some(start)
    }

    /// The change in level from reading `index` to its successor.
    ///
    /// Never zero for two readings of one trend: equal levels under an equal
    /// power state are the same reading by construction. It *is* zero across a
    /// power-state change, which is why callers spanning one test for that.
    fn step(&self, index: usize) -> f64 {
        self.readings[index + 1].percent - self.readings[index].percent
    }
}

/// Every device's history, and the file it is persisted to.
///
/// Interior mutability so that `&self` methods work from `axum` handlers holding
/// a shared reference.
#[derive(Debug)]
pub struct BatteryStore {
    path: PathBuf,
    devices: Mutex<BTreeMap<String, History>>,
}

impl BatteryStore {
    /// Opens the history persisted at `path`.
    ///
    /// Never fails; see [`crate::jsonfile`]. Whatever is loaded is capped, so a
    /// file that grew under a different build cannot grow this process's memory.
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut devices: BTreeMap<String, History> = jsonfile::read(&path, "battery history");
        for history in devices.values_mut() {
            history.truncate();
        }
        Self {
            path,
            devices: Mutex::new(devices),
        }
    }

    /// Folds one poll's reading into `device`'s history.
    pub fn record(&self, device: &str, percent: f64, power: Power, at: OffsetDateTime) {
        self.lock()
            .entry(device.to_owned())
            .or_default()
            .record(percent, power, at);
    }

    /// Each device's history and what it implies, ordered by device id.
    ///
    /// A `BTreeMap` rather than a `HashMap` so the JSON object's keys come out
    /// stable, both on the wire and in the persisted file: a store that reordered
    /// itself on every write would make the file undiffable.
    pub fn reports(&self) -> BTreeMap<String, Report> {
        self.lock()
            .iter()
            .map(|(device, history)| (device.clone(), history.report()))
            .collect()
    }

    /// Writes every device's history to its configured path, atomically.
    ///
    /// Called on each poll that carried a level, rather than only on the polls
    /// that changed one: a poll that merely repeats the level still moves the
    /// window's end, which is what the staleness test reads, and a device polls
    /// at most every thirty seconds. The file is one small object per device.
    pub fn persist(&self) -> Result<()> {
        let devices = self.lock().clone();
        jsonfile::write(&self.path, &devices, "battery history")
    }

    /// A panicking handler must not wedge the history for the rest of the
    /// process: the map is structurally intact either way, so recover the guard.
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, History>> {
        self.devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl Direction {
    /// What the device itself claimed, which is only ever "charging" or nothing:
    /// a reported `charging: false` also covers charge-complete and a board that
    /// cannot tell, so it says nothing about which way the level is going.
    fn reported(power: Power) -> Self {
        match power.charging {
            Some(true) => Self::Charging,
            _ => Self::Unknown,
        }
    }
}

/// When `rate` carries `newest` to its bound: 100% rising, 0% falling.
///
/// Projected from `until` rather than `since`, because the level is only known to
/// have held as of the most recent poll.
fn eta(newest: &Reading, rate: f64) -> Option<(OffsetDateTime, i64)> {
    let remaining = if rate > 0.0 {
        100.0 - newest.percent
    } else {
        newest.percent
    };
    if remaining <= 0.0 {
        return None;
    }

    // A near-flat rate projects centuries out, where the arithmetic stops meaning
    // anything and eventually stops fitting. Both conversions and the addition are
    // checked, so an absurd rate yields no projection rather than a panic.
    let seconds = (remaining / rate.abs()) * SECONDS_PER_HOUR;
    let duration =
        Duration::try_from(std::time::Duration::try_from_secs_f64(seconds).ok()?).ok()?;
    Some((
        newest.until.checked_add(duration)?,
        duration.whole_seconds(),
    ))
}

/// Hours from `from` to `to`, or `None` unless that span is positive.
///
/// A non-positive span is a clock that went backwards between polls, and a rate
/// divided by it is worse than no rate.
fn hours(from: OffsetDateTime, to: OffsetDateTime) -> Option<f64> {
    let seconds = (to - from).as_seconds_f64();
    (seconds > 0.0).then_some(seconds / SECONDS_PER_HOUR)
}

/// Three decimals. These numbers are read by humans on a debug endpoint, and
/// `-0.42372881355932207` says nothing that `-0.424` does not.
fn round(value: f64) -> f64 {
    (value * 1_000.0).round() / 1_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing reported about the power source — every client in service.
    const SILENT: Power = Power {
        charging: None,
        usb_connected: None,
    };
    /// A device that says it is charging.
    const PLUGGED: Power = Power {
        charging: Some(true),
        usb_connected: Some(true),
    };
    /// A device that says it is not.
    const UNPLUGGED: Power = Power {
        charging: Some(false),
        usb_connected: Some(false),
    };

    /// A fixed stamp, so nothing depends on wall-clock time.
    fn at(minutes: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000 + minutes * 60).unwrap()
    }

    /// A history built from `(percent, minute)` polls, in order.
    fn history(polls: &[(f64, i64)]) -> History {
        let mut history = History::default();
        for (percent, minute) in polls {
            history.record(*percent, SILENT, at(*minute));
        }
        history
    }

    /// A history built from `(percent, power, minute)` polls, in order.
    fn powered(polls: &[(f64, Power, i64)]) -> History {
        let mut history = History::default();
        for (percent, power, minute) in polls {
            history.record(*percent, *power, at(*minute));
        }
        history
    }

    /// One poll every five minutes at each level in turn — the shape of a real
    /// panel's reporting, where most polls repeat the level before them.
    fn stepping(levels: &[f64], polls_per_level: i64) -> History {
        let mut history = History::default();
        let mut minute = 0;
        for level in levels {
            for _ in 0..polls_per_level {
                history.record(*level, SILENT, at(minute));
                minute += 5;
            }
        }
        history
    }

    #[test]
    fn a_device_that_has_never_reported_has_no_history_and_no_trend() {
        let report = History::default().report();
        assert_eq!(report.percent, None);
        assert_eq!(report.reported_at, None);
        assert_eq!(report.power, Power::default());
        assert_eq!(report.trend.direction, Direction::Unknown);
        assert_eq!(report.trend.percent_per_hour, None);
        assert!(report.readings.is_empty());
    }

    #[test]
    fn repeated_readings_extend_one_entry_instead_of_adding_more() {
        // The panel in service reports an integer percentage every refresh, so
        // this is the common case by a wide margin: storing each repeat would
        // spend the whole window on them.
        let report = history(&[(80.0, 0), (80.0, 5), (80.0, 10)]).report();
        assert_eq!(report.readings.len(), 1);
        assert_eq!(report.readings[0].polls, 3);
        assert_eq!(report.readings[0].since, at(0));
        assert_eq!(
            report.readings[0].until,
            at(10),
            "a repeat must still move the window's end, or the level looks stale"
        );
    }

    #[test]
    fn one_level_is_not_enough_for_a_direction() {
        let trend = history(&[(80.0, 0), (80.0, 60)]).report().trend;
        assert_eq!(trend.direction, Direction::Unknown);
        assert_eq!(trend.percent_per_hour, None);
        assert_eq!(trend.eta_at, None);
        assert_eq!(trend.steps, 0);
    }

    #[test]
    fn a_device_that_says_it_is_charging_is_charging_before_any_level_moves() {
        // The whole point of reading the header: no inference can beat the
        // charger saying so, and inference needs hours of history to say
        // anything at all.
        let trend = powered(&[(80.0, PLUGGED, 0)]).report().trend;
        assert_eq!(trend.direction, Direction::Charging);
        assert_eq!(trend.percent_per_hour, None, "still nothing measured");
    }

    #[test]
    fn one_crossing_gives_a_direction_but_no_rate() {
        // Two levels bracket one crossing, and a rate needs the time between
        // two: from one, the elapsed time is the level's own plateau, which
        // predates the trend by however long the panel sat there.
        let trend = stepping(&[80.0, 79.0], 6).report().trend;
        assert_eq!(trend.direction, Direction::Discharging);
        assert_eq!(trend.steps, 1);
        assert_eq!(trend.percent_per_hour, None);
        assert_eq!(trend.eta_at, None);
    }

    #[test]
    fn a_discharge_rate_is_measured_between_crossings() {
        // Levels 100 down to 96, one crossing an hour. Four crossings, and the
        // three hours between the first and the last.
        let trend = stepping(&[100.0, 99.0, 98.0, 97.0, 96.0], 12)
            .report()
            .trend;
        assert_eq!(trend.direction, Direction::Discharging);
        assert_eq!(trend.percent_per_hour, Some(-1.0));
        assert_eq!(trend.observed_hours, Some(3.0));
        assert_eq!(trend.steps, 4);
        assert!(!trend.stale);
    }

    #[test]
    fn a_discharge_eta_projects_the_level_reaching_zero() {
        let trend = stepping(&[100.0, 99.0, 98.0, 97.0, 96.0], 12)
            .report()
            .trend;
        // 96% left at 1% an hour, from the last poll at 4h55m.
        assert_eq!(trend.eta_seconds, Some(96 * 3_600));
        assert_eq!(trend.eta_at, Some(at(295 + 96 * 60)));
    }

    #[test]
    fn a_charge_eta_projects_the_level_reaching_a_hundred() {
        let trend = stepping(&[80.0, 85.0, 90.0], 6).report().trend;
        assert_eq!(trend.direction, Direction::Charging);
        // 5% in the half hour between the two crossings.
        assert_eq!(trend.percent_per_hour, Some(10.0));
        assert_eq!(trend.eta_seconds, Some(3_600));
    }

    #[test]
    fn a_full_battery_that_is_still_charging_has_no_eta() {
        let trend = stepping(&[98.0, 99.0, 100.0], 6).report().trend;
        assert_eq!(trend.direction, Direction::Charging);
        assert_eq!(
            trend.eta_at, None,
            "there is no time at which a full battery becomes full"
        );
    }

    #[test]
    fn a_charge_does_not_pollute_the_discharge_that_follows_it() {
        // Charged 80 to 100, then left alone: 99, 98, 97 an hour apart. Averaged
        // over the whole history the level looks like it is still rising.
        let mut history = stepping(&[80.0, 90.0, 100.0], 6);
        for (percent, minute) in [(99.0, 180), (98.0, 240), (97.0, 300)] {
            history.record(percent, SILENT, at(minute));
        }

        let trend = history.report().trend;
        assert_eq!(trend.direction, Direction::Discharging);
        assert_eq!(trend.percent_per_hour, Some(-1.0));
        assert_eq!(
            trend.steps, 3,
            "only the crossings since the level turned over belong to this trend"
        );
    }

    #[test]
    fn the_time_spent_at_the_level_a_charge_stopped_at_is_not_charged_to_the_discharge() {
        // Unplugged at 100% and left for five hours before the first crossing.
        // Anchoring the discharge on when 100% was *reached* stretches it over
        // that idle time and reports a third of the real rate.
        let mut history = History::default();
        for hour in 0..=5 {
            history.record(100.0, SILENT, at(hour * 60));
        }
        for (percent, hour) in [(99.0, 6), (98.0, 7), (97.0, 8)] {
            history.record(percent, SILENT, at(hour * 60));
        }

        let trend = history.report().trend;
        assert_eq!(
            trend.percent_per_hour,
            Some(-1.0),
            "1% an hour is what the crossings say: {trend:?}"
        );
        assert_eq!(trend.observed_hours, Some(2.0));
    }

    #[test]
    fn a_charger_going_in_starts_a_new_trend_at_that_poll() {
        // Falling on the cell, then plugged in at an unchanged 58%. The rate must
        // come from the charge alone: the discharge before it is a different
        // regime, and waiting for the level to turn over would blend the two.
        let trend = powered(&[
            (60.0, UNPLUGGED, 0),
            (59.0, UNPLUGGED, 60),
            (58.0, UNPLUGGED, 120),
            (58.0, PLUGGED, 180),
            (59.0, PLUGGED, 210),
            (60.0, PLUGGED, 240),
        ])
        .report()
        .trend;

        assert_eq!(trend.direction, Direction::Charging);
        assert_eq!(
            trend.steps, 2,
            "the two crossings since the charger went in"
        );
        assert_eq!(trend.observed_hours, Some(0.5));
        assert_eq!(trend.percent_per_hour, Some(2.0));
        // 40% to go at 2% an hour, from the last poll.
        assert_eq!(trend.eta_seconds, Some(20 * 3_600));
    }

    #[test]
    fn a_power_state_change_at_an_unchanged_level_invents_no_direction() {
        // The charger came out at 100%. Nothing has moved, and reading that poll
        // as a level going down would be a rate's worth of fiction. The reported
        // state is all there is until the level actually falls.
        let report = powered(&[(100.0, PLUGGED, 0), (100.0, UNPLUGGED, 60)]).report();
        assert_eq!(report.trend.direction, Direction::Unknown);
        assert_eq!(report.trend.percent_per_hour, None);
        assert_eq!(report.trend.steps, 0);
        assert_eq!(report.power, UNPLUGGED, "what it says is still reported");

        // And once it does fall, the direction is read off that.
        let mut history = powered(&[(100.0, PLUGGED, 0), (100.0, UNPLUGGED, 60)]);
        history.record(99.0, UNPLUGGED, at(120));
        assert_eq!(
            history.report().trend.direction,
            Direction::Discharging,
            "one crossing is enough for a direction"
        );
    }
    #[test]
    fn a_reported_charge_wins_over_a_falling_level() {
        // A panel drawing more than its charger supplies. Both facts are real and
        // both are reported: the device says charging, the level says otherwise.
        let trend = powered(&[
            (60.0, PLUGGED, 0),
            (59.0, PLUGGED, 60),
            (58.0, PLUGGED, 120),
        ])
        .report()
        .trend;

        assert_eq!(trend.direction, Direction::Charging, "the device said so");
        assert_eq!(trend.percent_per_hour, Some(-1.0));
        assert_eq!(
            trend.eta_seconds,
            Some(58 * 3_600),
            "the projection follows the measurement, not the claim"
        );
    }

    #[test]
    fn a_reported_not_charging_leaves_the_direction_to_the_measurement() {
        // `charging: false` also covers charge-complete and a board that cannot
        // tell, so it must not be read as "discharging".
        let report = powered(&[
            (58.0, UNPLUGGED, 0),
            (59.0, UNPLUGGED, 60),
            (60.0, UNPLUGGED, 120),
        ])
        .report();
        assert_eq!(report.trend.direction, Direction::Charging);
        assert_eq!(report.power.charging, Some(false));
        assert_eq!(report.power.usb_connected, Some(false));
    }

    #[test]
    fn a_trend_that_stopped_moving_goes_steady_and_stops_projecting() {
        // Charged to 90% at 20% an hour, then the charger came out: the level has
        // sat still for seven hours, which no 20%-an-hour trend explains.
        let mut history = stepping(&[70.0, 80.0, 90.0], 6);
        for hour in 2..=8 {
            history.record(90.0, SILENT, at(hour * 60));
        }

        let trend = history.report().trend;
        assert!(trend.stale);
        assert_eq!(trend.direction, Direction::Steady);
        assert_eq!(
            trend.eta_at, None,
            "a stale trend must not project: the panel is not charging any more"
        );
        assert_eq!(
            trend.percent_per_hour,
            Some(20.0),
            "the measured rate is still reported; it is the projection that is withheld"
        );
    }

    #[test]
    fn a_plateau_the_length_of_one_step_is_not_stale() {
        // One step short of the next crossing is what quantisation looks like on
        // a device reporting whole percentages, and it happens every step.
        let mut history = stepping(&[80.0, 79.0, 78.0], 12);
        history.record(78.0, SILENT, at(180));

        let trend = history.report().trend;
        assert!(!trend.stale, "{trend:?}");
        assert_eq!(trend.direction, Direction::Discharging);
    }

    #[test]
    fn history_is_capped_and_drops_the_oldest_readings() {
        let mut history = History::default();
        for step in 0..(MAX_READINGS as i64 + 20) {
            history.record(1000.0 - step as f64, SILENT, at(step * 5));
        }

        let report = history.report();
        assert_eq!(report.readings.len(), MAX_READINGS);
        assert_eq!(
            report.readings[0].percent,
            1000.0 - 20.0,
            "the oldest readings are the ones dropped"
        );
        assert_eq!(report.percent, Some(1000.0 - (MAX_READINGS as f64 + 19.0)));
    }

    #[test]
    fn a_clock_that_went_backwards_yields_no_rate_rather_than_a_wrong_one() {
        let trend = history(&[(80.0, 0), (79.0, 120), (78.0, 60)])
            .report()
            .trend;
        assert_eq!(trend.direction, Direction::Discharging);
        assert_eq!(trend.percent_per_hour, None);
        assert_eq!(trend.eta_at, None);
    }

    #[test]
    fn a_rate_flat_enough_to_project_past_the_end_of_time_yields_no_eta() {
        // 1e-7% an hour is 57,000 years to empty. The projection has to fail
        // safely rather than overflow the calendar.
        let far_future = OffsetDateTime::from_unix_timestamp(200_000_000_000).unwrap();
        let mut history = History::default();
        for step in 0..3 {
            let percent = 50.0 - f64::from(step) * 1e-7;
            history.record(percent, SILENT, far_future + Duration::hours(step.into()));
        }

        let trend = history.report().trend;
        assert_eq!(trend.direction, Direction::Discharging);
        assert_eq!(
            trend.eta_at, None,
            "the projection overflows the calendar and must be withheld, not panicked on"
        );
        assert_eq!(trend.eta_seconds, None);
    }

    #[test]
    fn the_report_serialises_with_rfc_3339_stamps_and_a_lowercase_direction() {
        let json = serde_json::to_value(stepping(&[90.0, 89.0], 6).report()).unwrap();
        assert_eq!(json["percent"], 89.0);
        assert_eq!(json["reported_at"], "2023-11-14T23:08:20Z");
        assert_eq!(json["trend"]["direction"], "discharging");
        assert!(json["power"]["charging"].is_null());
        assert_eq!(json["readings"][0]["since"], "2023-11-14T22:13:20Z");
        assert_eq!(json["readings"][0]["polls"], 6);
    }

    /// A store file of its own per test, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "paneld-battery-{}-{name}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn file(&self) -> PathBuf {
            self.0.join("battery.json")
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_persisted_store_reloads_every_reading_and_the_trend_with_it() {
        let dir = Dir::new("roundtrip");
        let store = BatteryStore::load(dir.file());
        for (percent, minute) in [(80.0, 0), (80.0, 30), (79.0, 60), (78.0, 120)] {
            store.record("kindle", percent, PLUGGED, at(minute));
        }
        store.persist().unwrap();

        let reloaded = BatteryStore::load(dir.file()).reports();
        let kindle = &reloaded["kindle"];
        assert_eq!(kindle.percent, Some(78.0));
        assert_eq!(kindle.readings.len(), 3);
        assert_eq!(kindle.readings[0].polls, 2, "the run length survives");
        assert_eq!(kindle.readings[0].until, at(30));
        assert_eq!(kindle.power, PLUGGED);
        assert_eq!(
            kindle.trend.percent_per_hour,
            Some(-1.0),
            "the rate is measured from reloaded samples: {:?}",
            kindle.trend
        );
    }

    #[test]
    fn devices_keep_separate_histories_across_a_reload() {
        let dir = Dir::new("devices");
        let store = BatteryStore::load(dir.file());
        store.record("kitchen", 90.0, SILENT, at(0));
        store.record("hallway", 20.0, UNPLUGGED, at(0));
        store.persist().unwrap();

        let reloaded = BatteryStore::load(dir.file()).reports();
        assert_eq!(reloaded["kitchen"].percent, Some(90.0));
        assert_eq!(reloaded["hallway"].percent, Some(20.0));
        assert_eq!(reloaded["hallway"].power, UNPLUGGED);
    }

    #[test]
    fn a_file_holding_more_readings_than_the_cap_is_trimmed_on_load() {
        // The cap is a memory bound, so it has to hold against a file written by
        // a build with a larger one, or hand-edited.
        let dir = Dir::new("cap");
        let store = BatteryStore::load(dir.file());
        for step in 0..(MAX_READINGS as i64 + 20) {
            store.record("kindle", 1000.0 - step as f64, SILENT, at(step * 5));
        }
        store.persist().unwrap();
        // Persist wrote a capped history, so widen the file by hand.
        let padded = format!(
            r#"{{"kindle":[{{"percent":1,"power":{{}},"since":"2023-01-01T00:00:00Z","until":"2023-01-01T00:00:00Z","polls":1}},{}]}}"#,
            std::fs::read_to_string(dir.file())
                .unwrap()
                .split_once('[')
                .unwrap()
                .1
                .rsplit_once(']')
                .unwrap()
                .0
        );
        std::fs::write(dir.file(), padded).unwrap();

        let reloaded = BatteryStore::load(dir.file()).reports();
        assert_eq!(reloaded["kindle"].readings.len(), MAX_READINGS);
        assert_ne!(
            reloaded["kindle"].readings[0].percent, 1.0,
            "the padded oldest reading is the one dropped"
        );
    }

    #[test]
    fn an_unwritable_path_is_an_error_and_not_a_panic() {
        // The poll path logs this and carries on; it must never unwind into a
        // handler.
        let store = BatteryStore::load("/proc/paneld-cannot-write/battery.json");
        store.record("kindle", 50.0, SILENT, at(0));
        assert!(store.persist().is_err());
        assert_eq!(store.reports()["kindle"].percent, Some(50.0));
    }
}
