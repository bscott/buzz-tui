//! The conversation rail.
//!
//! Two sections, groups and direct messages, split by a rule. Selection and
//! "currently open" are different states with different backgrounds, and focus
//! lives on the separator column rather than on a border, so the rail never
//! grows chrome just to say where the cursor is.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{text, widgets};
use crate::app::{App, Focus};
use crate::model::{Channel, ChannelKind};

/// One rendered row, resolved before drawing so scrolling can work on rows
/// rather than on a mix of headings and entries.
enum Row<'a> {
    Heading(&'static str, usize),
    Channel(&'a Channel),
    Empty(&'static str),
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width < 4 || area.height == 0 {
        return;
    }
    let focused = app.focus == Focus::Sidebar;
    // The last column belongs to the separator.
    let content = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    let groups: Vec<&Channel> = app
        .channels
        .iter()
        .filter(|c| c.kind == ChannelKind::Group)
        .collect();
    let directs: Vec<&Channel> = app
        .channels
        .iter()
        .filter(|c| c.kind == ChannelKind::Direct)
        .collect();

    let mut rows: Vec<Row> = Vec::with_capacity(app.channels.len() + 4);
    rows.push(Row::Heading("channels", groups.len()));
    if groups.is_empty() {
        rows.push(Row::Empty("no channels yet"));
    }
    rows.extend(groups.iter().copied().map(Row::Channel));
    if !directs.is_empty() {
        rows.push(Row::Heading("direct", directs.len()));
        rows.extend(directs.iter().copied().map(Row::Channel));
    }

    // Keep the cursor on screen without letting the list jump around.
    let cursor_row = rows
        .iter()
        .position(|row| match row {
            Row::Channel(channel) => {
                app.channels.get(app.sidebar_cursor).map(|c| &c.id) == Some(&channel.id)
            }
            _ => false,
        })
        .unwrap_or(0);
    let height = content.height as usize;
    let offset = cursor_row.saturating_sub(height.saturating_sub(2)).min(
        rows.len().saturating_sub(height.min(rows.len())),
    );

    let palette = &app.palette;
    let budget = content.width.saturating_sub(2) as usize;
    let mut lines: Vec<Line> = Vec::with_capacity(height);
    // Row indices of section headings, so a rule can be drawn above each one
    // after the text has been laid out.
    let mut headings: Vec<(u16, &str)> = Vec::new();

    for row in rows.iter().skip(offset).take(height) {
        match row {
            Row::Heading(label, count) => {
                headings.push((lines.len() as u16, *label));
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {label}"),
                        Style::new()
                            .fg(palette.overlay0)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(format!("  {count}"), palette.pending()),
                ]));
            }
            Row::Empty(label) => {
                lines.push(Line::from(Span::styled(format!("   {label}"), palette.pending())));
            }
            Row::Channel(channel) => {
                lines.push(channel_line(app, channel, budget));
            }
        }
    }

    frame.render_widget(Paragraph::new(lines), content);
    for (row, label) in headings {
        if row == 0 || label != "direct" {
            continue;
        }
        widgets::rule(
            frame,
            Rect {
                y: content.y + row - 1,
                height: 1,
                ..content
            },
            palette,
        );
    }
    widgets::separator(frame, area, palette, focused);
    if rows.len() > height {
        widgets::scrollbar(
            frame,
            Rect {
                width: content.width,
                ..content
            },
            palette,
            (rows.len() - height - offset) as u16,
            rows.len() as u16,
            focused,
        );
    }
}

/// Renders one conversation. Three visual states are distinguished on purpose:
/// the keyboard cursor, the channel actually open, and everything else.
fn channel_line<'a>(app: &'a App, channel: &'a Channel, budget: usize) -> Line<'a> {
    let palette = &app.palette;
    let is_open = app.active.as_deref() == Some(channel.id.as_str());
    let is_cursor = app
        .channels
        .get(app.sidebar_cursor)
        .is_some_and(|c| c.id == channel.id);

    let base = match (is_cursor, is_open) {
        (true, _) => palette.selected(),
        (false, true) => palette.active(),
        (false, false) => Style::new().fg(palette.subtext0),
    };

    // Unread is carried by weight and a count, never by colour alone, so it
    // still reads on a monochrome terminal.
    let name_style = if channel.unread > 0 && !is_cursor {
        base.add_modifier(Modifier::BOLD).fg(palette.text)
    } else {
        base
    };

    let badge = if channel.mentions > 0 {
        Some(Span::styled(
            format!(" {} ", channel.mentions),
            Style::new()
                .bg(palette.red)
                .fg(palette.contrast_fg())
                .add_modifier(Modifier::BOLD),
        ))
    } else if channel.unread > 0 {
        Some(Span::styled(
            format!(" {} ", compact_count(channel.unread)),
            Style::new().fg(palette.accent).add_modifier(Modifier::BOLD),
        ))
    } else if channel.muted {
        Some(Span::styled(" \u{1f507}", palette.pending()))
    } else {
        None
    };

    let badge_width = badge.as_ref().map_or(0, |span| text::width(&span.content));
    let name_budget = budget.saturating_sub(badge_width + 2);
    let name = text::truncate_end(&channel.name, name_budget).into_owned();

    let mut spans = vec![
        Span::styled(if channel.pinned { "\u{25c6}" } else { " " }, palette.pending()),
        Span::styled(format!("{} ", channel.sigil()), base.fg(palette.overlay1)),
        Span::styled(text::pad_to(&name, name_budget), name_style),
    ];
    if let Some(badge) = badge {
        spans.push(badge);
    }
    Line::from(spans).style(if is_cursor || is_open {
        base
    } else {
        Style::new()
    })
}

/// Keeps a busy channel from widening the rail: anything past 99 is "99+".
fn compact_count(count: u32) -> String {
    if count > 99 {
        "99+".to_string()
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::compact_count;

    #[test]
    fn unread_counts_stay_narrow() {
        assert_eq!(compact_count(1), "1");
        assert_eq!(compact_count(99), "99");
        assert_eq!(compact_count(1000), "99+");
    }
}
