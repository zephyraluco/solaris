//! The welcome box shown while the transcript is empty.
//!
//! Ported from claurst's two-column welcome screen (`render_welcome_box`) with
//! its compact fallback (`welcome_banner_lines`): a rounded, accent-coloured
//! frame whose title carries the version, a divider separating the columns, the
//! companion on the left under a greeting, and tips plus recent activity on the
//! right.
//!
//! Like [`crate::components::markdown`] this is a line builder rather than a
//! [`Component`](crate::component::Component): the box lives inside the
//! transcript, so it scrolls away with the conversation instead of occupying a
//! fixed header.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::theme::Theme;
use crate::util::{display_width, pad_to_width, truncate_to_width, wrap_text};

/// Narrowest box that still fits two readable columns.
pub const MIN_WIDTH: u16 = 34;
/// The box is a fixed seven content rows plus its borders.
pub const BOX_HEIGHT: u16 = 9;

/// Content rows available between the borders.
const CONTENT_ROWS: usize = BOX_HEIGHT as usize - 2;
/// Rows the tip may take before it is cut short, so recent activity never gets
/// pushed out of the box.
const MAX_TIP_ROWS: usize = 2;
/// Rows kept for recent activity.
const MAX_ACTIVITY_ROWS: usize = 2;
/// Cells the frame spends on padding and the column divider: `"│ "`, `"│"`,
/// `" "`, `" "` and `"│"` — everything except the two column widths.
const CHROME: usize = 6;

/// One line of the "Recent activity" list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WelcomeEntry {
    /// What was done, already trimmed to a single line.
    pub label: String,
    /// How long ago, already formatted ("5m ago").
    pub when: String,
}

/// Everything the box displays, free of styling so the app can build it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WelcomeData {
    /// Program name in the title, e.g. `solaris`.
    pub app_name: String,
    /// Version shown after the name.
    pub version: String,
    /// Greeting line, e.g. `Welcome back zeal!`.
    pub greeting: String,
    /// One-line hint, used by the compact fallback.
    pub hint: String,
    /// The companion's sprite rows for the current animation frame.
    pub mascot: Vec<String>,
    /// The tip shown under "Tips for getting started".
    pub tip: String,
    /// Recent activity, newest first.
    pub recent: Vec<WelcomeEntry>,
}

/// Colours used while drawing the box.
#[derive(Debug, Clone, Copy)]
pub struct WelcomeStyles {
    /// Frame, divider and the accent parts of the title.
    pub border: Style,
    /// Program name in the title.
    pub title: Style,
    /// Version in the title.
    pub version: Style,
    /// The greeting.
    pub greeting: Style,
    /// Section headings.
    pub heading: Style,
    /// Body text, such as the tip.
    pub body: Style,
    /// Recent activity rows and the empty placeholder.
    pub muted: Style,
    /// The companion sprite.
    pub mascot: Style,
}

impl WelcomeStyles {
    /// Derive the palette from a [`Theme`], reusing the accent the rest of the
    /// interface already draws with.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            border: Style::default().fg(theme.accent),
            title: Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
            version: Style::default().fg(theme.dim),
            greeting: Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            heading: Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
            body: Style::default().fg(theme.muted),
            muted: Style::default().fg(theme.dim),
            mascot: Style::default().fg(theme.accent),
        }
    }
}

/// A column row: the text plus the style it is drawn with.
type Row = (String, Style);

/// Render the welcome box for `area`, or a compact banner when it does not fit.
pub fn render_welcome(
    data: &WelcomeData,
    area: Rect,
    styles: &WelcomeStyles,
) -> Vec<Line<'static>> {
    if area.width < MIN_WIDTH || area.height < BOX_HEIGHT {
        return banner(data, area, styles);
    }

    let width = area.width as usize;
    let columns = width.saturating_sub(CHROME);
    let left_width = left_column_width(data, columns);
    let right_width = columns.saturating_sub(left_width);

    let mut lines = vec![title_line(data, width, styles)];
    let left = left_column(data, left_width, styles);
    let right = right_column(data, right_width, styles);

    for index in 0..CONTENT_ROWS {
        let left_row = left.get(index).cloned().unwrap_or_default();
        let right_row = right.get(index).cloned().unwrap_or_default();
        lines.push(content_row(
            &left_row,
            &right_row,
            left_width,
            right_width,
            styles,
        ));
    }

    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(width.saturating_sub(2))),
        styles.border,
    )));
    lines
}

/// The frame's top edge, with the program name and version embedded in it.
fn title_line(data: &WelcomeData, width: usize, styles: &WelcomeStyles) -> Line<'static> {
    // "╭─" + " name " + "vX.Y " + filler + "╮"
    let name = format!(" {} ", data.app_name);
    let version = format!("v{} ", data.version);
    let used = 2 + display_width(&name) + display_width(&version) + 1;
    let filler = "─".repeat(width.saturating_sub(used));

    Line::from(vec![
        Span::styled("╭─", styles.border),
        Span::styled(name, styles.title),
        Span::styled(version, styles.version),
        Span::styled(format!("{filler}╮"), styles.border),
    ])
}

/// One content row: both columns padded to their width, split by the divider.
fn content_row(
    left: &Row,
    right: &Row,
    left_width: usize,
    right_width: usize,
    styles: &WelcomeStyles,
) -> Line<'static> {
    let left_text = pad_to_width(&truncate_to_width(&left.0, left_width, "…"), left_width);
    let right_text = pad_to_width(&truncate_to_width(&right.0, right_width, "…"), right_width);

    Line::from(vec![
        Span::styled("│ ", styles.border),
        Span::styled(left_text, left.1),
        Span::styled("│", styles.border),
        Span::styled(format!(" {right_text} "), right.1),
        Span::styled("│", styles.border),
    ])
}

/// Left column width: the sprite is centred in it, so it has to fit the art,
/// and it never takes more than a third of a wide box.
fn left_column_width(data: &WelcomeData, columns: usize) -> usize {
    let mascot_width = data
        .mascot
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0);

    // `clamp` needs a valid range: on a very narrow box the upper bound wins.
    let upper = 32usize.min(columns.saturating_sub(3)).max(20);
    (mascot_width + 4).clamp(20, upper)
}

/// Greeting, a blank row, then the centred companion.
fn left_column(data: &WelcomeData, width: usize, styles: &WelcomeStyles) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![
        (data.greeting.clone(), styles.greeting),
        (String::new(), styles.body),
    ];

    let mascot_width = data
        .mascot
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0);
    let indent = width.saturating_sub(mascot_width) / 2;
    for row in &data.mascot {
        rows.push((format!("{}{row}", " ".repeat(indent)), styles.mascot));
    }

    rows
}

/// Tip, then recent activity.
fn right_column(data: &WelcomeData, width: usize, styles: &WelcomeStyles) -> Vec<Row> {
    let mut rows: Vec<Row> = vec![("Tips for getting started".to_string(), styles.heading)];
    rows.extend(
        wrap_text(&data.tip, width)
            .into_iter()
            .take(MAX_TIP_ROWS)
            .map(|line| (line, styles.body)),
    );
    rows.push((String::new(), styles.body));
    rows.push(("Recent activity".to_string(), styles.heading));

    if data.recent.is_empty() {
        rows.push(("No recent activity".to_string(), styles.muted));
    } else {
        for entry in data.recent.iter().take(MAX_ACTIVITY_ROWS) {
            let when = format!(" {}", entry.when);
            let label_width = width.saturating_sub(display_width(&when)).max(1);
            let label = pad_to_width(
                &truncate_to_width(&entry.label, label_width, "…"),
                label_width,
            );
            rows.push((format!("{label}{when}"), styles.muted));
        }
    }

    rows
}

/// The fallback used when the box does not fit: the title, then the greeting and
/// the hint, in as many rows as the area actually has.
fn banner(data: &WelcomeData, area: Rect, styles: &WelcomeStyles) -> Vec<Line<'static>> {
    let width = area.width as usize;
    let mut lines = vec![Line::from(vec![
        Span::styled(format!("{} ", data.app_name), styles.title),
        Span::styled(format!("v{}", data.version), styles.version),
    ])];

    if width < 20 {
        return lines;
    }
    if area.height >= 2 {
        lines.push(Line::from(Span::styled(
            data.greeting.clone(),
            styles.greeting,
        )));
    }
    if area.height >= 3 {
        lines.push(Line::from(Span::styled(data.hint.clone(), styles.body)));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data() -> WelcomeData {
        WelcomeData {
            app_name: "solaris".to_string(),
            version: "0.1.0".to_string(),
            greeting: "Welcome back zeal!".to_string(),
            hint: "/help for commands  ·  ? for shortcuts".to_string(),
            mascot: vec![
                "   /\\_/\\    ".to_string(),
                "  ( ·   ·)  ".to_string(),
                "  (  ω  )   ".to_string(),
                "  (\")_(\")   ".to_string(),
            ],
            tip: "Tab switches between build and plan mode.".to_string(),
            recent: Vec::new(),
        }
    }

    fn with_activity() -> WelcomeData {
        let mut data = data();
        data.recent = vec![
            WelcomeEntry {
                label: "fix the parser".to_string(),
                when: "5m ago".to_string(),
            },
            WelcomeEntry {
                label: "add the welcome box".to_string(),
                when: "2h ago".to_string(),
            },
        ];
        data
    }

    fn rendered(data: &WelcomeData, width: u16, height: u16) -> Vec<String> {
        let theme = Theme::dark();
        render_welcome(
            data,
            Rect::new(0, 0, width, height),
            &WelcomeStyles::from_theme(&theme),
        )
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<Vec<_>>()
                .join("")
        })
        .collect()
    }

    #[test]
    fn the_box_is_titled_two_columned_and_closed() {
        let lines = rendered(&data(), 80, BOX_HEIGHT);
        assert_eq!(lines.len(), BOX_HEIGHT as usize);

        assert!(
            lines[0].starts_with("╭─ solaris v0.1.0 ─"),
            "{:?}",
            lines[0]
        );
        assert!(lines[0].ends_with('╮'), "{:?}", lines[0]);
        assert!(lines[1].contains("Welcome back zeal!"), "{:?}", lines[1]);
        assert!(
            lines[1].contains('│'),
            "the divider is missing: {:?}",
            lines[1]
        );
        assert!(
            lines[1].contains("Tips for getting started"),
            "{:?}",
            lines[1]
        );
        assert!(lines.last().unwrap().starts_with('╰'), "{lines:?}");

        // Every content row is closed on both sides.
        for line in &lines[1..lines.len() - 1] {
            assert!(line.starts_with('│'), "the left edge is open: {line:?}");
            assert!(line.ends_with('│'), "the right edge is open: {line:?}");
        }
    }

    #[test]
    fn every_row_is_exactly_the_box_width() {
        for width in [MIN_WIDTH, 60, 80, 120, 200] {
            for data in [data(), with_activity()] {
                for line in rendered(&data, width, BOX_HEIGHT) {
                    assert_eq!(
                        display_width(&line),
                        width as usize,
                        "row {line:?} is not {width} cells"
                    );
                }
            }
        }
    }

    #[test]
    fn the_mascot_is_centred_in_the_left_column() {
        let data = data();
        let lines = rendered(&data, 80, BOX_HEIGHT);

        // Sprite rows start after the title, the greeting and its blank row,
        // indented by one shared amount so the art keeps its relative alignment.
        let mut paddings = Vec::new();
        for (art, line) in data.mascot.iter().zip(lines.iter().skip(3)) {
            assert!(line.contains(art.as_str()), "sprite row lost: {line:?}");

            let inside: String = line.chars().skip(2).collect();
            let leading = |text: &str| text.chars().count() - text.trim_start().chars().count();
            paddings.push(leading(&inside) - leading(art));
        }

        assert_eq!(paddings.len(), data.mascot.len());
        assert!(
            paddings.windows(2).all(|pair| pair[0] == pair[1]),
            "the sprite is not offset uniformly: {paddings:?}"
        );
        assert!(paddings[0] > 0, "the sprite hugs the frame: {paddings:?}");
    }

    #[test]
    fn recent_activity_lists_entries_newest_first() {
        let text = rendered(&with_activity(), 80, BOX_HEIGHT).join("\n");
        assert!(text.contains("Recent activity"), "{text}");
        let first = text.find("fix the parser").expect("newest entry");
        let second = text.find("add the welcome box").expect("older entry");
        assert!(first < second, "{text}");
        assert!(text.contains("5m ago"), "{text}");

        // And without entries the placeholder takes over.
        let empty = rendered(&data(), 80, BOX_HEIGHT).join("\n");
        assert!(empty.contains("No recent activity"), "{empty}");
    }

    #[test]
    fn the_tip_is_wrapped_and_keeps_the_activity_section() {
        let mut data = data();
        data.tip = "Start with small features or bug fixes, propose a plan, and verify the edits."
            .to_string();

        let text = rendered(&data, 80, BOX_HEIGHT).join("\n");
        assert!(text.contains("Start with small"), "{text}");
        assert!(text.contains("No recent activity"), "activity kept: {text}");
    }

    #[test]
    fn a_narrow_box_falls_back_to_a_banner() {
        let lines = rendered(&data(), MIN_WIDTH - 1, BOX_HEIGHT);
        assert!(lines[0].starts_with("solaris v0.1.0"), "{lines:?}");
        assert!(lines[1].contains("Welcome back"), "{lines:?}");
        assert!(lines[2].contains("/help for commands"), "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains('╭')), "{lines:?}");
        assert!(lines.len() < BOX_HEIGHT as usize, "{lines:?}");
    }

    #[test]
    fn a_short_area_falls_back_to_a_banner() {
        let lines = rendered(&data(), 80, BOX_HEIGHT - 1);
        assert!(lines[0].contains("solaris"), "{lines:?}");
        assert!(!lines.iter().any(|line| line.contains('╭')), "{lines:?}");
    }

    #[test]
    fn the_banner_never_exceeds_the_rows_it_has() {
        for height in 1..BOX_HEIGHT {
            let lines = rendered(&data(), 80, height);
            assert!(
                lines.len() <= height as usize,
                "height {height} drew {} rows: {lines:?}",
                lines.len()
            );
            assert!(lines[0].starts_with("solaris v0.1.0"), "{lines:?}");
        }

        // One row is just the title; two add the greeting; three the hint.
        assert_eq!(rendered(&data(), 80, 1).len(), 1);
        assert_eq!(rendered(&data(), 80, 2).len(), 2);
        assert_eq!(rendered(&data(), 80, 3).len(), 3);
    }

    #[test]
    fn long_values_are_truncated_instead_of_wrapping_the_row() {
        let mut data = data();
        data.tip = "x".repeat(200);
        data.greeting = "Welcome back someone-with-a-very-long-name!".to_string();
        data.recent = vec![WelcomeEntry {
            label: "y".repeat(200),
            when: "3d ago".to_string(),
        }];

        for line in rendered(&data, 60, BOX_HEIGHT) {
            assert_eq!(display_width(&line), 60, "{line:?}");
        }
    }

    #[test]
    fn an_empty_mascot_still_renders_a_valid_box() {
        let mut data = data();
        data.mascot.clear();

        let lines = rendered(&data, 60, BOX_HEIGHT);
        assert_eq!(lines.len(), BOX_HEIGHT as usize);
        assert!(
            lines.iter().all(|line| display_width(line) == 60),
            "{lines:?}"
        );
    }

    #[test]
    fn a_five_row_mascot_still_fits_the_box() {
        // A hatted companion uses the fifth row, which is exactly the room the
        // left column has (greeting + blank + five sprite rows).
        let mut data = data();
        data.mascot.insert(0, "    /^\\     ".to_string());

        let lines = rendered(&data, 80, BOX_HEIGHT);
        assert_eq!(lines.len(), BOX_HEIGHT as usize);
        // Content rows are greeting, blank, then the five sprite rows.
        assert!(lines[3].contains("/^\\"), "{lines:?}");
        assert!(lines[7].contains("(\")_(\")"), "{lines:?}");
        assert!(
            lines.iter().all(|line| display_width(line) == 80),
            "{lines:?}"
        );
        assert!(lines[1].contains("Tips for getting started"), "{lines:?}");
        assert!(lines[4].contains("Recent activity"), "{lines:?}");
    }
}
