//! Server-side SVG bar charts for the admin stats page.
//!
//! The stats page renders four charts, tokens, cost, requests and request
//! latency, as plain `<svg>` markup generated here: no JavaScript, no
//! charting crate and no template logic beyond the `|safe` filter that
//! injects the returned string. Heights are normalized to the largest bucket
//! of the series, every bar carries a `<title>` so the browser shows a native
//! tooltip without any script, and all of them share the same viewport,
//! gutters and bar geometry.
//!
//! [`Granularity`] picks the x-axis from the selected day range: one or two
//! days are drawn hour by hour, three days and more day by day, the fixed
//! 1 through 31 grid the daily charts have always used. The axis is the one
//! place that knows how many slots a chart has, which of them carry a label
//! and how a bucket is placed and named in a tooltip, so both granularities
//! share every drawing rule.
//!
//! The token chart consumes [`DayUsage`] or [`HourUsage`] rows exactly as the
//! SQL aggregate hands them over and buckets them with [`aggregate_daily`].
//! The cost chart takes a pre-summed `(bucket, micro-USD)` series instead of
//! the same rows, because a price is a function of the price table of the
//! key's channel ([`crate::usage::group_cost`]), which no usage row carries:
//! the caller prices its own query groups and sums them per bucket, so no
//! cost is ever derived from a row priced with another channel's rates. The
//! status chart consumes [`DayStatusCodeUsage`] or [`HourStatusCodeUsage`]
//! rows and stacks one segment per exact response status, with a legend that
//! maps the palette colors to the codes. The latency chart consumes
//! [`HourLatency`] rows and plots the mean `duration_ms` of a bucket, on a
//! daily range the mean of every timed request of a day: the hours of a day
//! fold into it weighted by the timed requests each hour covers.
//!
//! A bucket whose label names no slot of the axis is skipped, and a series
//! that maps to no bar at all renders a placeholder note inside a valid,
//! empty chart rather than an error.
//!
//! Colors travel as [`ChartTheme`] rather than through CSS: the SVG is built
//! here, so the palette of the theme the page is showing is chosen on the
//! server. The values are the same ones `assets/admin.css` defines for the
//! light and the dark palette, and the page reads the theme from the cookie
//! the toggle script writes (see [`theme_from_headers`]).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use axum::http::HeaderMap;
use time::{Date, Duration};

/// The aggregate rows the SQL layer defines, reused here so both branches of
/// the stats page read one shape instead of two identical copies.
pub(crate) use crate::db::request_logs::{
    DayStatusCodeUsage, DayUsage, HourLatency, HourStatusCodeUsage, HourUsage, StatusBucket,
};

/// Viewport width in user units; the root element scales it to the card.
const VIEW_W: f64 = 720.0;
/// Viewport height in user units.
const VIEW_H: f64 = 260.0;
/// Left gutter before the first slot: wide enough for the right-anchored
/// labels of the y-axis ticks, so the widest of them, a cost such as
/// `$1000.00` or a latency such as `850 ms`, never runs past the viewport
/// edge.
const PAD_LEFT: f64 = 52.0;
/// Right gutter.
const PAD_RIGHT: f64 = 12.0;
/// Top gutter: the axis label sits here, above the tallest bar.
const PAD_TOP: f64 = 20.0;
/// Bottom gutter under the bucket labels.
const PAD_BOTTOM: f64 = 30.0;
/// Horizontal extent of the plotting area.
const PLOT_W: f64 = VIEW_W - PAD_LEFT - PAD_RIGHT;
/// Vertical extent of the plotting area.
const PLOT_H: f64 = VIEW_H - PAD_TOP - PAD_BOTTOM;
/// Day slots on the x-axis of a daily chart: a fixed 1 through 31 so the axis
/// is stable across months independent of the data.
const DAY_SLOTS: u32 = 31;
/// Hour slots of one day on an hourly axis.
const HOUR_SLOTS: u32 = 24;
/// The longest range an hourly chart covers: one or two days.
const HOURLY_MAX_DAYS: u32 = 2;
/// Width of the bar inside a slot, as a fraction of that slot.
const BAR_FRACTION: f64 = 0.62;
/// The three letter month names an hourly label of a two day range carries,
/// in calendar order. The stored bucket labels are numeric, so the chart
/// abbreviates the month itself instead of formatting a date.
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
/// Y coordinate of the baseline every bar grows from.
const BASE_Y: f64 = PAD_TOP + PLOT_H;

/// The light `--accent` from `assets/admin.css`, the input-token segment.
const ACCENT_FILL: &str = "#2563eb";
/// The light `--text-muted` from `assets/admin.css`, the output-token
/// segment.
const MUTED_FILL: &str = "#6a7280";
/// The light `--accent-dark` from `assets/admin.css`, the cost bars and the
/// fifth slot of the per-code palette.
const COST_FILL: &str = "#1e50c8";
/// The light `--ok` from `assets/admin.css`, the green slot of the per-code
/// palette.
const OK_FILL: &str = "#15803d";
/// The light `--warn` from `assets/admin.css`, the amber slot of the per-code
/// palette.
const WARN_FILL: &str = "#92400e";
/// The light `--danger` from `assets/admin.css`, the red slot of the per-code
/// palette.
const DANGER_FILL: &str = "#b91c1c";
/// The light `--border` from `assets/admin.css`, the axis rules.
const BORDER_STROKE: &str = "#d9dde3";

/// The dark `--accent` from `assets/admin.css`: GitHub's link blue.
const DARK_ACCENT_FILL: &str = "#2f81f7";
/// The dark `--text-muted` from `assets/admin.css`.
const DARK_MUTED_FILL: &str = "#8b949e";
/// The dark `--accent-dark` from `assets/admin.css`, kept apart from the dark
/// accent so the cost bars stay distinguishable from the input-token bars.
const DARK_COST_FILL: &str = "#1f6feb";
/// The dark `--ok` from `assets/admin.css`: GitHub's green.
const DARK_OK_FILL: &str = "#3fb950";
/// The dark `--warn` from `assets/admin.css`: GitHub's attention amber.
const DARK_WARN_FILL: &str = "#d29922";
/// The dark `--danger` from `assets/admin.css`: GitHub's red.
const DARK_DANGER_FILL: &str = "#f85149";
/// The dark `--border` from `assets/admin.css`, the axis rules.
const DARK_BORDER_STROKE: &str = "#30363d";

/// The segment colors of the requests chart, cycled over the status codes in
/// their canonical order: the light `--ok`, `--warn`, `--danger`, `--accent`,
/// `--accent-dark` and `--text-muted` from `assets/admin.css`, so the three
/// named classes keep the colors the class chart used. A series with more
/// codes than slots repeats the palette rather than inventing a color.
const STATUS_CODE_PALETTE: [&str; 6] = [
    OK_FILL,
    WARN_FILL,
    DANGER_FILL,
    ACCENT_FILL,
    COST_FILL,
    MUTED_FILL,
];

/// The same cycle in the dark palette, in the same order, so a code keeps the
/// slot it would take in the light theme.
const DARK_STATUS_CODE_PALETTE: [&str; 6] = [
    DARK_OK_FILL,
    DARK_WARN_FILL,
    DARK_DANGER_FILL,
    DARK_ACCENT_FILL,
    DARK_COST_FILL,
    DARK_MUTED_FILL,
];

/// The name of the cookie the admin UI toggle script writes, and the only
/// place the theme of a page request is carried.
pub(crate) const THEME_COOKIE: &str = "theme";

/// Which palette a chart is painted in. The admin stylesheet resolves the
/// same choice for the page around the chart, so the two stay in step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ChartTheme {
    /// The light palette, the values the stylesheet defines in `:root`.
    #[default]
    Light,
    /// The dark palette, the values the stylesheet applies on
    /// `prefers-color-scheme: dark` and on the stored dark choice.
    Dark,
}

impl ChartTheme {
    /// The theme a cookie value names. Only the exact value the toggle script
    /// writes selects the dark palette; every other value, including a cookie
    /// another application set, keeps the light one the stylesheet paints
    /// without a stored choice.
    pub(crate) fn from_cookie(value: &str) -> Self {
        if value.trim() == "dark" {
            ChartTheme::Dark
        } else {
            ChartTheme::Light
        }
    }

    /// The palette of this theme.
    fn palette(self) -> &'static Palette {
        match self {
            ChartTheme::Light => &LIGHT_PALETTE,
            ChartTheme::Dark => &DARK_PALETTE,
        }
    }
}

/// The colors one theme paints the charts in, named after the custom
/// properties of `assets/admin.css` they mirror.
struct Palette {
    /// `--accent`: input-token segments, the latency bars and the fourth
    /// status slot.
    accent: &'static str,
    /// `--text-muted`: output-token segments, axis labels, the empty note and
    /// the sixth status slot.
    muted: &'static str,
    /// `--accent-dark`: cost bars and the fifth status slot.
    cost: &'static str,
    /// `--border`: the axis rules.
    border: &'static str,
    /// The per-code colors in their canonical cycle order.
    status: [&'static str; 6],
}

/// The light palette, unchanged from the stylesheet this chart has always
/// mirrored.
const LIGHT_PALETTE: Palette = Palette {
    accent: ACCENT_FILL,
    muted: MUTED_FILL,
    cost: COST_FILL,
    border: BORDER_STROKE,
    status: STATUS_CODE_PALETTE,
};

/// The GitHub Dark palette.
const DARK_PALETTE: Palette = Palette {
    accent: DARK_ACCENT_FILL,
    muted: DARK_MUTED_FILL,
    cost: DARK_COST_FILL,
    border: DARK_BORDER_STROKE,
    status: DARK_STATUS_CODE_PALETTE,
};

/// The chart theme a page request asks for, read from the `theme` cookie of
/// its `Cookie` header and defaulting to the light palette. The toggle script
/// writes the cookie, so the server-rendered charts follow the choice the
/// stylesheet shows on the same paint.
pub(crate) fn theme_from_headers(headers: &HeaderMap) -> ChartTheme {
    for header in headers.get_all(axum::http::header::COOKIE) {
        let Ok(value) = header.to_str() else {
            continue;
        };
        for pair in value.split(';') {
            if let Some((name, value)) = pair.split_once('=') {
                if name.trim() == THEME_COOKIE {
                    return ChartTheme::from_cookie(value);
                }
            }
        }
    }
    ChartTheme::Light
}

/// The bucket one bar of a chart spans, chosen from the selected day range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Granularity {
    /// One bar per hour: the range covers one or two days, where a bar per
    /// day would hide the shape of the day an admin is looking at.
    Hourly,
    /// One bar per day: the range covers three days or more, where a bar per
    /// hour would be too thin to read.
    Daily,
}

impl Granularity {
    /// The granularity of an inclusive range of UTC days.
    ///
    /// The bounds are the days the `from` and `to` parameters name, so one
    /// day is a one day range and two consecutive days are a two day one; a
    /// range that names no day at all (`to` before `from`) falls back to the
    /// daily axis, the layout that renders any series.
    pub(crate) fn from_range(from: Date, to: Date) -> Self {
        let days = (to - from).whole_days().saturating_add(1);
        if (1..=i64::from(HOURLY_MAX_DAYS)).contains(&days) {
            Granularity::Hourly
        } else {
            Granularity::Daily
        }
    }
}

/// The x-axis of one chart: how many slots it has, which of them carry a
/// label, and where and how a slot is drawn and named.
///
/// One axis serves every chart, so a bucket can never be placed or titled by
/// two different rules. The daily axis is the fixed 1 through 31 grid the
/// charts have always used, independent of the selected range; the hourly
/// axis is one slot per hour of the one or two days a range covers, in clock
/// order from the first hour of its first day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Axis {
    /// One slot per day of the month, the first day in the first slot.
    Daily,
    /// One slot per hour, starting at the first hour of `from`.
    Hourly { from: Date, days: u32 },
}

impl Axis {
    /// The daily axis, one slot per day of the month.
    fn daily() -> Self {
        Axis::Daily
    }

    /// The hourly axis of the days `from` through `to`, clamped to
    /// [`HOURLY_MAX_DAYS`] days: a caller passes the range of an hourly
    /// chart, which [`Granularity::from_range`] has already sized.
    fn hourly(from: Date, to: Date) -> Self {
        let days = (to - from)
            .whole_days()
            .saturating_add(1)
            .clamp(1, i64::from(HOURLY_MAX_DAYS));
        Axis::Hourly {
            from,
            days: days as u32,
        }
    }

    /// The number of slots bars are placed in.
    fn slots(self) -> u32 {
        match self {
            Axis::Daily => DAY_SLOTS,
            Axis::Hourly { days, .. } => days * HOUR_SLOTS,
        }
    }

    /// The period one slot spans, as a chart heading names it.
    fn period(self) -> &'static str {
        match self {
            Axis::Daily => "day",
            Axis::Hourly { .. } => "hour",
        }
    }

    /// The width of one slot.
    fn slot_w(self) -> f64 {
        PLOT_W / f64::from(self.slots())
    }

    /// The width of the bar drawn inside a slot.
    fn bar_w(self) -> f64 {
        self.slot_w() * BAR_FRACTION
    }

    /// Left edge of the bar of a slot.
    fn bar_x(self, slot: u32) -> f64 {
        PAD_LEFT + f64::from(slot) * self.slot_w() + (self.slot_w() - self.bar_w()) / 2.0
    }

    /// Horizontal center of a slot, where its axis label sits.
    fn slot_center(self, slot: u32) -> f64 {
        PAD_LEFT + f64::from(slot) * self.slot_w() + self.slot_w() / 2.0
    }

    /// Whether a slot carries an axis label. Both axes stay sparse: the first
    /// day and every fifth one of the month, and every third hour of a one
    /// day range or every sixth hour of a two day one, which keeps the labels
    /// of the widest axis (48 slots) about as far apart as the daily ones.
    fn labelled(self, slot: u32) -> bool {
        match self {
            Axis::Daily => slot == 0 || (slot + 1).is_multiple_of(5),
            Axis::Hourly { days: 1, .. } => slot.is_multiple_of(3),
            Axis::Hourly { .. } => slot.is_multiple_of(6),
        }
    }

    /// The text under a slot: the day of the month on the daily axis, the
    /// clock hour on a one day range, and the day with the hour on a two day
    /// one, where an hour alone would name two different buckets.
    fn label(self, slot: u32) -> String {
        match self {
            Axis::Daily => (slot + 1).to_string(),
            Axis::Hourly { days: 1, .. } => hour_text(slot),
            Axis::Hourly { from, .. } => {
                let day = shift_day(from, i64::from(slot / HOUR_SLOTS));
                format!(
                    "{} {} {}",
                    MONTHS[month_index(day)],
                    day.day(),
                    hour_text(slot)
                )
            }
        }
    }

    /// How a slot names itself in a tooltip: `Day 4` on the daily axis, the
    /// bucket's own label on the hourly one.
    fn name(self, slot: u32) -> String {
        match self {
            Axis::Daily => format!("Day {}", slot + 1),
            Axis::Hourly { .. } => self.label(slot),
        }
    }

    /// The slot a bucket label names, `None` when the axis carries no such
    /// slot: an unparsable label, a day the month does not have, and a bucket
    /// outside the range of an hourly axis are all skipped the same way.
    fn slot_of(self, bucket: &str) -> Option<u32> {
        match self {
            Axis::Daily => day_slot(bucket),
            Axis::Hourly { from, days } => {
                let (day, hour) = hour_parts(bucket)?;
                let offset = (day - from).whole_days();
                if !(0..i64::from(days)).contains(&offset) {
                    return None;
                }
                Some(offset as u32 * HOUR_SLOTS + u32::from(hour))
            }
        }
    }
}

/// The slot of the day of the month a bucket label names, `None` when the
/// label carries no parsable day of the month in 1 through 31. The day is the
/// last `-` separated part of the label, so a `YYYY-MM-DD` label and a bare
/// day both read as the day they name.
fn day_slot(label: &str) -> Option<u32> {
    let day = label.split(' ').next()?;
    let number: u32 = day.rsplit('-').next()?.parse().ok()?;
    // The check is written as a subtraction so that a day of a month the
    // label names as `00` is rejected instead of underflowing.
    number.checked_sub(1).filter(|slot| *slot < DAY_SLOTS)
}

/// The day and the hour a `YYYY-MM-DD HH` bucket label names, `None` for
/// anything else.
fn hour_parts(label: &str) -> Option<(Date, u8)> {
    let (day, time) = label.split_once(' ')?;
    let day = parse_day(day)?;
    let hour: u8 = time.get(..2)?.parse().ok()?;
    (hour < HOUR_SLOTS as u8).then_some((day, hour))
}

/// Reads the `YYYY-MM-DD` day of a bucket label; `None` for anything else,
/// including a day its month does not have (`2026-02-30`) and a year, month
/// or day of the wrong width.
fn parse_day(raw: &str) -> Option<Date> {
    let (year, rest) = raw.split_once('-')?;
    let (month, day) = rest.split_once('-')?;
    if !digits(year, 4) || !digits(month, 2) || !digits(day, 2) {
        return None;
    }
    let month = time::Month::try_from(month.parse::<u8>().ok()?).ok()?;
    Date::from_calendar_date(year.parse().ok()?, month, day.parse::<u8>().ok()?).ok()
}

/// Exactly `width` ASCII digits.
fn digits(text: &str, width: usize) -> bool {
    text.len() == width && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The clock label of an hourly slot, `14:00`.
fn hour_text(slot: u32) -> String {
    format!("{:02}:00", slot % HOUR_SLOTS)
}

/// The index of a day's month in [`MONTHS`].
fn month_index(day: Date) -> usize {
    usize::from(u8::from(day.month())) - 1
}

/// The day `offset` days after `day`. No axis asks for a day the calendar
/// cannot hold, and a date outside it stays where it is rather than panicking.
fn shift_day(day: Date, offset: i64) -> Date {
    day.checked_add(Duration::days(offset)).unwrap_or(day)
}

/// The measure of the tokens chart, and of the other charts; the axis adds
/// the period its bars span.
const TOKENS_NAME: &str = "Tokens";
/// The measure of the cost chart.
const COST_NAME: &str = "Cost";
/// The measure of the requests chart.
const REQUESTS_NAME: &str = "Requests";
/// The measure of the latency chart.
const LATENCY_NAME: &str = "Latency";
/// Note rendered when a series maps to no bar.
const EMPTY_NOTE: &str = "No usage in this period";
/// Class of the legend group of a chart, which is also what tells the legend
/// rects apart from the bar rects.
const LEGEND_OPEN: &str = "<g class=\"chart-legend\">";

/// The accessible name of one chart: its measure and the period its bars
/// span, `Tokens per day` or `Tokens per hour`.
fn chart_label(name: &str, axis: Axis) -> String {
    format!("{name} per {}", axis.period())
}

/// Which pair of values a chart plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartKind {
    /// Input and output tokens as two stacked segments of one bar.
    Stacked,
    /// One bar per bucket from a caller-priced cost series.
    Single,
}

impl ChartKind {
    /// The text above the plot naming the largest value of the series.
    fn max_label(self, max: i64) -> String {
        match self {
            ChartKind::Stacked => format!("max {} tokens", compact_count(max)),
            ChartKind::Single => format!("max {}", compact_usd(max)),
        }
    }

    /// A value label of the y-axis: the same compact read of the series the
    /// max label above the plot uses, so a tick and the top of the plot speak
    /// one language, and an integer series never shows a formatted fraction.
    fn tick_label(self, value: f64) -> String {
        match self {
            ChartKind::Stacked => count_tick(value),
            ChartKind::Single => compact_usd(value.round() as i64),
        }
    }
}

/// One slot of a chart series, already summed across keys and models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bar {
    /// Index of the slot on the axis the series was bucketed into.
    slot: u32,
    /// Bottom segment: input tokens, or the micro-USD cost.
    input: i64,
    /// Top segment: output tokens; zero for the cost chart.
    output: i64,
    /// Logged requests behind the bar.
    requests: i64,
}

impl Bar {
    /// Height driver of the bar, saturating so no input can overflow.
    fn total(&self) -> i64 {
        self.input.saturating_add(self.output)
    }
}

/// One slot of the latency series: the mean duration of a bucket.
#[derive(Debug, Clone, Copy, PartialEq)]
struct LatencyBar {
    /// Index of the slot on the axis the series was bucketed into.
    slot: u32,
    /// Mean `duration_ms` of the bucket.
    avg_ms: f64,
    /// Timed requests the mean covers.
    requests: i64,
}

/// One slot of the requests chart: the request count of every exact response
/// status the bucket logged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusCodeBar {
    /// Index of the slot on the axis the series was bucketed into.
    slot: u32,
    /// One `(status, request count)` pair per code of the bucket, in the
    /// canonical code order the segments stack in.
    counts: Vec<(Option<i64>, i64)>,
}

impl StatusCodeBar {
    /// Height driver of the bar, saturating so no input can overflow.
    fn total(&self) -> i64 {
        self.counts
            .iter()
            .fold(0, |total, (_, count)| total.saturating_add(*count))
    }
}

/// The canonical order of a status across the whole chart: exact codes
/// ascending, then the requests that carry no status at all. Ordering the
/// codes here rather than trusting the row order is what keeps the segments,
/// the legend and the colors of one bucket identical whatever order the
/// backend returns the rows in, and it puts the `n/a` segment last.
fn code_order(status: Option<i64>) -> (u8, i64) {
    match status {
        Some(code) => (0, code),
        None => (1, 0),
    }
}

/// The label of a status: its exact code, or `n/a` for a request that never
/// reached an upstream and so has none.
fn code_label(status: Option<i64>) -> String {
    match status {
        Some(code) => code.to_string(),
        None => "n/a".to_string(),
    }
}

/// The label of a status in the legend: its code, or `n/a` followed by the
/// class it belongs to, because a legend entry with no code of its own says
/// more when it names the bucket those requests land in.
fn legend_label(status: Option<i64>) -> String {
    match status {
        Some(code) => code.to_string(),
        None => format!("n/a ({})", StatusBucket::Other.label()),
    }
}

/// Sums the rows into one `(day, prompt tokens, completion tokens,
/// requests)` tuple per day, ordered by the day label.
///
/// Rows with a NULL model take part in the sums like any other: a daily
/// total is model-agnostic. Unparsable day labels are kept here, because the
/// caller may want to show them; the charts skip them when they place bars.
pub(crate) fn aggregate_daily(rows: &[DayUsage]) -> Vec<(String, i64, i64, i64)> {
    let mut by_day: BTreeMap<&str, (i64, i64, i64)> = BTreeMap::new();
    for row in rows {
        let slot = by_day.entry(row.day.as_str()).or_default();
        slot.0 = slot.0.saturating_add(row.prompt_tokens);
        slot.1 = slot.1.saturating_add(row.completion_tokens);
        slot.2 = slot.2.saturating_add(row.requests);
    }
    by_day
        .into_iter()
        .map(|(day, (prompt, completion, requests))| (day.to_owned(), prompt, completion, requests))
        .collect()
}

/// Renders the tokens-per-day chart: one stacked bar per day that has data,
/// input tokens at the bottom and output tokens on top, in the palette of
/// `theme`.
///
/// Heights are normalized to the largest daily total, and days without data
/// stay empty slots so the axis keeps a fixed 1 through 31 extent. An empty
/// result or a series of unparsable day labels renders a placeholder note.
pub(crate) fn daily_token_bars(rows: &[DayUsage], theme: ChartTheme) -> String {
    let axis = Axis::daily();
    bar_chart(
        chart_label(TOKENS_NAME, axis),
        ChartKind::Stacked,
        axis,
        token_series(rows, axis),
        theme,
    )
}

/// Renders the tokens-per-hour chart of a one or two day range: the hourly
/// twin of [`daily_token_bars`], one stacked bar per hour of the range, with
/// the hour (and, on a two day range, the day) under it on the axis.
pub(crate) fn hourly_token_bars(
    rows: &[HourUsage],
    from: Date,
    to: Date,
    theme: ChartTheme,
) -> String {
    let axis = Axis::hourly(from, to);
    bar_chart(
        chart_label(TOKENS_NAME, axis),
        ChartKind::Stacked,
        axis,
        hour_token_series(rows, axis),
        theme,
    )
}

/// Renders the cost-per-day chart: one bar per day, already priced by the
/// caller, in the palette of `theme`.
///
/// The series is `(day label, micro-USD)` and is summed per day here, so the
/// caller can hand over one entry per query group instead of pre-bucketing.
/// See the module docs for why cost does not travel as [`DayUsage`].
pub(crate) fn daily_cost_bars(costs: &[(String, i64)], theme: ChartTheme) -> String {
    let axis = Axis::daily();
    bar_chart(
        chart_label(COST_NAME, axis),
        ChartKind::Single,
        axis,
        cost_series(costs, axis),
        theme,
    )
}

/// Renders the cost-per-hour chart of a one or two day range: the same
/// caller-priced series as [`daily_cost_bars`], bucketed by hour.
pub(crate) fn hourly_cost_bars(
    costs: &[(String, i64)],
    from: Date,
    to: Date,
    theme: ChartTheme,
) -> String {
    let axis = Axis::hourly(from, to);
    bar_chart(
        chart_label(COST_NAME, axis),
        ChartKind::Single,
        axis,
        cost_series(costs, axis),
        theme,
    )
}

/// Renders the requests-per-day chart: one bar per day that has data, a
/// segment per exact upstream status code, and a legend mapping the colors to
/// the codes.
///
/// The codes are the ones the SQL aggregate groups by, so the bar segments
/// and the tooltip figures always agree, and the class filter that narrows
/// the aggregate narrows what the chart shows. An empty series renders a
/// placeholder note, exactly like the other charts.
pub(crate) fn daily_status_code_bars(rows: &[DayStatusCodeUsage], theme: ChartTheme) -> String {
    let axis = Axis::daily();
    status_code_chart(status_code_series(rows, axis), axis, theme)
}

/// Renders the requests-per-hour chart of a one or two day range: the hourly
/// twin of [`daily_status_code_bars`], one segment per exact status code of
/// an hour.
pub(crate) fn hourly_status_code_bars(
    rows: &[HourStatusCodeUsage],
    from: Date,
    to: Date,
    theme: ChartTheme,
) -> String {
    let axis = Axis::hourly(from, to);
    status_code_chart(hour_status_code_series(rows, axis), axis, theme)
}

/// Renders the latency chart of a range: one bar per bucket, the mean
/// `duration_ms` of the bucket scaled to the largest mean of the series, in
/// the palette of `theme`.
///
/// The series comes from the hourly aggregate, and the granularity of the
/// range decides how its hours are read: a one or two day range plots one bar
/// per hour, and a longer one folds the hours of a day into that day's mean,
/// weighted by the timed requests each hour covers, on the same daily grid
/// the other daily charts use. An empty series renders a placeholder note.
pub(crate) fn latency_bars(
    rows: &[HourLatency],
    from: Date,
    to: Date,
    theme: ChartTheme,
) -> String {
    let axis = match Granularity::from_range(from, to) {
        Granularity::Hourly => Axis::hourly(from, to),
        Granularity::Daily => Axis::daily(),
    };
    let bars = latency_series(rows, axis);
    if bars.is_empty() {
        return placeholder_chart(&chart_label(LATENCY_NAME, axis), theme);
    }
    render_latency_chart(&bars, axis, theme)
}

/// Buckets the token rows of a daily range into the slots of `axis`.
fn token_series(rows: &[DayUsage], axis: Axis) -> Vec<Bar> {
    let mut slots: BTreeMap<u32, Bar> = BTreeMap::new();
    for (day, prompt, completion, requests) in aggregate_daily(rows) {
        add_bucket(&mut slots, axis, &day, prompt, completion, requests);
    }
    slots.into_values().collect()
}

/// Buckets the hourly token rows into the slots of `axis`. The rows arrive
/// one per hour, key and model, so the sums of one bucket are the sums of
/// every group it holds.
fn hour_token_series(rows: &[HourUsage], axis: Axis) -> Vec<Bar> {
    let mut slots: BTreeMap<u32, Bar> = BTreeMap::new();
    for row in rows {
        add_bucket(
            &mut slots,
            axis,
            &row.hour,
            row.prompt_tokens,
            row.completion_tokens,
            row.requests,
        );
    }
    slots.into_values().collect()
}

/// Buckets the caller-priced costs into the slots of `axis`.
fn cost_series(costs: &[(String, i64)], axis: Axis) -> Vec<Bar> {
    let mut slots: BTreeMap<u32, Bar> = BTreeMap::new();
    for (bucket, micro_usd) in costs {
        add_bucket(&mut slots, axis, bucket, *micro_usd, 0, 0);
    }
    slots.into_values().collect()
}

/// Folds one bucket of values into its slot, summing over every row that
/// lands there. A label the axis does not carry contributes nothing.
fn add_bucket(
    slots: &mut BTreeMap<u32, Bar>,
    axis: Axis,
    label: &str,
    input: i64,
    output: i64,
    requests: i64,
) {
    let Some(slot) = axis.slot_of(label) else {
        return;
    };
    let bar = slots.entry(slot).or_insert(Bar {
        slot,
        input: 0,
        output: 0,
        requests: 0,
    });
    bar.input = bar.input.saturating_add(input);
    bar.output = bar.output.saturating_add(output);
    bar.requests = bar.requests.saturating_add(requests);
}

/// Folds the hourly latency rows into the slots of `axis`.
///
/// Every row contributes its mean weighted by the timed requests it covers,
/// so a folded day reads as the mean of every timed request of that day
/// rather than as the mean of its hours' means.
fn latency_series(rows: &[HourLatency], axis: Axis) -> Vec<LatencyBar> {
    let mut sums: BTreeMap<u32, (f64, i64)> = BTreeMap::new();
    for row in rows {
        let Some(slot) = axis.slot_of(&row.hour) else {
            continue;
        };
        let entry = sums.entry(slot).or_insert((0.0, 0));
        entry.0 += row.avg_duration_ms * row.requests as f64;
        entry.1 = entry.1.saturating_add(row.requests);
    }
    sums.into_iter()
        .map(|(slot, (weighted, requests))| LatencyBar {
            slot,
            avg_ms: mean_ms(weighted, requests),
            requests,
        })
        .collect()
}

/// The mean duration of a slot from the weighted sum of its rows. Every row
/// carries a duration, so a bucket holds at least one timed request; the
/// guard keeps a bucket without one at zero instead of a `NaN` coordinate.
fn mean_ms(weighted: f64, requests: i64) -> f64 {
    if requests > 0 {
        weighted / requests as f64
    } else {
        0.0
    }
}

/// Buckets the daily per-code rows into the slots of `axis`.
fn status_code_series(rows: &[DayStatusCodeUsage], axis: Axis) -> Vec<StatusCodeBar> {
    fold_status_codes(
        axis,
        rows.iter()
            .map(|row| (row.day.as_str(), row.status, row.requests)),
    )
}

/// Buckets the hourly per-code rows into the slots of `axis`.
fn hour_status_code_series(rows: &[HourStatusCodeUsage], axis: Axis) -> Vec<StatusCodeBar> {
    fold_status_codes(
        axis,
        rows.iter()
            .map(|row| (row.hour.as_str(), row.status, row.requests)),
    )
}

/// Folds `(bucket label, status, requests)` triples into one bar per slot,
/// the codes of a bar in canonical order.
fn fold_status_codes<'a>(
    axis: Axis,
    rows: impl Iterator<Item = (&'a str, Option<i64>, i64)>,
) -> Vec<StatusCodeBar> {
    let mut slots: BTreeMap<u32, Vec<(Option<i64>, i64)>> = BTreeMap::new();
    for (label, status, requests) in rows {
        let Some(slot) = axis.slot_of(label) else {
            continue;
        };
        let counts = slots.entry(slot).or_default();
        match counts.iter_mut().find(|(code, _)| *code == status) {
            Some((_, count)) => *count = count.saturating_add(requests),
            None => counts.push((status, requests)),
        }
    }
    slots
        .into_iter()
        .map(|(slot, mut counts)| {
            counts.sort_by_key(|(code, _)| code_order(*code));
            StatusCodeBar { slot, counts }
        })
        .collect()
}

/// Renders a chart from its already bucketed series: the placeholder note
/// when the series maps to no bar at all, the chart itself otherwise.
fn bar_chart(
    label: String,
    kind: ChartKind,
    axis: Axis,
    bars: Vec<Bar>,
    theme: ChartTheme,
) -> String {
    if bars.is_empty() {
        return placeholder_chart(&label, theme);
    }
    render_chart(&label, &bars, kind, axis, theme)
}

/// Renders the requests chart of a series: the placeholder note when no bar
/// has data, the stacked chart with its code legend otherwise.
fn status_code_chart(bars: Vec<StatusCodeBar>, axis: Axis, theme: ChartTheme) -> String {
    if bars.is_empty() {
        return placeholder_chart(&chart_label(REQUESTS_NAME, axis), theme);
    }
    let palette = code_palette(&bars, theme);
    render_status_code_chart(&bars, &palette, axis, theme)
}

/// Every code the series carries in canonical order, each with its color: the
/// palette cycles in that order, so a code keeps one color across the bars and
/// the legend, and two codes share a color only once the palette wraps.
fn code_palette(bars: &[StatusCodeBar], theme: ChartTheme) -> Vec<(Option<i64>, &'static str)> {
    let status = theme.palette().status;
    let mut codes: Vec<Option<i64>> = bars
        .iter()
        .flat_map(|bar| bar.counts.iter().map(|(code, _)| *code))
        .collect();
    codes.sort_by_key(|code| code_order(*code));
    codes.dedup();
    codes
        .into_iter()
        .enumerate()
        .map(|(index, code)| (code, status[index % status.len()]))
        .collect()
}

/// The color of one code, from the series palette, or the muted color of the
/// theme for a code the palette does not carry.
fn code_fill(
    palette: &[(Option<i64>, &'static str)],
    status: Option<i64>,
    muted: &'static str,
) -> &'static str {
    palette
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, fill)| *fill)
        .unwrap_or(muted)
}

/// One complete chart: frame, axes, bars, closing tag.
fn render_chart(
    label: &str,
    bars: &[Bar],
    kind: ChartKind,
    axis: Axis,
    theme: ChartTheme,
) -> String {
    let max = bars.iter().map(Bar::total).max().unwrap_or(0);
    let scale = if max > 0 { PLOT_H / max as f64 } else { 0.0 };
    let mut svg = String::new();
    push_open(&mut svg, label);
    push_axes(&mut svg, &kind.max_label(max), axis, theme);
    push_y_axis(
        &mut svg,
        max as f64,
        1.0,
        |value| kind.tick_label(value),
        theme,
    );
    for bar in bars {
        push_bar(&mut svg, bar, kind, axis, scale, theme);
    }
    push_close(&mut svg);
    svg
}

/// One complete requests chart: frame, axes, stacked bars, legend, closing
/// tag. The legend goes into the free top gutter, so it takes no room from
/// the plot and the chart keeps the height of the other charts.
fn render_status_code_chart(
    bars: &[StatusCodeBar],
    palette: &[(Option<i64>, &'static str)],
    axis: Axis,
    theme: ChartTheme,
) -> String {
    let max = bars.iter().map(StatusCodeBar::total).max().unwrap_or(0);
    let scale = if max > 0 { PLOT_H / max as f64 } else { 0.0 };
    let mut svg = String::new();
    push_open(&mut svg, &chart_label(REQUESTS_NAME, axis));
    push_axes(
        &mut svg,
        &format!("max {} requests", compact_count(max)),
        axis,
        theme,
    );
    push_y_axis(&mut svg, max as f64, 1.0, count_tick, theme);
    for bar in bars {
        push_status_code_bar(&mut svg, bar, palette, axis, scale, theme);
    }
    push_code_legend(&mut svg, palette, theme);
    push_close(&mut svg);
    svg
}

/// One complete latency chart: frame, axes, one bar per bucket, closing tag.
fn render_latency_chart(bars: &[LatencyBar], axis: Axis, theme: ChartTheme) -> String {
    let max = bars.iter().map(|bar| bar.avg_ms).fold(0.0_f64, f64::max);
    let scale = if max > 0.0 { PLOT_H / max } else { 0.0 };
    let mut svg = String::new();
    push_open(&mut svg, &chart_label(LATENCY_NAME, axis));
    push_axes(&mut svg, &format!("max {}", ms_text(max)), axis, theme);
    push_y_axis(&mut svg, max, 0.1, latency_tick, theme);
    for bar in bars {
        push_latency_bar(&mut svg, bar, axis, scale, theme);
    }
    push_close(&mut svg);
    svg
}

/// A valid chart carrying only the note that the series is empty.
fn placeholder_chart(label: &str, theme: ChartTheme) -> String {
    let mut svg = String::new();
    push_open(&mut svg, label);
    push_baseline(&mut svg, theme);
    let _ = write!(
        svg,
        "<text x=\"{:.2}\" y=\"{:.2}\" text-anchor=\"middle\" font-size=\"13\" \
         fill=\"{}\">{EMPTY_NOTE}</text>",
        PAD_LEFT + PLOT_W / 2.0,
        PAD_TOP + PLOT_H / 2.0,
        theme.palette().muted,
    );
    push_close(&mut svg);
    svg
}

/// Opens the root element. `width: 100%` plus the viewBox is what makes the
/// chart fill a card at any size without a fixed pixel width.
fn push_open(svg: &mut String, label: &str) {
    let _ = write!(
        svg,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {VIEW_W:.0} {VIEW_H:.0}\" \
         width=\"100%\" preserveAspectRatio=\"xMidYMid meet\" style=\"height:auto\" \
         role=\"img\" aria-label=\"{label}\">",
    );
}

/// Closes the root element.
fn push_close(svg: &mut String) {
    svg.push_str("</svg>");
}

/// Draws the top gridline, the baseline, the largest value and the sparse
/// bucket labels, in the rules and the muted color of `theme`.
fn push_axes(svg: &mut String, max_label: &str, axis: Axis, theme: ChartTheme) {
    let palette = theme.palette();
    let right = PAD_LEFT + PLOT_W;
    let _ = write!(
        svg,
        "<line x1=\"{PAD_LEFT:.2}\" y1=\"{PAD_TOP:.2}\" x2=\"{right:.2}\" y2=\"{PAD_TOP:.2}\" \
         stroke=\"{}\" stroke-width=\"1\"/>",
        palette.border,
    );
    push_baseline(svg, theme);
    let _ = write!(
        svg,
        "<text x=\"{PAD_LEFT:.2}\" y=\"{:.2}\" text-anchor=\"start\" font-size=\"11\" \
         fill=\"{}\">{max_label}</text>",
        PAD_TOP - 6.0,
        palette.muted,
    );
    for slot in 0..axis.slots() {
        if !axis.labelled(slot) {
            continue;
        }
        let _ = write!(
            svg,
            "<text x=\"{:.2}\" y=\"{:.2}\" text-anchor=\"middle\" font-size=\"11\" \
             fill=\"{}\">{}</text>",
            axis.slot_center(slot),
            BASE_Y + 18.0,
            palette.muted,
            axis.label(slot),
        );
    }
}

/// The x-axis rule bars grow from.
fn push_baseline(svg: &mut String, theme: ChartTheme) {
    let _ = write!(
        svg,
        "<line x1=\"{PAD_LEFT:.2}\" y1=\"{BASE_Y:.2}\" x2=\"{:.2}\" y2=\"{BASE_Y:.2}\" \
         stroke=\"{}\" stroke-width=\"1\"/>",
        PAD_LEFT + PLOT_W,
        theme.palette().border,
    );
}

/// One bar plus its hover tooltip, positioned and normalized by `scale`.
///
/// A slot whose total is zero keeps its title but draws no rectangle, so the
/// chart never invents a height that carries no usage.
fn push_bar(
    svg: &mut String,
    bar: &Bar,
    kind: ChartKind,
    axis: Axis,
    scale: f64,
    theme: ChartTheme,
) {
    let palette = theme.palette();
    let input_h = (bar.input as f64 * scale).min(PLOT_H);
    let output_h = (bar.output as f64 * scale).min(PLOT_H - input_h);
    let _ = write!(svg, "<g><title>{}</title>", bar_title(bar, kind, axis));
    if input_h > 0.0 {
        let fill = match kind {
            ChartKind::Stacked => palette.accent,
            ChartKind::Single => palette.cost,
        };
        push_rect(svg, axis, bar.slot, BASE_Y - input_h, input_h, fill);
    }
    if kind == ChartKind::Stacked && output_h > 0.0 {
        push_rect(
            svg,
            axis,
            bar.slot,
            BASE_Y - input_h - output_h,
            output_h,
            palette.muted,
        );
    }
    svg.push_str("</g>");
}

/// One latency bar plus its hover tooltip, positioned and normalized by
/// `scale`. A bucket whose mean is zero keeps its title but draws no
/// rectangle.
fn push_latency_bar(svg: &mut String, bar: &LatencyBar, axis: Axis, scale: f64, theme: ChartTheme) {
    let height = (bar.avg_ms * scale).min(PLOT_H);
    let _ = write!(svg, "<g><title>{}</title>", latency_title(bar, axis));
    if height > 0.0 {
        push_rect(
            svg,
            axis,
            bar.slot,
            BASE_Y - height,
            height,
            theme.palette().accent,
        );
    }
    svg.push_str("</g>");
}

/// One segment rectangle.
fn push_rect(svg: &mut String, axis: Axis, slot: u32, y: f64, height: f64, fill: &str) {
    let _ = write!(
        svg,
        "<rect x=\"{:.2}\" y=\"{y:.2}\" width=\"{:.2}\" height=\"{height:.2}\" \
         fill=\"{fill}\"/>",
        axis.bar_x(slot),
        axis.bar_w(),
    );
}

/// One requests bar plus its hover tooltip: the code segments stacked from
/// the baseline upwards in canonical code order, each normalized by `scale`.
///
/// A segment that carries no request draws no rectangle, so a bucket whose
/// codes are all zero keeps its title and stays an empty slot.
fn push_status_code_bar(
    svg: &mut String,
    bar: &StatusCodeBar,
    palette: &[(Option<i64>, &'static str)],
    axis: Axis,
    scale: f64,
    theme: ChartTheme,
) {
    let _ = write!(
        svg,
        "<g><title>{}</title>",
        status_code_bar_title(bar, axis)
    );
    let mut top = BASE_Y;
    for (code, count) in &bar.counts {
        let height = (*count as f64 * scale).min(top - PAD_TOP);
        if height <= 0.0 {
            continue;
        }
        top -= height;
        let fill = code_fill(palette, *code, theme.palette().muted);
        push_rect(svg, axis, bar.slot, top, height, fill);
    }
    svg.push_str("</g>");
}

/// The legend of the requests chart, drawn right-aligned in the free top
/// gutter as one swatch and one label per code, right to left.
///
/// It is part of the returned string because the page injects the chart as
/// markup, so the colors are explained where they appear. The entries are
/// laid out from the right edge with an estimated character width, which is
/// enough for a row of three digit codes and costs no text metrics engine.
fn push_code_legend(svg: &mut String, palette: &[(Option<i64>, &'static str)], theme: ChartTheme) {
    /// Side of a legend swatch.
    const SWATCH: f64 = 9.0;
    /// Gap between a swatch and its label.
    const SWATCH_GAP: f64 = 6.0;
    /// Gap between two legend entries.
    const ENTRY_GAP: f64 = 14.0;
    /// Approximate advance width of one character at the legend font size.
    const CHAR_W: f64 = 6.0;
    let baseline = PAD_TOP - 6.0;
    let mut right = PAD_LEFT + PLOT_W;
    let mut legend = String::from(LEGEND_OPEN);
    for (code, fill) in palette.iter().rev() {
        let label = legend_label(*code);
        let text_x = right - label.chars().count() as f64 * CHAR_W;
        let swatch_x = text_x - SWATCH_GAP - SWATCH;
        let _ = write!(
            legend,
            "<rect x=\"{swatch_x:.2}\" y=\"{:.2}\" width=\"{SWATCH:.2}\" height=\"{SWATCH:.2}\" \
             fill=\"{fill}\"/><text x=\"{text_x:.2}\" y=\"{baseline:.2}\" text-anchor=\"start\" \
             font-size=\"11\" fill=\"{}\">{label}</text>",
            baseline - 4.0 - SWATCH / 2.0,
            theme.palette().muted,
        );
        right = swatch_x - ENTRY_GAP;
    }
    legend.push_str("</g>");
    svg.push_str(&legend);
}

/// Tooltip text of one bar.
fn bar_title(bar: &Bar, kind: ChartKind, axis: Axis) -> String {
    match kind {
        ChartKind::Stacked => format!(
            "{}: {} tokens ({} in, {} out) in {} requests",
            axis.name(bar.slot),
            group_digits(bar.total()),
            group_digits(bar.input),
            group_digits(bar.output),
            group_digits(bar.requests),
        ),
        ChartKind::Single => format!("{}: {}", axis.name(bar.slot), format_usd(bar.input)),
    }
}

/// Tooltip text of one requests bar: the bucket's request count and how it
/// splits across the exact codes, so a hover answers which statuses the bar
/// holds.
fn status_code_bar_title(bar: &StatusCodeBar, axis: Axis) -> String {
    let mut split = String::new();
    for (index, (code, count)) in bar.counts.iter().enumerate() {
        if index > 0 {
            split.push_str(", ");
        }
        let _ = write!(split, "{} {}", group_digits(*count), code_label(*code));
    }
    format!(
        "{}: {} requests ({split})",
        axis.name(bar.slot),
        group_digits(bar.total())
    )
}

/// Tooltip text of one latency bar: the bucket's mean duration and the timed
/// requests it covers.
fn latency_title(bar: &LatencyBar, axis: Axis) -> String {
    format!(
        "{}: {} ({} requests)",
        axis.name(bar.slot),
        ms_text(bar.avg_ms),
        group_digits(bar.requests),
    )
}

/// Decimal digits with a comma every three and a leading minus when
/// negative.
fn group_digits(value: i64) -> String {
    let digits = value.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    if value < 0 {
        format!("-{out}")
    } else {
        out
    }
}

/// A token count for an axis label: exact while it stays short, otherwise
/// one decimal and a scale suffix. The formatter is what keeps the label
/// inside its gutter at any magnitude.
fn compact_count(value: i64) -> String {
    if value.abs() < 1_000 {
        value.to_string()
    } else {
        compact_units(value as f64)
    }
}

/// Micro-USD for an axis label: cents while the amount stays below a
/// thousand dollars, otherwise a shortened dollar figure.
fn compact_usd(micro_usd: i64) -> String {
    let dollars = micro_usd as f64 / 1_000_000.0;
    if dollars.abs() < 1_000.0 {
        format!("${dollars:.2}")
    } else {
        format!("${}", compact_units(dollars))
    }
}

/// A number shortened to one decimal and a scale suffix, `1234` as `1.2K`
/// and `9_223_372_036_854_775_807` as `9.2E`.
fn compact_units(value: f64) -> String {
    const SCALES: [(f64, &str); 6] = [
        (1e18, "E"),
        (1e15, "P"),
        (1e12, "T"),
        (1e9, "B"),
        (1e6, "M"),
        (1e3, "K"),
    ];
    match SCALES.iter().find(|(scale, _)| value.abs() >= *scale) {
        Some((scale, suffix)) => format!("{:.1}{suffix}", value / scale),
        None => format!("{value:.0}"),
    }
}

/// Micro-USD as dollars, down to the six decimals the integer unit carries
/// and never less than two, so a tooltip reads `$12.50` rather than
/// `$12.500000`.
fn format_usd(micro_usd: i64) -> String {
    let whole = micro_usd.div_euclid(1_000_000);
    let mut fraction = format!("{:06}", micro_usd.rem_euclid(1_000_000));
    while fraction.len() > 2 && fraction.ends_with('0') {
        fraction.pop();
    }
    format!("${whole}.{fraction}")
}

/// Milliseconds for a latency tooltip and its axis label: one decimal, which
/// keeps the fractional mean of a small bucket readable.
fn ms_text(value: f64) -> String {
    format!("{value:.1} ms")
}

/// A whole-number tick label of a token or request axis: the same compact
/// read the max label above the plot uses, rounded because the series itself
/// is an `i64` total and a fraction of a token can never appear in a bar.
fn count_tick(value: f64) -> String {
    compact_count(value.round() as i64)
}

/// A latency tick label in milliseconds: one decimal below ten so a small
/// mean stays readable there, whole milliseconds up to ten thousand, and a
/// shortened figure above that so a pathological duration never widens the
/// gutter past [`PAD_LEFT`].
fn latency_tick(value: f64) -> String {
    if value < 10.0 {
        format!("{value:.1} ms")
    } else if value < 10_000.0 {
        format!("{value:.0} ms")
    } else {
        format!("{} ms", compact_units(value))
    }
}

/// The step between two y-axis gridlines of a chart whose largest value is
/// `max`: the smallest number of the 1-2-5 ladder that keeps at most four
/// steps across the plot, so the axis carries two to four lines and every
/// label stays a human number rather than something like 3.7 or 11.2. A
/// non-positive or non-finite max has no step at all, which is how an empty
/// chart ends up with no gridlines.
fn nice_step(max: f64) -> f64 {
    if !max.is_finite() || max <= 0.0 {
        return 0.0;
    }
    let target = max / 4.0;
    let decade = 10f64.powf(target.log10().floor());
    let fraction = target / decade;
    let ladder = if fraction <= 1.0 {
        1.0
    } else if fraction <= 2.0 {
        2.0
    } else if fraction <= 5.0 {
        5.0
    } else {
        10.0
    };
    ladder * decade
}

/// The values the y-axis of a chart carries as gridlines: every multiple of
/// [`nice_step`] strictly below `max`, ascending, so the top of the plot
/// stays the max line the chart already draws instead of gaining a duplicate
/// tick on top of it. `min_step` is the finest step the series allows: a
/// whole-number axis passes 1.0 because half a token or half a request is not
/// a value a bar can show, while a latency in milliseconds keeps its
/// fractions.
fn tick_values(max: f64, min_step: f64) -> Vec<f64> {
    let step = nice_step(max).max(min_step);
    if !step.is_finite() || step <= 0.0 {
        return Vec::new();
    }
    let mut values = Vec::new();
    let mut multiple = 1.0;
    while multiple * step < max {
        values.push(multiple * step);
        multiple += 1.0;
    }
    values
}

/// Draws the y-axis of one chart: a gridline across the plot and a
/// right-anchored value label in the left gutter for every tick, in the rule
/// and the muted color of `theme`. The label is shifted down by half of its
/// font size so it sits centred on the line it names.
fn push_y_axis(
    svg: &mut String,
    max: f64,
    min_step: f64,
    tick_label: impl Fn(f64) -> String,
    theme: ChartTheme,
) {
    let palette = theme.palette();
    let right = PAD_LEFT + PLOT_W;
    for value in tick_values(max, min_step) {
        let y = BASE_Y - PLOT_H * value / max;
        let _ = write!(
            svg,
            "<line x1=\"{PAD_LEFT:.2}\" y1=\"{y:.2}\" x2=\"{right:.2}\" y2=\"{y:.2}\" \
             stroke=\"{}\" stroke-width=\"1\"/><text x=\"{:.2}\" y=\"{:.2}\" \
             text-anchor=\"end\" font-size=\"10\" fill=\"{}\">{}</text>",
            palette.border,
            PAD_LEFT - 6.0,
            y + 3.0,
            palette.muted,
            tick_label(value),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row as the SQL aggregate would hand it over. The cache dimensions
    /// take no part in a daily token total, so they stay at zero here.
    fn row(
        day: &str,
        model: Option<&str>,
        prompt: i64,
        completion: i64,
        requests: i64,
    ) -> DayUsage {
        DayUsage {
            day: day.to_owned(),
            key_id: 1,
            model: model.map(str::to_owned),
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_input_tokens: 0,
            cached_output_tokens: 0,
            cache_write_input_tokens: 0,
            requests,
        }
    }

    /// Number of occurrences of a needle.
    fn count(haystack: &str, needle: &str) -> usize {
        haystack.matches(needle).count()
    }

    /// Number of bar rectangles painted in a fill; text elements end with a
    /// `</text>` tag instead, so the pattern isolates the bars.
    fn rects(svg: &str, fill: &str) -> usize {
        count(svg, &format!("fill=\"{fill}\"/>"))
    }

    /// The stroke color of every `<line>` of a chart, in document order, so a
    /// test can tell the axis rules and the gridlines apart from the rest of
    /// the markup without counting a fixed number of them.
    fn line_strokes(svg: &str) -> Vec<&str> {
        svg.split("<line ")
            .skip(1)
            .filter_map(|element| element.split("stroke=\"").nth(1))
            .filter_map(|rest| rest.split('"').next())
            .collect()
    }

    /// The vertical position and the text of every right-anchored label of a
    /// chart, which is what a y-axis tick is, in document order. The position
    /// carries the half font size the label is shifted down by, so it is
    /// compared against the gutters rather than against a line coordinate.
    fn y_ticks(svg: &str) -> Vec<(f64, String)> {
        svg.split("<text ")
            .skip(1)
            .filter_map(|element| {
                let (attributes, rest) = element.split_once('>')?;
                if !attributes.contains("text-anchor=\"end\"") {
                    return None;
                }
                let y: f64 = attributes
                    .split("y=\"")
                    .nth(1)?
                    .split('"')
                    .next()?
                    .parse()
                    .ok()?;
                Some((y, rest.split('<').next()?.to_owned()))
            })
            .collect()
    }

    /// One per-code row as the SQL aggregate would hand it over. The class
    /// travels with the row for the page, while the chart paints by code.
    fn code_row(day: &str, status: Option<i64>, requests: i64) -> DayStatusCodeUsage {
        DayStatusCodeUsage {
            day: day.to_owned(),
            status,
            status_class: StatusBucket::Other,
            requests,
        }
    }

    /// The chart without its legend, which is emitted last and is the only
    /// other place the palette colors appear.
    fn bars_only(svg: &str) -> (&str, &str) {
        svg.split_once(LEGEND_OPEN)
            .unwrap_or_else(|| panic!("the status chart carries a legend: {svg}"))
    }

    #[test]
    fn empty_status_code_rows_render_a_placeholder_chart() {
        let svg = daily_status_code_bars(&[], ChartTheme::Light);
        assert!(svg.starts_with("<svg "));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains(EMPTY_NOTE));
        assert_eq!(count(&svg, "<rect"), 0);
        assert_eq!(count(&svg, "<title>"), 0);
        assert_eq!(count(&svg, LEGEND_OPEN), 0);
    }

    #[test]
    fn unparsable_status_code_days_are_skipped() {
        let rows = [
            code_row("bogus", Some(200), 1),
            code_row("2026-11-00", Some(200), 1),
            code_row("2026-11-32", Some(200), 1),
        ];
        let svg = daily_status_code_bars(&rows, ChartTheme::Light);
        assert!(svg.contains(EMPTY_NOTE));
        assert_eq!(count(&svg, "<rect"), 0);
    }

    #[test]
    fn code_segments_stack_in_canonical_order_with_one_color_each() {
        let rows = [
            code_row("2026-11-04", None, 1),
            code_row("2026-11-04", Some(500), 2),
            code_row("2026-11-04", Some(200), 4),
            code_row("2026-11-04", Some(404), 3),
        ];
        let svg = daily_status_code_bars(&rows, ChartTheme::Light);
        let (bars, legend) = bars_only(&svg);
        assert_eq!(count(bars, "<rect"), 4, "one segment per code: {bars}");
        // The bottom segment is drawn first, so the order in the markup is
        // the stacking order: the codes ascending, the requests without a
        // status last. Colors follow that same order, so the palette slots
        // 200, 404, 500 and n/a are the four segments in sequence.
        let order: Vec<usize> = STATUS_CODE_PALETTE[..4]
            .iter()
            .map(|fill| {
                bars.find(&format!("fill=\"{fill}\"/>"))
                    .unwrap_or_else(|| panic!("{fill} carries a segment: {bars}"))
            })
            .collect();
        assert!(
            order.windows(2).all(|pair| pair[0] < pair[1]),
            "the segments stack in code order: {bars}"
        );
        assert!(svg.contains("Day 4: 10 requests (4 200, 3 404, 2 500, 1 n/a)"));
        assert!(svg.contains(">max 10 requests</text>"));
        // The legend lists the same four entries once more, as swatches.
        assert_eq!(count(legend, "<rect"), 4, "one swatch per code: {legend}");
    }

    #[test]
    fn the_code_legend_names_every_code_and_the_bucket_of_the_unlabelled_requests() {
        let rows = [
            code_row("2026-11-01", Some(200), 1),
            code_row("2026-11-01", Some(503), 1),
            code_row("2026-11-01", None, 1),
        ];
        let svg = daily_status_code_bars(&rows, ChartTheme::Light);
        let (bars, legend) = bars_only(&svg);
        for label in ["200", "503", "n/a (other)"] {
            assert!(
                legend.contains(&format!(">{label}</text>")),
                "{label} is missing from the legend: {legend}"
            );
        }
        // Every code keeps one color across the segments and the legend, and
        // no two codes of this series share one.
        let expected = [
            (Some(200), STATUS_CODE_PALETTE[0]),
            (Some(503), STATUS_CODE_PALETTE[1]),
            (None, STATUS_CODE_PALETTE[2]),
        ];
        for (code, fill) in expected {
            assert_eq!(rects(bars, fill), 1, "{code:?} carries one segment: {bars}");
            assert_eq!(
                rects(legend, fill),
                1,
                "{code:?} carries the same color in the legend: {legend}"
            );
        }
        // The legend is laid out from the right edge of the plot with an
        // estimated label width, so every coordinate it computes has to stay
        // inside the viewport it shares with the axis label.
        for x in legend
            .split("x=\"")
            .skip(1)
            .filter_map(|rest| rest.split('"').next())
        {
            let value: f64 = x.parse().expect("an x coordinate is a number");
            assert!(
                (PAD_LEFT..=PAD_LEFT + PLOT_W).contains(&value),
                "{value} leaves the plot: {legend}"
            );
        }
    }

    #[test]
    fn the_palette_cycles_once_it_runs_out_of_colors() {
        let codes = [
            Some(100),
            Some(200),
            Some(201),
            Some(204),
            Some(301),
            Some(400),
            Some(404),
        ];
        let rows: Vec<DayStatusCodeUsage> = codes
            .iter()
            .map(|code| code_row("2026-11-01", *code, 1))
            .collect();
        let svg = daily_status_code_bars(&rows, ChartTheme::Light);
        let (bars, legend) = bars_only(&svg);
        assert_eq!(count(bars, "<rect"), codes.len());
        assert_eq!(
            rects(bars, STATUS_CODE_PALETTE[0]),
            2,
            "the seventh code wraps back to the first color: {bars}"
        );
        for code in codes {
            let label = code.expect("every code of the sample is real");
            assert!(
                legend.contains(&format!(">{label}</text>")),
                "{label} is missing from the legend: {legend}"
            );
        }
    }

    #[test]
    fn a_day_with_one_code_draws_one_segment_and_keeps_its_title() {
        let svg =
            daily_status_code_bars(&[code_row("2026-11-07", Some(503), 2)], ChartTheme::Light);
        let (bars, _) = bars_only(&svg);
        assert_eq!(count(bars, "<rect"), 1);
        assert_eq!(rects(bars, STATUS_CODE_PALETTE[0]), 1);
        assert_eq!(
            rects(bars, DANGER_FILL),
            0,
            "a lone code takes the first palette color, not its class color: {bars}"
        );
        assert!(svg.contains("Day 7: 2 requests (2 503)"));
        assert!(!svg.contains(EMPTY_NOTE));
    }

    #[test]
    fn a_status_code_day_with_no_requests_keeps_a_title_without_a_segment() {
        let svg =
            daily_status_code_bars(&[code_row("2026-11-07", Some(200), 0)], ChartTheme::Light);
        let (bars, _) = bars_only(&svg);
        assert_eq!(count(bars, "<rect"), 0);
        assert_eq!(count(bars, "<title>"), 1);
        assert!(svg.contains("Day 7: 0 requests (0 200)"));
        assert!(svg.contains(">max 0 requests</text>"));
        assert!(!svg.contains(EMPTY_NOTE));
    }

    #[test]
    fn every_day_of_the_month_gets_a_requests_bar() {
        let rows: Vec<DayStatusCodeUsage> = (1..=DAY_SLOTS)
            .map(|day| code_row(&format!("2026-11-{day:02}"), Some(200), 1))
            .collect();
        let svg = daily_status_code_bars(&rows, ChartTheme::Light);
        let (bars, _) = bars_only(&svg);
        assert_eq!(count(bars, "<g><title>"), DAY_SLOTS as usize);
        assert_eq!(count(bars, "<rect"), DAY_SLOTS as usize);
        // The day labels keep the sparse layout of the other charts; the
        // remaining left-anchored texts are the largest-value label and the
        // one legend entry of a single-code series.
        assert_eq!(count(&svg, "text-anchor=\"middle\""), 7);
        assert_eq!(count(&svg, "text-anchor=\"start\""), 2);
    }

    #[test]
    fn status_code_output_is_deterministic_and_order_independent() {
        /// The same rows in a fixed order, rebuilt on every call.
        fn sample() -> Vec<DayStatusCodeUsage> {
            vec![
                code_row("2026-11-02", None, 1),
                code_row("2026-11-01", Some(500), 2),
                code_row("2026-11-01", Some(200), 3),
            ]
        }
        let mut reversed = sample();
        reversed.reverse();
        assert_eq!(
            daily_status_code_bars(&sample(), ChartTheme::Light),
            daily_status_code_bars(&sample(), ChartTheme::Light)
        );
        assert_eq!(
            daily_status_code_bars(&sample(), ChartTheme::Light),
            daily_status_code_bars(&reversed, ChartTheme::Light),
            "the colors follow the canonical code order, not the row order"
        );
    }

    #[test]
    fn status_code_values_saturate_and_stay_inside_the_plot() {
        let rows = [
            code_row("2026-11-01", Some(200), i64::MAX),
            code_row("2026-11-01", Some(500), i64::MAX),
        ];
        let svg = daily_status_code_bars(&rows, ChartTheme::Light);
        let (bars, _) = bars_only(&svg);
        // Saturation drops the segment that no longer fits, but no
        // coordinate ever leaves the plot.
        assert_eq!(count(bars, "<rect"), 1);
        assert!(!svg.contains("NaN"));
        assert!(!svg.contains("y=\"-"));
        assert!(svg.contains("y=\"20.00\""));
        assert!(svg.contains("9,223,372,036,854,775,807"));
        assert!(svg.contains(">max 9.2E requests</text>"));
    }

    #[test]
    fn empty_rows_render_a_placeholder_chart() {
        let svg = daily_token_bars(&[], ChartTheme::Light);
        assert!(svg.starts_with("<svg "));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains(EMPTY_NOTE));
        assert_eq!(count(&svg, "<rect"), 0);
        assert_eq!(count(&svg, "<title>"), 0);

        let cost = daily_cost_bars(&[], ChartTheme::Light);
        assert!(cost.contains(EMPTY_NOTE));
        assert_eq!(count(&cost, "<rect"), 0);
    }

    #[test]
    fn unparsable_days_are_skipped() {
        let rows = [
            row("bogus", None, 10, 10, 1),
            row("2026-11-00", None, 10, 10, 1),
            row("2026-11-32", None, 10, 10, 1),
        ];
        let svg = daily_token_bars(&rows, ChartTheme::Light);
        assert!(svg.contains(EMPTY_NOTE));
        assert_eq!(count(&svg, "<rect"), 0);
    }

    #[test]
    fn single_day_fills_the_plot() {
        let rows = [row("2026-11-03", Some("gpt-4o"), 100, 100, 2)];
        let svg = daily_token_bars(&rows, ChartTheme::Light);
        // Two segments, both scaled by the same factor, the taller one
        // reaching the top gridline.
        assert_eq!(count(&svg, "<rect"), 2);
        assert!(svg.contains("y=\"20.00\""), "{svg}");
        assert!(svg.contains("Day 3: 200 tokens (100 in, 100 out) in 2 requests"));
        assert_eq!(rects(&svg, ACCENT_FILL), 1);
        assert_eq!(rects(&svg, MUTED_FILL), 1);
    }

    #[test]
    fn every_day_of_the_month_gets_a_bar() {
        let rows: Vec<DayUsage> = (1..=DAY_SLOTS)
            .map(|day| row(&format!("2026-11-{day:02}"), None, 1, 0, 1))
            .collect();
        let svg = daily_token_bars(&rows, ChartTheme::Light);
        assert_eq!(count(&svg, "<g><title>"), DAY_SLOTS as usize);
        // One input segment per day; the output value is zero.
        assert_eq!(count(&svg, "<rect"), DAY_SLOTS as usize);
        assert_eq!(rects(&svg, ACCENT_FILL), DAY_SLOTS as usize);
        assert_eq!(rects(&svg, MUTED_FILL), 0);
    }

    #[test]
    fn day_labels_are_sparse() {
        let rows = [row("2026-11-01", None, 1, 1, 1)];
        let svg = daily_token_bars(&rows, ChartTheme::Light);
        // Days 1, 5, 10, 15, 20, 25 and 30 carry a day label; the
        // largest-value label above the plot is the only left-anchored one,
        // and the y-axis carries one tick at one of the two tokens the day
        // holds, the only right-anchored label here.
        assert_eq!(count(&svg, "text-anchor=\"middle\""), 7);
        assert_eq!(count(&svg, "text-anchor=\"start\""), 1);
        assert_eq!(count(&svg, "text-anchor=\"end\""), 1);
    }

    #[test]
    fn zero_values_draw_a_title_without_a_bar() {
        // Both segments are drawn while the values are non-zero.
        let rows = [row("2026-11-07", None, 1, 2, 3)];
        assert_eq!(
            count(&daily_token_bars(&rows, ChartTheme::Light), "<rect"),
            2
        );
        let zero = [row("2026-11-07", None, 0, 0, 0)];
        let svg = daily_token_bars(&zero, ChartTheme::Light);
        assert_eq!(count(&svg, "<title>"), 1);
        assert_eq!(count(&svg, "<rect"), 0);
        assert!(svg.contains("Day 7: 0 tokens (0 in, 0 out) in 0 requests"));
        // An all-zero series still renders a chart, not a placeholder.
        assert!(!svg.contains(EMPTY_NOTE));
        assert!(svg.contains(">max 0 tokens</text>"));
    }

    #[test]
    fn large_values_saturate_and_stay_inside_the_plot() {
        let rows = [
            row("2026-11-01", None, i64::MAX, i64::MAX, i64::MAX),
            row("2026-11-02", None, i64::MAX, 0, 0),
        ];
        let svg = daily_token_bars(&rows, ChartTheme::Light);
        // Saturation loses the second segment of the first day, but no
        // coordinate ever leaves the plot.
        assert_eq!(count(&svg, "<rect"), 2);
        assert!(!svg.contains("NaN"));
        assert!(!svg.contains("inf"));
        assert!(!svg.contains("y=\"-"));
        assert!(svg.contains("9,223,372,036,854,775,807"));
        assert!(!svg.contains("18,446,744,073,709,551,614"));
        // The axis label collapses the magnitude instead of widening.
        assert!(svg.contains(">max 9.2E tokens</text>"));
        assert!(svg.contains("y=\"20.00\""));
    }

    #[test]
    fn null_model_rows_join_the_daily_total() {
        let rows = [
            row("2026-11-05", None, 10_000, 5, 1),
            row("2026-11-05", Some("gpt-4o"), 20_000, 5, 1),
        ];
        let svg = daily_token_bars(&rows, ChartTheme::Light);
        assert_eq!(count(&svg, "<rect"), 2);
        assert!(svg.contains("Day 5: 30,010 tokens (30,000 in, 10 out) in 2 requests"));
    }

    #[test]
    fn output_is_deterministic_and_order_independent() {
        /// The same rows in a fixed order, rebuilt on every call so the
        /// assertion never needs a clone.
        fn sample() -> Vec<DayUsage> {
            vec![
                row("2026-11-02", None, 10, 20, 1),
                row("2026-11-01", Some("gpt-4o"), 30, 40, 2),
                row("2026-11-01", None, 1, 2, 3),
            ]
        }
        let mut reversed = sample();
        reversed.reverse();
        assert_eq!(
            daily_token_bars(&sample(), ChartTheme::Light),
            daily_token_bars(&sample(), ChartTheme::Light)
        );
        assert_eq!(
            daily_token_bars(&sample(), ChartTheme::Light),
            daily_token_bars(&reversed, ChartTheme::Light)
        );
    }

    #[test]
    fn aggregate_daily_sums_per_day_in_day_order() {
        let rows = [
            row("2026-11-02", Some("gpt-4o"), 10, 20, 1),
            row("2026-11-01", None, 1, 2, 3),
            row("2026-11-01", Some("gpt-4o"), 4, 5, 6),
        ];
        assert_eq!(
            aggregate_daily(&rows),
            vec![
                ("2026-11-01".to_owned(), 5, 7, 9),
                ("2026-11-02".to_owned(), 10, 20, 1),
            ],
        );
        assert!(aggregate_daily(&[]).is_empty());
    }

    #[test]
    fn cost_bars_render_one_bar_per_day() {
        let costs = vec![
            ("2026-11-01".to_owned(), 123_456),
            ("2026-11-01".to_owned(), 1),
            ("2026-11-03".to_owned(), 60_000),
            ("bogus".to_owned(), 1),
        ];
        let svg = daily_cost_bars(&costs, ChartTheme::Light);
        assert_eq!(count(&svg, "<rect"), 2);
        assert_eq!(rects(&svg, COST_FILL), 2);
        assert_eq!(rects(&svg, ACCENT_FILL), 0);
        assert!(svg.contains("Day 1: $0.123457"));
        assert!(svg.contains("Day 3: $0.06"));
        assert!(svg.contains(">max $0.12</text>"));
        assert!(!svg.contains("inf"));
    }

    #[test]
    fn cost_values_saturate_and_stay_inside_the_plot() {
        let costs = vec![
            ("2026-11-01".to_owned(), i64::MAX),
            ("2026-11-02".to_owned(), i64::MAX),
        ];
        let svg = daily_cost_bars(&costs, ChartTheme::Light);
        assert_eq!(count(&svg, "<rect"), 2);
        assert!(!svg.contains("NaN"));
        assert!(!svg.contains("y=\"-"));
        assert!(svg.contains("y=\"20.00\""));
        assert_eq!(
            daily_cost_bars(&costs, ChartTheme::Light),
            daily_cost_bars(&costs, ChartTheme::Light)
        );
    }

    #[test]
    fn numbers_are_grouped_and_priced_for_reading() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1_000), "1,000");
        assert_eq!(group_digits(-1_234_567), "-1,234,567");
        assert_eq!(group_digits(i64::MIN), "-9,223,372,036,854,775,808");
        assert_eq!(format_usd(0), "$0.00");
        assert_eq!(format_usd(1_000_000), "$1.00");
        assert_eq!(format_usd(1_500_000), "$1.50");
        assert_eq!(format_usd(2_376_000_000), "$2376.00");
        assert_eq!(format_usd(1), "$0.000001");
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_000), "1.0K");
        assert_eq!(compact_count(1_234_567), "1.2M");
        assert_eq!(compact_count(i64::MAX), "9.2E");
        assert_eq!(compact_count(-2_500_000), "-2.5M");
        assert_eq!(compact_usd(123_457), "$0.12");
        assert_eq!(compact_usd(999_999_999), "$1000.00");
        assert_eq!(compact_usd(1_500_000_000), "$1.5K");
        assert_eq!(compact_usd(i64::MAX), "$9.2T");
    }

    /// A request carrying one `Cookie` header value.
    fn cookies(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(axum::http::header::COOKIE, value.parse().unwrap());
        headers
    }

    #[test]
    fn the_dark_theme_keeps_every_chart_in_the_dark_palette() {
        let rows = [row("2026-11-01", None, 100, 50, 1)];
        let svg = daily_token_bars(&rows, ChartTheme::Dark);
        assert_eq!(rects(&svg, DARK_ACCENT_FILL), 1, "{svg}");
        assert_eq!(rects(&svg, DARK_MUTED_FILL), 1, "{svg}");
        assert_eq!(
            rects(&svg, ACCENT_FILL),
            0,
            "no light color in a dark chart"
        );
        assert_eq!(rects(&svg, MUTED_FILL), 0, "no light color in a dark chart");
        // The top rule, the baseline and every gridline of the y-axis come
        // from the same theme border, and no line keeps the light one.
        let strokes = line_strokes(&svg);
        assert!(
            strokes.len() > 2 && strokes.iter().all(|stroke| *stroke == DARK_BORDER_STROKE),
            "every axis rule and gridline follows the theme: {svg}"
        );
        assert!(
            !strokes.contains(&BORDER_STROKE),
            "no light rule in a dark chart: {svg}"
        );
        assert!(
            svg.contains(&format!(
                "text-anchor=\"end\" font-size=\"10\" fill=\"{DARK_MUTED_FILL}\""
            )),
            "the tick labels keep the dark muted color: {svg}"
        );

        let costs = [("2026-11-01".to_owned(), 1_000_000)];
        let svg = daily_cost_bars(&costs, ChartTheme::Dark);
        assert_eq!(rects(&svg, DARK_COST_FILL), 1, "{svg}");
        assert_eq!(rects(&svg, COST_FILL), 0, "no light color in a dark chart");

        let rows = [code_row("2026-11-01", Some(500), 2)];
        let svg = daily_status_code_bars(&rows, ChartTheme::Dark);
        let (bars, legend) = bars_only(&svg);
        assert_eq!(rects(bars, DARK_STATUS_CODE_PALETTE[0]), 1, "{bars}");
        assert_eq!(rects(bars, STATUS_CODE_PALETTE[0]), 0, "{bars}");
        assert_eq!(
            rects(legend, DARK_STATUS_CODE_PALETTE[0]),
            1,
            "the legend swatch keeps the dark color: {legend}"
        );
        assert!(
            count(
                &svg,
                &format!("font-size=\"11\" fill=\"{DARK_MUTED_FILL}\"")
            ) >= 8,
            "the axis labels and the legend entry keep the dark muted color: {svg}"
        );
    }

    #[test]
    fn an_empty_series_notes_it_in_the_color_of_the_theme() {
        let svg = daily_token_bars(&[], ChartTheme::Dark);
        assert!(svg.contains(EMPTY_NOTE), "{svg}");
        assert!(
            svg.contains(&format!("fill=\"{DARK_MUTED_FILL}\"")),
            "{svg}"
        );
        assert!(
            !svg.contains(MUTED_FILL),
            "the light note color is gone: {svg}"
        );
        assert!(
            svg.contains(&format!("stroke=\"{DARK_BORDER_STROKE}\"")),
            "the placeholder baseline follows the theme: {svg}"
        );
    }

    #[test]
    fn the_theme_cookie_of_a_page_selects_the_palette() {
        assert_eq!(theme_from_headers(&HeaderMap::new()), ChartTheme::Light);
        assert_eq!(theme_from_headers(&cookies("theme=dark")), ChartTheme::Dark);
        assert_eq!(
            theme_from_headers(&cookies("other=1; theme=dark ; more=2")),
            ChartTheme::Dark,
            "the cookie is found among others and its value is trimmed"
        );
        for value in [
            "theme=light",
            "theme=Dark",
            "theme=",
            "theme=darkish",
            "hydra_admin_session=abc",
        ] {
            assert_eq!(
                theme_from_headers(&cookies(value)),
                ChartTheme::Light,
                "{value} names no dark theme"
            );
        }
    }

    /// A UTC day of the calendar.
    fn day(year: i32, month: u8, day: u8) -> Date {
        Date::from_calendar_date(year, time::Month::try_from(month).expect("a month"), day)
            .expect("a day of the calendar")
    }

    /// One hourly usage row as the SQL aggregate hands it over.
    fn hour_row(hour: &str, prompt: i64, completion: i64, requests: i64) -> HourUsage {
        HourUsage {
            hour: hour.to_owned(),
            key_id: 1,
            model: None,
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_input_tokens: 0,
            cached_output_tokens: 0,
            cache_write_input_tokens: 0,
            requests,
        }
    }

    /// One hourly per-code row as the SQL aggregate hands it over.
    fn hour_code_row(hour: &str, status: Option<i64>, requests: i64) -> HourStatusCodeUsage {
        HourStatusCodeUsage {
            hour: hour.to_owned(),
            status,
            status_class: StatusBucket::Other,
            requests,
        }
    }

    /// One hourly latency row as the SQL aggregate hands it over.
    fn latency_row(hour: &str, avg_ms: f64, requests: i64) -> HourLatency {
        HourLatency {
            hour: hour.to_owned(),
            avg_duration_ms: avg_ms,
            requests,
        }
    }

    #[test]
    fn a_one_and_a_two_day_range_are_hourly_and_longer_ones_daily() {
        let start = day(2026, 11, 16);
        assert_eq!(
            Granularity::from_range(start, start),
            Granularity::Hourly,
            "a single day is drawn hour by hour"
        );
        assert_eq!(
            Granularity::from_range(start, day(2026, 11, 17)),
            Granularity::Hourly,
            "two days are the widest hourly range"
        );
        assert_eq!(
            Granularity::from_range(start, day(2026, 11, 18)),
            Granularity::Daily,
            "the third day leaves the hourly range"
        );
        assert_eq!(
            Granularity::from_range(start, day(2026, 12, 16)),
            Granularity::Daily
        );
        assert_eq!(
            Granularity::from_range(day(2026, 11, 30), day(2026, 12, 1)),
            Granularity::Hourly,
            "two days across a month boundary stay hourly"
        );
        assert_eq!(
            Granularity::from_range(day(2026, 12, 1), day(2026, 11, 30)),
            Granularity::Daily,
            "a range that names no day falls back to the daily axis"
        );
    }

    #[test]
    fn a_one_day_range_labels_every_third_hour_by_the_clock() {
        let rows = [hour_row("2026-11-08 14", 100, 50, 2)];
        let svg = hourly_token_bars(&rows, day(2026, 11, 8), day(2026, 11, 8), ChartTheme::Light);
        assert_eq!(count(&svg, "text-anchor=\"middle\""), 8, "{svg}");
        assert!(svg.contains(">15:00</text>"), "{svg}");
        assert!(
            !svg.contains(">14:00</text>"),
            "an unlabelled hour stays bare: {svg}"
        );
        assert!(
            !svg.contains("Nov"),
            "a single day needs no day label: {svg}"
        );
        assert!(svg.contains("aria-label=\"Tokens per hour\""), "{svg}");
        assert!(
            svg.contains("<title>14:00: 150 tokens (100 in, 50 out) in 2 requests</title>"),
            "{svg}"
        );
        assert_eq!(count(&svg, "<rect"), 2);
    }

    #[test]
    fn a_two_day_range_labels_every_sixth_hour_with_its_day() {
        let rows = [
            hour_row("2026-11-16 14", 10, 0, 1),
            hour_row("2026-11-17 06", 20, 0, 1),
        ];
        let svg = hourly_token_bars(
            &rows,
            day(2026, 11, 16),
            day(2026, 11, 17),
            ChartTheme::Light,
        );
        assert_eq!(count(&svg, "text-anchor=\"middle\""), 8, "{svg}");
        for label in ["Nov 16 12:00", "Nov 17 06:00"] {
            assert!(
                svg.contains(&format!(">{label}</text>")),
                "{label} is missing from the axis: {svg}"
            );
        }
        // The bucket that carries no label of its own still names itself in
        // the tooltip, day included, because `14:00` alone is ambiguous.
        assert!(
            svg.contains("<title>Nov 16 14:00: 10 tokens (10 in, 0 out) in 1 requests</title>"),
            "{svg}"
        );
        assert!(svg.contains(">max 20 tokens</text>"), "{svg}");
    }

    #[test]
    fn an_hourly_bucket_outside_the_range_draws_nothing() {
        let rows = [
            hour_row("2026-11-16 23", 10, 0, 1),
            hour_row("2026-11-18 00", 999, 0, 1),
        ];
        let svg = hourly_token_bars(
            &rows,
            day(2026, 11, 16),
            day(2026, 11, 17),
            ChartTheme::Light,
        );
        assert!(svg.contains("<title>Nov 16 23:00:"), "{svg}");
        assert!(
            !svg.contains("999"),
            "the day after the range stays out: {svg}"
        );
        assert!(svg.contains(">max 10 tokens</text>"), "{svg}");
    }

    #[test]
    fn an_hourly_series_skips_labels_the_axis_cannot_read() {
        let rows = [
            hour_row("bogus", 10, 0, 1),
            hour_row("2026-11-08", 10, 0, 1),
            hour_row("2026-11-08 25", 10, 0, 1),
            hour_row("2026-02-30 10", 10, 0, 1),
        ];
        let svg = hourly_token_bars(&rows, day(2026, 11, 8), day(2026, 11, 8), ChartTheme::Light);
        assert!(svg.contains(EMPTY_NOTE), "{svg}");
        assert_eq!(count(&svg, "<rect"), 0, "{svg}");
    }

    #[test]
    fn hourly_cost_bars_price_one_bar_per_hour() {
        let costs = [
            ("2026-11-08 10".to_owned(), 60_000),
            ("2026-11-08 10".to_owned(), 60_000),
            ("2026-11-08 12".to_owned(), 500_000),
        ];
        let svg = hourly_cost_bars(
            &costs,
            day(2026, 11, 8),
            day(2026, 11, 8),
            ChartTheme::Light,
        );
        assert_eq!(
            count(&svg, "<rect"),
            2,
            "the two rows of ten o'clock share a bar: {svg}"
        );
        assert_eq!(rects(&svg, COST_FILL), 2);
        assert!(svg.contains("<title>10:00: $0.12</title>"), "{svg}");
        assert!(svg.contains(">max $0.50</text>"), "{svg}");
    }

    #[test]
    fn hourly_status_code_bars_stack_the_codes_of_an_hour() {
        let rows = [
            hour_code_row("2026-11-16 14", Some(200), 2),
            hour_code_row("2026-11-16 14", Some(500), 1),
        ];
        let svg = hourly_status_code_bars(
            &rows,
            day(2026, 11, 16),
            day(2026, 11, 16),
            ChartTheme::Light,
        );
        let (bars, legend) = bars_only(&svg);
        assert_eq!(count(bars, "<rect"), 2, "{bars}");
        assert!(
            svg.contains("<title>14:00: 3 requests (2 200, 1 500)</title>"),
            "{svg}"
        );
        assert!(svg.contains(">max 3 requests</text>"), "{svg}");
        for label in ["200", "500"] {
            assert!(
                legend.contains(&format!(">{label}</text>")),
                "{label} is missing from the legend: {legend}"
            );
        }
    }

    #[test]
    fn latency_bars_plot_the_mean_of_every_hour_of_a_day() {
        let rows = [
            latency_row("2026-11-08 10", 20.0, 2),
            latency_row("2026-11-08 14", 40.0, 1),
        ];
        let svg = latency_bars(&rows, day(2026, 11, 8), day(2026, 11, 8), ChartTheme::Light);
        assert!(svg.contains("aria-label=\"Latency per hour\""), "{svg}");
        assert_eq!(count(&svg, "<rect"), 2, "{svg}");
        assert_eq!(rects(&svg, ACCENT_FILL), 2);
        assert!(
            svg.contains("<title>10:00: 20.0 ms (2 requests)</title>"),
            "{svg}"
        );
        assert!(
            svg.contains("<title>14:00: 40.0 ms (1 requests)</title>"),
            "{svg}"
        );
        assert!(svg.contains(">max 40.0 ms</text>"), "{svg}");
        assert!(
            svg.contains("y=\"20.00\""),
            "the tallest mean fills the plot: {svg}"
        );
    }

    #[test]
    fn a_daily_latency_range_folds_the_hours_of_a_day_weighted() {
        let rows = [
            latency_row("2026-11-08 10", 10.0, 1),
            latency_row("2026-11-08 11", 30.0, 3),
            latency_row("2026-11-09 09", 50.0, 2),
        ];
        let svg = latency_bars(
            &rows,
            day(2026, 11, 1),
            day(2026, 11, 10),
            ChartTheme::Light,
        );
        assert!(svg.contains("aria-label=\"Latency per day\""), "{svg}");
        assert_eq!(count(&svg, "<rect"), 2, "{svg}");
        assert!(
            svg.contains("<title>Day 8: 25.0 ms (4 requests)</title>"),
            "the hours fold into the mean of their timed requests: {svg}"
        );
        assert!(
            svg.contains("<title>Day 9: 50.0 ms (2 requests)</title>"),
            "{svg}"
        );
        assert!(svg.contains(">max 50.0 ms</text>"), "{svg}");
    }

    #[test]
    fn an_empty_hourly_series_renders_a_placeholder_chart() {
        let (from, to) = (day(2026, 11, 8), day(2026, 11, 9));
        let charts = [
            hourly_token_bars(&[], from, to, ChartTheme::Light),
            hourly_cost_bars(&[], from, to, ChartTheme::Light),
            hourly_status_code_bars(&[], from, to, ChartTheme::Light),
            latency_bars(&[], from, to, ChartTheme::Light),
        ];
        for svg in charts {
            assert!(svg.starts_with("<svg "), "{svg}");
            assert!(svg.ends_with("</svg>"), "{svg}");
            assert!(svg.contains(EMPTY_NOTE), "{svg}");
            assert_eq!(count(&svg, "<rect"), 0, "{svg}");
        }
    }

    #[test]
    fn the_dark_theme_paints_the_hourly_and_latency_charts_dark() {
        let rows = [hour_row("2026-11-08 14", 100, 50, 1)];
        let svg = hourly_token_bars(&rows, day(2026, 11, 8), day(2026, 11, 8), ChartTheme::Dark);
        assert_eq!(rects(&svg, DARK_ACCENT_FILL), 1, "{svg}");
        assert_eq!(rects(&svg, DARK_MUTED_FILL), 1, "{svg}");
        assert_eq!(
            rects(&svg, ACCENT_FILL),
            0,
            "no light color in a dark chart"
        );
        let strokes = line_strokes(&svg);
        assert!(
            strokes.len() > 2 && strokes.iter().all(|stroke| *stroke == DARK_BORDER_STROKE),
            "every axis rule and gridline follows the theme: {svg}"
        );
        assert!(
            !strokes.contains(&BORDER_STROKE),
            "no light rule in a dark chart: {svg}"
        );

        let rows = [latency_row("2026-11-08 14", 12.5, 1)];
        let svg = latency_bars(&rows, day(2026, 11, 8), day(2026, 11, 8), ChartTheme::Dark);
        assert_eq!(rects(&svg, DARK_ACCENT_FILL), 1, "{svg}");
        assert_eq!(rects(&svg, ACCENT_FILL), 0, "{svg}");
    }

    /// The x coordinates and the texts of the middle-anchored axis labels of
    /// a chart, in the order they appear.
    fn axis_labels(svg: &str) -> Vec<(f64, String)> {
        svg.split("<text ")
            .skip(1)
            .filter_map(|element| {
                let (attributes, rest) = element.split_once('>')?;
                if !attributes.contains("text-anchor=\"middle\"") {
                    return None;
                }
                let x: f64 = attributes
                    .split("x=\"")
                    .nth(1)?
                    .split('"')
                    .next()?
                    .parse()
                    .ok()?;
                Some((x, rest.split('<').next()?.to_owned()))
            })
            .collect()
    }

    #[test]
    fn the_axis_labels_of_an_hourly_chart_fit_the_plot_without_colliding() {
        // The widest hourly axis is the 48 slot one of a two day range, and
        // it carries the longest labels; the one day axis is checked with it,
        // so neither can drift into a collision unnoticed.
        for (from, to, bucket) in [
            (day(2026, 11, 8), day(2026, 11, 8), "2026-11-08 14"),
            (day(2026, 11, 16), day(2026, 11, 17), "2026-11-16 14"),
        ] {
            let rows = [hour_row(bucket, 1, 0, 1)];
            let svg = hourly_token_bars(&rows, from, to, ChartTheme::Light);
            let labels = axis_labels(&svg);
            assert_eq!(labels.len(), 8, "{svg}");
            for (x, label) in &labels {
                assert!(
                    (PAD_LEFT..=PAD_LEFT + PLOT_W).contains(x),
                    "{label} at {x} leaves the plot: {svg}"
                );
            }
            // The labels are laid out with an estimated character width, the
            // same one the legend uses, so two neighbours that share it must
            // not overlap.
            let widths: Vec<f64> = labels
                .iter()
                .map(|(_, label)| label.chars().count() as f64 * 6.0)
                .collect();
            for (index, pair) in labels.windows(2).enumerate() {
                let gap = pair[1].0 - pair[0].0;
                let needed = (widths[index] + widths[index + 1]) / 2.0;
                assert!(
                    gap >= needed,
                    "{:?} and {:?} are {gap:.2} apart, {needed:.2} needed: {svg}",
                    pair[0].1,
                    pair[1].1
                );
            }
        }
    }

    #[test]
    fn a_known_max_draws_one_gridline_tick_per_step_below_it() {
        // A day of 150 tokens steps by 50, so the axis carries the ticks at
        // 50 and 100: the top line keeps the max and gains no duplicate.
        let rows = [row("2026-11-01", None, 100, 50, 1)];
        let svg = daily_token_bars(&rows, ChartTheme::Light);
        assert!(svg.contains(">max 150 tokens</text>"), "{svg}");
        assert_eq!(
            count(&svg, "<line "),
            4,
            "the top line, the baseline and two ticks: {svg}"
        );
        let ticks = y_ticks(&svg);
        let texts: Vec<&str> = ticks.iter().map(|(_, text)| text.as_str()).collect();
        assert_eq!(
            texts,
            ["50", "100"],
            "the ticks ascend from the baseline: {svg}"
        );
        for window in ticks.windows(2) {
            assert!(
                window[0].0 > window[1].0,
                "a larger tick sits higher on the plot: {svg}"
            );
        }
        for (y, text) in &ticks {
            assert!(
                *y > PAD_TOP + 3.0,
                "the {text} tick sits strictly below the top line: {svg}"
            );
            assert!(
                *y < BASE_Y + 3.0,
                "the {text} tick sits strictly above the baseline: {svg}"
            );
        }
    }

    #[test]
    fn nice_step_picks_the_next_number_of_the_1_2_5_ladder() {
        for (max, step) in [
            (4.0, 1.0),
            (8.0, 2.0),
            (20.0, 5.0),
            (40.0, 10.0),
            (100.0, 50.0),
            (800.0, 200.0),
            (4000.0, 1000.0),
            (1.5, 0.5),
        ] {
            let got = nice_step(max);
            assert!(
                (got - step).abs() <= step * 1e-9,
                "nice_step({max}) = {got}, wanted {step}"
            );
        }
        // A chart without a positive, finite max carries no step at all,
        // which is how a zero series ends up without a gridline.
        for max in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(nice_step(max), 0.0, "nice_step({max})");
        }
    }

    #[test]
    fn every_tick_sits_above_zero_and_strictly_below_the_max() {
        for max in [2.5_f64, 3.0, 7.0, 40.0, 150.0, 999.9, 1_234.0, 1e6, 3.6e9] {
            let ticks = tick_values(max, 1.0);
            assert!(!ticks.is_empty(), "a {max} max carries a tick: {ticks:?}");
            assert!(ticks.len() <= 3, "a {max} max stays sparse: {ticks:?}");
            for window in ticks.windows(2) {
                assert!(window[0] < window[1], "ticks ascend: {ticks:?}");
            }
            for value in ticks {
                assert!(value > 0.0, "a {max} max has no tick at zero");
                assert!(value < max, "a {max} max has no tick on top of it");
            }
        }
        // A latency axis keeps the fractions a whole-number axis cannot
        // show: the same 1.5 max steps by 0.5 in milliseconds but by a whole
        // token or request.
        assert_eq!(tick_values(1.5, 0.1), vec![0.5, 1.0]);
        assert_eq!(tick_values(1.5, 1.0), vec![1.0]);
        // A max a chart cannot hold carries no tick at all.
        for max in [0.0, -3.0] {
            assert!(tick_values(max, 1.0).is_empty(), "max {max}");
        }
    }

    #[test]
    fn tick_labels_read_compactly_for_every_series() {
        // Tokens and requests are whole numbers, so a tick never shows a
        // fraction, and a huge count collapses the way the max label does.
        assert_eq!(count_tick(999.0), "999");
        assert_eq!(count_tick(1_000.0), "1.0K");
        assert_eq!(count_tick(3_500_000.0), "3.5M");
        assert_eq!(ChartKind::Stacked.tick_label(1_600.0), "1.6K");
        // A cost tick reuses the dollar form of the max label.
        assert_eq!(ChartKind::Single.tick_label(1_250_000.0), "$1.25");
        assert_eq!(ChartKind::Single.tick_label(999_999_999.0), "$1000.00");
        assert_eq!(ChartKind::Single.tick_label(1_500_000_000.0), "$1.5K");
        // A latency tick keeps a decimal below ten and whole milliseconds
        // above, so a small mean stays readable.
        assert_eq!(latency_tick(0.5), "0.5 ms");
        assert_eq!(latency_tick(9.94), "9.9 ms");
        assert_eq!(latency_tick(10.0), "10 ms");
        assert_eq!(latency_tick(850.0), "850 ms");
        assert_eq!(latency_tick(12_000.0), "12.0K ms");
    }

    #[test]
    fn every_chart_family_labels_its_y_axis_in_its_own_unit() {
        // Tokens: a 150 token max steps by 50.
        let tokens = daily_token_bars(&[row("2026-11-01", None, 100, 50, 1)], ChartTheme::Light);
        let texts: Vec<String> = y_ticks(&tokens).into_iter().map(|(_, text)| text).collect();
        assert_eq!(texts, ["50", "100"], "{tokens}");

        // Cost: a 12 cent max steps by 5 cents, priced like the max label.
        let cost = daily_cost_bars(&[("2026-11-01".to_owned(), 123_457)], ChartTheme::Light);
        assert!(cost.contains(">max $0.12</text>"), "{cost}");
        let texts: Vec<String> = y_ticks(&cost).into_iter().map(|(_, text)| text).collect();
        assert_eq!(texts, ["$0.05", "$0.10"], "{cost}");

        // Requests: a ten request max steps by five.
        let requests =
            daily_status_code_bars(&[code_row("2026-11-01", Some(200), 10)], ChartTheme::Light);
        let texts: Vec<String> = y_ticks(&requests)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert_eq!(texts, ["5"], "{requests}");

        // Latency: a 40 ms max steps by ten whole milliseconds.
        let latency = latency_bars(
            &[latency_row("2026-11-08 10", 40.0, 1)],
            day(2026, 11, 8),
            day(2026, 11, 8),
            ChartTheme::Light,
        );
        assert!(latency.contains(">max 40.0 ms</text>"), "{latency}");
        let texts: Vec<String> = y_ticks(&latency)
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert_eq!(texts, ["10 ms", "20 ms", "30 ms"], "{latency}");
    }

    #[test]
    fn a_placeholder_chart_carries_no_gridline_ticks() {
        let (from, to) = (day(2026, 11, 8), day(2026, 11, 9));
        let charts = [
            daily_token_bars(&[], ChartTheme::Light),
            daily_cost_bars(&[], ChartTheme::Light),
            daily_status_code_bars(&[], ChartTheme::Light),
            latency_bars(&[], from, to, ChartTheme::Light),
        ];
        for svg in charts {
            assert!(svg.contains(EMPTY_NOTE), "{svg}");
            assert!(
                y_ticks(&svg).is_empty(),
                "a placeholder carries no tick label: {svg}"
            );
            assert_eq!(
                count(&svg, "<line "),
                1,
                "a placeholder keeps its baseline and nothing else: {svg}"
            );
        }
    }
}
