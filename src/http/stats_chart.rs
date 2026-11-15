//! Server-side SVG bar charts for the admin stats page.
//!
//! The stats page renders three charts, tokens per day, cost per day and
//! requests per day, as plain `<svg>` markup generated here: no JavaScript,
//! no charting crate and no template logic beyond the `|safe` filter that
//! injects the returned string. Heights are normalized to the largest daily
//! total in the series, every bar carries a `<title>` so the browser shows a
//! native tooltip without any script, and all three share the same viewport,
//! gutters, day slots and day labels.
//!
//! The token chart consumes [`DayUsage`] rows exactly as the SQL aggregate
//! hands them over and buckets them with [`aggregate_daily`]. The cost chart
//! takes a pre-summed `(day, micro-USD)` series instead of the same rows,
//! because a price is a function of the price table of the key's channel
//! ([`crate::usage::group_cost`]), which no usage row carries: the caller
//! prices its own query groups and sums them per day, so no cost is ever
//! derived from a row priced with another channel's rates. The status chart
//! consumes [`DayStatusCodeUsage`] rows and stacks one segment per exact
//! response status, with a legend that maps the palette colors to the codes.
//!
//! Rows whose day label does not parse to a day of the month are skipped,
//! and a series that maps to no bar at all renders a placeholder note inside
//! a valid, empty chart rather than an error.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The aggregate row the SQL layer defines, reused here so both branches of
/// the stats page read one shape instead of two identical copies.
pub(crate) use crate::db::request_logs::{DayStatusCodeUsage, DayUsage, StatusBucket};

/// Viewport width in user units; the root element scales it to the card.
const VIEW_W: f64 = 720.0;
/// Viewport height in user units.
const VIEW_H: f64 = 260.0;
/// Left gutter before the first day slot; every value label sits above the
/// plot, so this is spacing rather than a place for text.
const PAD_LEFT: f64 = 20.0;
/// Right gutter.
const PAD_RIGHT: f64 = 12.0;
/// Top gutter: the axis label sits here, above the tallest bar.
const PAD_TOP: f64 = 20.0;
/// Bottom gutter under the day labels.
const PAD_BOTTOM: f64 = 30.0;
/// Horizontal extent of the plotting area.
const PLOT_W: f64 = VIEW_W - PAD_LEFT - PAD_RIGHT;
/// Vertical extent of the plotting area.
const PLOT_H: f64 = VIEW_H - PAD_TOP - PAD_BOTTOM;
/// Day slots on the x-axis: a fixed 1 through 31 so the axis is stable
/// across months independent of the data.
const DAY_SLOTS: u32 = 31;
/// Width of one day slot.
const SLOT_W: f64 = PLOT_W / DAY_SLOTS as f64;
/// Width of the bar inside a slot.
const BAR_W: f64 = SLOT_W * 0.62;
/// Y coordinate of the baseline every bar grows from.
const BASE_Y: f64 = PAD_TOP + PLOT_H;

/// `--accent` from `assets/admin.css`, the input-token segment.
const ACCENT_FILL: &str = "#2563eb";
/// `--text-muted` from `assets/admin.css`, the output-token segment.
const MUTED_FILL: &str = "#6a7280";
/// `--accent-dark` from `assets/admin.css`, the cost bars and the fifth slot
/// of the per-code palette.
const COST_FILL: &str = "#1e50c8";
/// `--ok` from `assets/admin.css`, the green slot of the per-code palette.
const OK_FILL: &str = "#15803d";
/// `--warn` from `assets/admin.css`, the amber slot of the per-code palette.
const WARN_FILL: &str = "#92400e";
/// `--danger` from `assets/admin.css`, the red slot of the per-code palette.
const DANGER_FILL: &str = "#b91c1c";
/// `--border` from `assets/admin.css`, the axis rules.
const BORDER_STROKE: &str = "#d9dde3";

/// The segment colors of the requests chart, cycled over the status codes in
/// their canonical order: `--ok`, `--warn`, `--danger`, `--accent`,
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

/// Accessible name of the tokens chart.
const TOKENS_LABEL: &str = "Tokens per day";
/// Accessible name of the cost chart.
const COST_LABEL: &str = "Cost per day";
/// Accessible name of the requests chart.
const STATUS_LABEL: &str = "Requests per day";
/// Note rendered when a series maps to no bar.
const EMPTY_NOTE: &str = "No usage in this period";
/// Class of the legend group of a chart, which is also what tells the legend
/// rects apart from the bar rects.
const LEGEND_OPEN: &str = "<g class=\"chart-legend\">";

/// Which pair of values a chart plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartKind {
    /// Input and output tokens as two stacked segments of one bar.
    Stacked,
    /// One bar per day from a caller-priced cost series.
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
}

/// One day of a chart series, already summed across keys and models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DayBar {
    /// Day of the month, 1 through 31.
    day: u32,
    /// Bottom segment: input tokens, or the micro-USD cost.
    input: i64,
    /// Top segment: output tokens; zero for the cost chart.
    output: i64,
    /// Logged requests behind the bar.
    requests: i64,
}

impl DayBar {
    /// Height driver of the bar, saturating so no input can overflow.
    fn total(&self) -> i64 {
        self.input.saturating_add(self.output)
    }
}

/// One day of the requests chart: the request count of every exact response
/// status the day logged.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StatusCodeBar {
    /// Day of the month, 1 through 31.
    day: u32,
    /// One `(status, request count)` pair per code of the day, in the
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
/// the legend and the colors of one month identical whatever order the
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
/// input tokens at the bottom and output tokens on top.
///
/// Heights are normalized to the largest daily total, and days without data
/// stay empty slots so the axis keeps a fixed 1 through 31 extent. An empty
/// result or a series of unparsable day labels renders a placeholder note.
pub(crate) fn daily_token_bars(rows: &[DayUsage]) -> String {
    let bars = token_series(rows);
    if bars.is_empty() {
        return placeholder_chart(TOKENS_LABEL);
    }
    render_chart(TOKENS_LABEL, &bars, ChartKind::Stacked)
}

/// Renders the cost-per-day chart: one bar per day, already priced by the
/// caller.
///
/// The series is `(day label, micro-USD)` and is summed per day here, so the
/// caller can hand over one entry per query group instead of pre-bucketing.
/// See the module docs for why cost does not travel as [`DayUsage`].
pub(crate) fn daily_cost_bars(costs: &[(String, i64)]) -> String {
    let bars = cost_series(costs);
    if bars.is_empty() {
        return placeholder_chart(COST_LABEL);
    }
    render_chart(COST_LABEL, &bars, ChartKind::Single)
}

/// Buckets the token rows into the fixed day slots.
fn token_series(rows: &[DayUsage]) -> Vec<DayBar> {
    let mut slots: BTreeMap<u32, DayBar> = BTreeMap::new();
    for (day, prompt, completion, requests) in aggregate_daily(rows) {
        let Some(number) = day_number(&day) else {
            continue;
        };
        let bar = slots.entry(number).or_insert(DayBar {
            day: number,
            input: 0,
            output: 0,
            requests: 0,
        });
        bar.input = bar.input.saturating_add(prompt);
        bar.output = bar.output.saturating_add(completion);
        bar.requests = bar.requests.saturating_add(requests);
    }
    slots.into_values().collect()
}

/// Buckets the caller-priced costs into the fixed day slots.
fn cost_series(costs: &[(String, i64)]) -> Vec<DayBar> {
    let mut slots: BTreeMap<u32, DayBar> = BTreeMap::new();
    for (day, micro_usd) in costs {
        let Some(number) = day_number(day) else {
            continue;
        };
        let bar = slots.entry(number).or_insert(DayBar {
            day: number,
            input: 0,
            output: 0,
            requests: 0,
        });
        bar.input = bar.input.saturating_add(*micro_usd);
    }
    slots.into_values().collect()
}

/// Renders the requests-per-day chart: one bar per day that has data, a
/// segment per exact upstream status code, and a legend mapping the colors to
/// the codes.
///
/// The codes are the ones the SQL aggregate groups by, so the bar segments
/// and the tooltip figures always agree, and the class filter that narrows
/// the aggregate narrows what the chart shows. An empty series renders a
/// placeholder note, exactly like the other two charts.
pub(crate) fn daily_status_code_bars(rows: &[DayStatusCodeUsage]) -> String {
    let bars = status_code_series(rows);
    if bars.is_empty() {
        return placeholder_chart(STATUS_LABEL);
    }
    let palette = code_palette(&bars);
    render_status_code_chart(&bars, &palette)
}

/// Buckets the per-code rows into the fixed day slots, folding the codes of
/// one day into one bar.
fn status_code_series(rows: &[DayStatusCodeUsage]) -> Vec<StatusCodeBar> {
    let mut slots: BTreeMap<u32, Vec<(Option<i64>, i64)>> = BTreeMap::new();
    for row in rows {
        let Some(number) = day_number(&row.day) else {
            continue;
        };
        let counts = slots.entry(number).or_default();
        match counts.iter_mut().find(|(code, _)| *code == row.status) {
            Some((_, count)) => *count = count.saturating_add(row.requests),
            None => counts.push((row.status, row.requests)),
        }
    }
    slots
        .into_iter()
        .map(|(day, mut counts)| {
            counts.sort_by_key(|(code, _)| code_order(*code));
            StatusCodeBar { day, counts }
        })
        .collect()
}

/// Every code the series carries in canonical order, each with its color: the
/// palette cycles in that order, so a code keeps one color across the bars and
/// the legend, and two codes share a color only once the palette wraps.
fn code_palette(bars: &[StatusCodeBar]) -> Vec<(Option<i64>, &'static str)> {
    let mut codes: Vec<Option<i64>> = bars
        .iter()
        .flat_map(|bar| bar.counts.iter().map(|(code, _)| *code))
        .collect();
    codes.sort_by_key(|code| code_order(*code));
    codes.dedup();
    codes
        .into_iter()
        .enumerate()
        .map(|(index, code)| (code, STATUS_CODE_PALETTE[index % STATUS_CODE_PALETTE.len()]))
        .collect()
}

/// The color of one code, from the series palette.
fn code_fill(palette: &[(Option<i64>, &'static str)], status: Option<i64>) -> &'static str {
    palette
        .iter()
        .find(|(code, _)| *code == status)
        .map(|(_, fill)| *fill)
        .unwrap_or(MUTED_FILL)
}

/// Day of the month a `YYYY-MM-DD` (or bare day) label names, `None` when
/// the label does not parse or falls outside 1 through 31.
fn day_number(day: &str) -> Option<u32> {
    let number: u32 = day.rsplit('-').next()?.parse().ok()?;
    (1..=DAY_SLOTS).contains(&number).then_some(number)
}

/// One complete chart: frame, axes, bars, closing tag.
fn render_chart(label: &str, bars: &[DayBar], kind: ChartKind) -> String {
    let max = bars.iter().map(DayBar::total).max().unwrap_or(0);
    let scale = if max > 0 { PLOT_H / max as f64 } else { 0.0 };
    let mut svg = String::new();
    push_open(&mut svg, label);
    push_axes(&mut svg, &kind.max_label(max));
    for bar in bars {
        push_bar(&mut svg, bar, kind, scale);
    }
    push_close(&mut svg);
    svg
}

/// One complete requests chart: frame, axes, stacked bars, legend, closing
/// tag. The legend goes into the free top gutter, so it takes no room from
/// the plot and the chart keeps the height of the other two.
fn render_status_code_chart(
    bars: &[StatusCodeBar],
    palette: &[(Option<i64>, &'static str)],
) -> String {
    let max = bars.iter().map(StatusCodeBar::total).max().unwrap_or(0);
    let scale = if max > 0 { PLOT_H / max as f64 } else { 0.0 };
    let mut svg = String::new();
    push_open(&mut svg, STATUS_LABEL);
    push_axes(&mut svg, &format!("max {} requests", compact_count(max)));
    for bar in bars {
        push_status_code_bar(&mut svg, bar, palette, scale);
    }
    push_code_legend(&mut svg, palette);
    push_close(&mut svg);
    svg
}

/// A valid chart carrying only the note that the series is empty.
fn placeholder_chart(label: &str) -> String {
    let mut svg = String::new();
    push_open(&mut svg, label);
    push_baseline(&mut svg);
    let _ = write!(
        svg,
        "<text x=\"{:.2}\" y=\"{:.2}\" text-anchor=\"middle\" font-size=\"13\" \
         fill=\"{MUTED_FILL}\">{EMPTY_NOTE}</text>",
        PAD_LEFT + PLOT_W / 2.0,
        PAD_TOP + PLOT_H / 2.0,
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
/// day labels.
fn push_axes(svg: &mut String, max_label: &str) {
    let right = PAD_LEFT + PLOT_W;
    let _ = write!(
        svg,
        "<line x1=\"{PAD_LEFT:.2}\" y1=\"{PAD_TOP:.2}\" x2=\"{right:.2}\" y2=\"{PAD_TOP:.2}\" \
         stroke=\"{BORDER_STROKE}\" stroke-width=\"1\"/>",
    );
    push_baseline(svg);
    let _ = write!(
        svg,
        "<text x=\"{PAD_LEFT:.2}\" y=\"{:.2}\" text-anchor=\"start\" font-size=\"11\" \
         fill=\"{MUTED_FILL}\">{max_label}</text>",
        PAD_TOP - 6.0,
    );
    for day in 1..=DAY_SLOTS {
        if !labelled(day) {
            continue;
        }
        let _ = write!(
            svg,
            "<text x=\"{:.2}\" y=\"{:.2}\" text-anchor=\"middle\" font-size=\"11\" \
             fill=\"{MUTED_FILL}\">{day}</text>",
            slot_center(day),
            BASE_Y + 18.0,
        );
    }
}

/// The x-axis rule bars grow from.
fn push_baseline(svg: &mut String) {
    let _ = write!(
        svg,
        "<line x1=\"{PAD_LEFT:.2}\" y1=\"{BASE_Y:.2}\" x2=\"{:.2}\" y2=\"{BASE_Y:.2}\" \
         stroke=\"{BORDER_STROKE}\" stroke-width=\"1\"/>",
        PAD_LEFT + PLOT_W,
    );
}

/// One bar plus its hover tooltip, positioned and normalized by `scale`.
///
/// A day whose total is zero keeps its title but draws no rectangle, so the
/// chart never invents a height that carries no usage.
fn push_bar(svg: &mut String, bar: &DayBar, kind: ChartKind, scale: f64) {
    let input_h = (bar.input as f64 * scale).min(PLOT_H);
    let output_h = (bar.output as f64 * scale).min(PLOT_H - input_h);
    let _ = write!(svg, "<g><title>{}</title>", bar_title(bar, kind));
    if input_h > 0.0 {
        let fill = match kind {
            ChartKind::Stacked => ACCENT_FILL,
            ChartKind::Single => COST_FILL,
        };
        push_rect(svg, bar.day, BASE_Y - input_h, input_h, fill);
    }
    if kind == ChartKind::Stacked && output_h > 0.0 {
        push_rect(
            svg,
            bar.day,
            BASE_Y - input_h - output_h,
            output_h,
            MUTED_FILL,
        );
    }
    svg.push_str("</g>");
}

/// One segment rectangle.
fn push_rect(svg: &mut String, day: u32, y: f64, height: f64, fill: &str) {
    let _ = write!(
        svg,
        "<rect x=\"{:.2}\" y=\"{y:.2}\" width=\"{BAR_W:.2}\" height=\"{height:.2}\" \
         fill=\"{fill}\"/>",
        bar_x(day),
    );
}

/// One requests bar plus its hover tooltip: the code segments stacked from
/// the baseline upwards in canonical code order, each normalized by `scale`.
///
/// A segment that carries no request draws no rectangle, so a day whose codes
/// are all zero keeps its title and stays an empty slot.
fn push_status_code_bar(
    svg: &mut String,
    bar: &StatusCodeBar,
    palette: &[(Option<i64>, &'static str)],
    scale: f64,
) {
    let _ = write!(svg, "<g><title>{}</title>", status_code_bar_title(bar));
    let mut top = BASE_Y;
    for (code, count) in &bar.counts {
        let height = (*count as f64 * scale).min(top - PAD_TOP);
        if height <= 0.0 {
            continue;
        }
        top -= height;
        push_rect(svg, bar.day, top, height, code_fill(palette, *code));
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
fn push_code_legend(svg: &mut String, palette: &[(Option<i64>, &'static str)]) {
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
             font-size=\"11\" fill=\"{MUTED_FILL}\">{label}</text>",
            baseline - 4.0 - SWATCH / 2.0,
        );
        right = swatch_x - ENTRY_GAP;
    }
    legend.push_str("</g>");
    svg.push_str(&legend);
}

/// Tooltip text of one bar.
fn bar_title(bar: &DayBar, kind: ChartKind) -> String {
    match kind {
        ChartKind::Stacked => format!(
            "Day {}: {} tokens ({} in, {} out) in {} requests",
            bar.day,
            group_digits(bar.total()),
            group_digits(bar.input),
            group_digits(bar.output),
            group_digits(bar.requests),
        ),
        ChartKind::Single => format!("Day {}: {}", bar.day, format_usd(bar.input)),
    }
}

/// Tooltip text of one requests bar: the day's request count and how it
/// splits across the exact codes, so a hover answers which statuses the bar
/// holds.
fn status_code_bar_title(bar: &StatusCodeBar) -> String {
    let mut split = String::new();
    for (index, (code, count)) in bar.counts.iter().enumerate() {
        if index > 0 {
            split.push_str(", ");
        }
        let _ = write!(split, "{} {}", group_digits(*count), code_label(*code));
    }
    format!(
        "Day {}: {} requests ({split})",
        bar.day,
        group_digits(bar.total())
    )
}

/// Left edge of a day's bar.
fn bar_x(day: u32) -> f64 {
    PAD_LEFT + (day.saturating_sub(1)) as f64 * SLOT_W + (SLOT_W - BAR_W) / 2.0
}

/// Horizontal center of a day's slot, where its axis label sits.
fn slot_center(day: u32) -> f64 {
    PAD_LEFT + (day.saturating_sub(1)) as f64 * SLOT_W + SLOT_W / 2.0
}

/// Whether a day carries an axis label; every fifth day keeps the row of
/// labels readable without crowding the bars.
fn labelled(day: u32) -> bool {
    day == 1 || day.is_multiple_of(5)
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
        let svg = daily_status_code_bars(&[]);
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
        let svg = daily_status_code_bars(&rows);
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
        let svg = daily_status_code_bars(&rows);
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
        let svg = daily_status_code_bars(&rows);
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
        let svg = daily_status_code_bars(&rows);
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
        let svg = daily_status_code_bars(&[code_row("2026-11-07", Some(503), 2)]);
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
        let svg = daily_status_code_bars(&[code_row("2026-11-07", Some(200), 0)]);
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
        let svg = daily_status_code_bars(&rows);
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
            daily_status_code_bars(&sample()),
            daily_status_code_bars(&sample())
        );
        assert_eq!(
            daily_status_code_bars(&sample()),
            daily_status_code_bars(&reversed),
            "the colors follow the canonical code order, not the row order"
        );
    }

    #[test]
    fn status_code_values_saturate_and_stay_inside_the_plot() {
        let rows = [
            code_row("2026-11-01", Some(200), i64::MAX),
            code_row("2026-11-01", Some(500), i64::MAX),
        ];
        let svg = daily_status_code_bars(&rows);
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
        let svg = daily_token_bars(&[]);
        assert!(svg.starts_with("<svg "));
        assert!(svg.ends_with("</svg>"));
        assert!(svg.contains(EMPTY_NOTE));
        assert_eq!(count(&svg, "<rect"), 0);
        assert_eq!(count(&svg, "<title>"), 0);

        let cost = daily_cost_bars(&[]);
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
        let svg = daily_token_bars(&rows);
        assert!(svg.contains(EMPTY_NOTE));
        assert_eq!(count(&svg, "<rect"), 0);
    }

    #[test]
    fn single_day_fills_the_plot() {
        let rows = [row("2026-11-03", Some("gpt-4o"), 100, 100, 2)];
        let svg = daily_token_bars(&rows);
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
        let svg = daily_token_bars(&rows);
        assert_eq!(count(&svg, "<g><title>"), DAY_SLOTS as usize);
        // One input segment per day; the output value is zero.
        assert_eq!(count(&svg, "<rect"), DAY_SLOTS as usize);
        assert_eq!(rects(&svg, ACCENT_FILL), DAY_SLOTS as usize);
        assert_eq!(rects(&svg, MUTED_FILL), 0);
    }

    #[test]
    fn day_labels_are_sparse() {
        let rows = [row("2026-11-01", None, 1, 1, 1)];
        let svg = daily_token_bars(&rows);
        // Days 1, 5, 10, 15, 20, 25 and 30 carry a day label; the
        // largest-value label above the plot is the only left-anchored one.
        assert_eq!(count(&svg, "text-anchor=\"middle\""), 7);
        assert_eq!(count(&svg, "text-anchor=\"start\""), 1);
        assert_eq!(count(&svg, "text-anchor=\"end\""), 0);
    }

    #[test]
    fn zero_values_draw_a_title_without_a_bar() {
        // Both segments are drawn while the values are non-zero.
        let rows = [row("2026-11-07", None, 1, 2, 3)];
        assert_eq!(count(&daily_token_bars(&rows), "<rect"), 2);
        let zero = [row("2026-11-07", None, 0, 0, 0)];
        let svg = daily_token_bars(&zero);
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
        let svg = daily_token_bars(&rows);
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
        let svg = daily_token_bars(&rows);
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
        assert_eq!(daily_token_bars(&sample()), daily_token_bars(&sample()));
        assert_eq!(daily_token_bars(&sample()), daily_token_bars(&reversed));
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
        let svg = daily_cost_bars(&costs);
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
        let svg = daily_cost_bars(&costs);
        assert_eq!(count(&svg, "<rect"), 2);
        assert!(!svg.contains("NaN"));
        assert!(!svg.contains("y=\"-"));
        assert!(svg.contains("y=\"20.00\""));
        assert_eq!(daily_cost_bars(&costs), daily_cost_bars(&costs));
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
}
