//! Server-side SVG bar charts for the admin stats page.
//!
//! The stats page renders two charts, tokens per day and cost per day, as
//! plain `<svg>` markup generated here: no JavaScript, no charting crate and
//! no template logic beyond the `|safe` filter that injects the returned
//! string. Heights are normalized to the largest daily total in the series,
//! and every bar carries a `<title>` so the browser shows a native tooltip
//! without any script.
//!
//! The token chart consumes [`DayUsage`] rows exactly as the SQL aggregate
//! hands them over and buckets them with [`aggregate_daily`]. The cost chart
//! takes a pre-summed `(day, micro-USD)` series instead of the same rows,
//! because a price is a function of the cached-token dimensions
//! ([`crate::usage::group_cost`]) that `DayUsage` deliberately does not
//! carry: the caller prices its own query groups and sums them per day, so
//! no cost is ever derived from a truncated row.
//!
//! Rows whose day label does not parse to a day of the month are skipped,
//! and a series that maps to no bar at all renders a placeholder note inside
//! a valid, empty chart rather than an error.
//!
//! This branch carries its own copy of the pinned row shape, because the SQL
//! aggregate that defines it lands on its own branch. When the two branches
//! meet, the struct below becomes a re-export of
//! `crate::db::request_logs::DayUsage` and the charts keep reading the same
//! fields.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// One aggregate row of the stats page: the tokens and requests one day, key
/// and model accounted for.
///
/// The shape is fixed by the SQL aggregate that fills it, so the chart code
/// never queries the database itself. Its derives match that aggregate's
/// type, which is what lets the copy collapse into a re-export.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DayUsage {
    /// The day the row was grouped by, `YYYY-MM-DD` for the canonical
    /// `created_at` text form. Only the day of the month is read here.
    pub day: String,
    /// The key the row was grouped by. The daily charts sum across keys, so
    /// the field exists for the caller's filtering.
    pub key_id: i64,
    /// The logged model name, NULL for rows logged without one. Irrelevant
    /// to a daily total and kept for the caller's grouping.
    pub model: Option<String>,
    /// Input tokens of the day's rows.
    pub prompt_tokens: i64,
    /// Output tokens of the day's rows.
    pub completion_tokens: i64,
    /// Logged requests of the day's rows, whatever their status.
    pub requests: i64,
}

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
/// `--accent-dark` from `assets/admin.css`, the cost bars.
const COST_FILL: &str = "#1e50c8";
/// `--border` from `assets/admin.css`, the axis rules.
const BORDER_STROKE: &str = "#d9dde3";

/// Accessible name of the tokens chart.
const TOKENS_LABEL: &str = "Tokens per day";
/// Accessible name of the cost chart.
const COST_LABEL: &str = "Cost per day";
/// Note rendered when a series maps to no bar.
const EMPTY_NOTE: &str = "No usage in this period";

/// Which pair of values a chart plots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChartKind {
    /// Input and output tokens as two stacked segments of one bar.
    Stacked,
    /// One bar per day from a caller-priced cost series.
    Single,
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
    push_axes(&mut svg, max, kind);
    for bar in bars {
        push_bar(&mut svg, bar, kind, scale);
    }
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
fn push_axes(svg: &mut String, max: i64, kind: ChartKind) {
    let right = PAD_LEFT + PLOT_W;
    let _ = write!(
        svg,
        "<line x1=\"{PAD_LEFT:.2}\" y1=\"{PAD_TOP:.2}\" x2=\"{right:.2}\" y2=\"{PAD_TOP:.2}\" \
         stroke=\"{BORDER_STROKE}\" stroke-width=\"1\"/>",
    );
    push_baseline(svg);
    let label = match kind {
        ChartKind::Stacked => format!("max {} tokens", compact_count(max)),
        ChartKind::Single => format!("max {}", compact_usd(max)),
    };
    let _ = write!(
        svg,
        "<text x=\"{PAD_LEFT:.2}\" y=\"{:.2}\" text-anchor=\"start\" font-size=\"11\" \
         fill=\"{MUTED_FILL}\">{label}</text>",
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

    /// A row as the SQL aggregate would hand it over.
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
