//! Product-specific content builders for modal text dialogs (`/help`, `/skills`,
//! `/stats`, and `/buddy`).

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use solaris_tui::theme::Theme;
use solaris_tui::util::{display_width, pad_to_width};

use crate::tui::buddy::{self, Companion};

/// Keyboard shortcuts shown in the help dialog.
pub const KEY_HELP: &[(&str, &str)] = &[
    ("enter", "send the prompt — or run the command under `/`"),
    ("alt+enter", "insert a newline"),
    (
        "tab",
        "cycle mode (default / plan / auto / ask) — or fill in the `/` command",
    ),
    ("ctrl+t", "cycle the theme"),
    ("ctrl+o", "toggle thinking blocks"),
    ("ctrl+l", "clear the transcript"),
    ("page up / down", "scroll the transcript"),
    (
        "← / →",
        "open / switch subagent terminals (when the prompt is empty)",
    ),
    ("esc", "return from a subagent terminal"),
    ("drag", "select any text"),
    ("ctrl+c", "copy the selection, or stop the turn"),
    ("ctrl+c again", "quit"),
    ("ctrl+d", "quit, when the prompt is empty"),
    ("ctrl+v", "paste from the clipboard"),
    ("?", "this help (when the prompt is empty)"),
];

/// Build the help dialog contents.
pub fn help_lines(
    theme: &Theme,
    commands: &[crate::tui::commands::SlashCommandSpec],
) -> Vec<Line<'static>> {
    let heading = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(theme.accent);
    let text_style = Style::default().fg(theme.fg);
    let muted = Style::default().fg(theme.muted);

    let mut lines = vec![
        Line::from(Span::styled("Keyboard", heading)),
        Line::default(),
    ];
    let width = KEY_HELP
        .iter()
        .map(|(key, _)| display_width(key))
        .max()
        .unwrap_or(0);
    for (key, description) in KEY_HELP {
        let padding = " ".repeat(width - display_width(key) + 2);
        lines.push(Line::from(vec![
            Span::styled(format!("  {key}"), key_style),
            Span::styled(padding, text_style),
            Span::styled(*description, text_style),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Commands", heading)));
    lines.push(Line::default());
    // Built first, so the column is measured from the text that is really drawn:
    // a name ending in `:` runs on into its argument rather than being followed
    // by a space, which is what makes `/skill:<name>` read as one word.
    let invocations: Vec<String> = commands
        .iter()
        .map(
            |spec| match (spec.args.is_empty(), spec.name.ends_with(':')) {
                (true, _) => format!("/{}", spec.name),
                (false, true) => format!("/{}{}", spec.name, spec.args),
                (false, false) => format!("/{} {}", spec.name, spec.args),
            },
        )
        .collect();
    let name_width = invocations
        .iter()
        .map(|invocation| display_width(invocation))
        .max()
        .unwrap_or(0);

    for (invocation, spec) in invocations.iter().zip(commands) {
        let padding = " ".repeat(name_width.saturating_sub(display_width(invocation)) + 2);
        lines.push(Line::from(vec![
            Span::styled(format!("  {invocation}"), key_style),
            Span::styled(padding, muted),
            Span::styled(spec.description, text_style),
        ]));
    }

    lines
}

/// Build the statistics dialog contents.
pub fn stats_lines(
    theme: &Theme,
    session: &crate::tui::state::SessionState,
    model: &str,
    backend: &str,
    mode: solaris_agent::Mode,
    context_window: u64,
    price_known: Option<bool>,
) -> Vec<Line<'static>> {
    let heading = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme.muted);
    let value = Style::default().fg(theme.fg);

    let tokens = session.total_tokens();
    let usage = session.total_usage();
    let context = session.context_tokens();
    let context_window = if session.context.window > 0 {
        u64::from(session.context.window)
    } else {
        context_window
    };
    let used_ratio = if context_window == 0 {
        0.0
    } else {
        f64::from(context.unwrap_or(0)) / context_window as f64
    };

    let row = |name: &str, value_text: String| {
        Line::from(vec![
            Span::styled(format!("  {name:<16}"), label),
            Span::styled(value_text, value),
        ])
    };

    let mut lines = vec![
        Line::from(Span::styled("Session", heading)),
        Line::default(),
        row("backend", backend.to_string()),
        row("model", model.to_string()),
        row("mode", mode.label().to_string()),
        row("turns", session.turns.len().to_string()),
        row("session tokens", tokens.to_string()),
        row("input", usage.input_tokens.to_string()),
        row("output", usage.output_tokens.to_string()),
        row("cache read", usage.cache_read_tokens.to_string()),
        row("cache write", usage.cache_write_tokens.to_string()),
        row(
            "cost",
            if let Some(totals) = session.billing {
                if totals.cost_complete {
                    format!("${:.4}", totals.cost_usd)
                } else {
                    format!("${:.4} (partial; total unknown)", totals.cost_usd)
                }
            } else if price_known == Some(false) {
                "unknown (pricing unavailable)".to_string()
            } else {
                "unknown (no settled request)".to_string()
            },
        ),
        row("context window", context_window.to_string()),
        row(
            "context used",
            match context {
                Some(tokens) => format!(
                    "{}{:.1}% ({tokens} tok)",
                    if session.context.estimated { "~" } else { "" },
                    used_ratio * 100.0
                ),
                None => "unknown".into(),
            },
        ),
        Line::default(),
        Line::from(Span::styled(
            "  Token and cost figures come from the active backend.",
            label,
        )),
        Line::from(Span::styled(
            "  Costs come from SDK responses; your final bill may differ.",
            label,
        )),
    ];

    if usage.estimated {
        lines.push(Line::from(Span::styled(
            "  ≈ a provider reported no usage, so part of this is an estimate.",
            label,
        )));
    }

    lines
}

/// Lines for the `/buddy` card: the sprite with its name and face, then every
/// trait.
pub fn buddy_lines(companion: &Companion, theme: &Theme) -> Vec<Line<'static>> {
    let heading = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme.muted);
    let value = Style::default().fg(theme.fg);
    let accent = Style::default().fg(theme.accent);

    let sprite = buddy::render_lines(&companion.bones, 0);
    let sprite_width = sprite
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0)
        + 4;

    let mut lines = Vec::new();
    for (index, row) in sprite.iter().enumerate() {
        let mut spans = vec![Span::styled(
            pad_to_width(&format!("  {row}"), sprite_width),
            accent,
        )];
        match index {
            0 => spans.push(Span::styled(companion.display_name().to_string(), heading)),
            1 => spans.push(Span::styled(buddy::render_face(&companion.bones), accent)),
            _ => {}
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());

    // Everything but the trait summary, which is split over two rows so it fits
    // the card's width instead of being cut mid-value.
    let mut card = companion.card().into_iter().skip(1);
    for _ in 0..5 {
        if let Some((name, text)) = card.next() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<10}", format!("{name}:")), label),
                Span::styled(text, value),
            ]));
        }
    }

    let traits: Vec<String> = companion
        .bones
        .stats
        .rows()
        .iter()
        .map(|(name, value)| format!("{name} {value}"))
        .collect();
    for (index, chunk) in traits.chunks(3).enumerate() {
        let name = if index == 0 { "stats:" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<10}"), label),
            Span::styled(chunk.join(" · "), value),
        ]));
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_lines_mention_every_command() {
        let theme = Theme::dark();
        let lines = help_lines(&theme, crate::tui::commands::PROMPT_SLASH_COMMANDS);
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        for spec in crate::tui::commands::PROMPT_SLASH_COMMANDS {
            assert!(
                text.contains(&format!("/{}", spec.name)),
                "missing {}",
                spec.name
            );
        }
        assert!(text.contains("ctrl+t"));
        assert!(!text.contains("ctrl+k"));
    }

    #[test]
    fn stats_lines_report_totals() {
        let theme = Theme::dark();
        let mut session = crate::tui::state::SessionState::new();
        session.context = solaris_agent::ContextUsage {
            tokens: Some(150),
            window: 1000,
            estimated: false,
        };
        session.turns.push(crate::tui::state::Turn {
            usage: solaris_agent::Usage::new(120, 30),
            cost_usd: 0.01,
            complete: true,
            ..Default::default()
        });

        let lines = stats_lines(
            &theme,
            &session,
            "claude-sonnet-4-5",
            "anthropic",
            solaris_agent::Mode::Default,
            1000,
            None,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("anthropic"));
        assert!(text.contains("claude-sonnet-4-5"));
        assert!(text.contains("150"), "the buckets are summed: {text}");
        assert!(text.contains("120"));
        assert!(text.contains("DEFAULT"));
        assert!(text.contains("15.0%"));
        // A measured session makes no apology for its numbers.
        assert!(!text.contains('≈'), "{text}");
    }

    /// Flatten rendered lines back into text, for assertions.
    fn stats_text(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_context_gauge_measures_the_window_not_the_session() {
        let theme = Theme::dark();
        let mut session = crate::tui::state::SessionState::new();
        session.context = solaris_agent::ContextUsage {
            tokens: Some(150),
            window: 1000,
            estimated: false,
        };
        for _ in 0..3 {
            session.turns.push(crate::tui::state::Turn {
                prompt: "hi".to_string(),
                usage: solaris_agent::Usage::new(100, 50),
                complete: true,
                ..Default::default()
            });
        }

        let text = stats_text(&stats_lines(
            &theme,
            &session,
            "claude-sonnet-4-5",
            "anthropic",
            solaris_agent::Mode::Default,
            1000,
            None,
        ));

        // Cumulative billing and the independently measured request have different sizes.
        assert!(text.contains("450"), "the session total: {text}");
        assert!(text.contains("15.0% (150 tok)"), "the context: {text}");
    }

    #[test]
    fn stats_lines_admit_an_estimated_turn() {
        let theme = Theme::dark();
        let mut session = crate::tui::state::SessionState::new();
        session.turns.push(crate::tui::state::Turn {
            usage: solaris_agent::Usage {
                estimated: true,
                ..solaris_agent::Usage::new(100, 100)
            },
            ..Default::default()
        });

        let lines = stats_lines(
            &theme,
            &session,
            "llama3.2",
            "local",
            solaris_agent::Mode::Plan,
            1000,
            None,
        );
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains('≈'), "{text}");
    }
}
