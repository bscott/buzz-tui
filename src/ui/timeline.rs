//! The conversation.
//!
//! Messages are laid out into rows once per frame and then sliced by the scroll
//! offset. Doing the layout eagerly is what lets the scrollbar, the page keys,
//! and inline images all agree about how tall the conversation is; measuring
//! separately from drawing is how timelines end up scrolling past their own end.

use chrono::{DateTime, Datelike, Local, TimeZone};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui_image::{Resize, StatefulImage};

use super::text::{self, Segment};
use super::theme::Palette;
use super::widgets;
use crate::app::{App, Focus};
use crate::media::Status;
use crate::model::{Delivery, Message};

/// Messages from the same author within this many seconds share a header.
const GROUP_WINDOW: i64 = 300;

/// A laid-out row. Images reserve their rows here so that everything below them
/// scrolls by the right amount whether or not the picture can be drawn yet.
enum Row {
    Text(Line<'static>),
    /// The first row of an image block.
    ImageTop { url: String, rows: u16 },
    /// A continuation row of an image block. It carries nothing because the
    /// picture is drawn from its top row; this only reserves the space.
    ImageBody,
}

pub fn render(frame: &mut Frame, area: Rect, app: &mut App) {
    if area.width < 8 || area.height == 0 {
        return;
    }
    let focused = app.focus == Focus::Timeline;
    // Leave the last column for the scrollbar so text never sits under it.
    let content = Rect {
        width: area.width.saturating_sub(1),
        ..area
    };

    // A relay that refuses your key leaves nothing to show, and "no channel
    // open" would blame the wrong thing. Name what happened and hand over the
    // exact request an administrator needs.
    if app.membership_rejected().is_some() {
        let npub = app.npub();
        widgets::empty_state(
            frame,
            content,
            &app.palette,
            "this relay does not know your key",
            "a buzz community admits only keys its operator has added, and yours \
             is not on the list yet",
            &[
                (
                    &app.keymap.hint(crate::keys::Action::CopyIdentity),
                    "copy a request to send them",
                ),
                (&npub, "your public key"),
            ],
        );
        return;
    }

    if app.active.is_none() {
        widgets::empty_state(
            frame,
            content,
            &app.palette,
            "no channel open",
            "a channel is one room in this community; everything in it is a signed event",
            &[
                (
                    &app.keymap.hint(crate::keys::Action::OpenSwitcher),
                    "jump to a channel",
                ),
                (
                    &app.keymap.hint(crate::keys::Action::CreateChannel),
                    "create one",
                ),
            ],
        );
        return;
    }

    let rows = layout(app, content.width as usize);
    let total = rows.len() as u16;
    app.viewport.timeline_rows = content.height;
    app.viewport.timeline_content = total;

    if rows.is_empty() {
        let joined = app.active_channel().is_some_and(|c| c.joined);
        widgets::empty_state(
            frame,
            content,
            &app.palette,
            "no messages yet",
            if joined {
                "this channel is empty; anything you send here is signed with your key"
            } else {
                "you are not a member yet, so history may be hidden"
            },
            &[
                (&app.keymap.hint(crate::keys::Action::FocusComposer), "write something"),
                (&app.keymap.hint(crate::keys::Action::JoinChannel), "join the channel"),
            ],
        );
        return;
    }

    // The offset counts rows scrolled up from the newest message, which keeps a
    // live conversation pinned to the bottom as it grows. A conversation
    // shorter than the pane is padded at the top for the same reason: chat
    // grows upward from the composer, it does not hang from the header.
    let height = content.height;
    let max_offset = total.saturating_sub(height);
    let offset = app.scroll.min(max_offset);
    let start = max_offset.saturating_sub(offset) as usize;
    let pad = height.saturating_sub(total);

    let mut lines: Vec<Line> = vec![Line::from(""); pad as usize];
    let mut images: Vec<(String, Rect)> = Vec::new();

    for (index, row) in rows
        .into_iter()
        .skip(start)
        .take(height.saturating_sub(pad) as usize)
        .enumerate()
    {
        let y = content.y + pad + index as u16;
        let remaining_rows = content.bottom().saturating_sub(y);
        // Every row contributes exactly one line. An image reserves its height
        // as separate `ImageBody` rows, so emitting more than one line here
        // would push the newest messages off the bottom of the pane.
        match row {
            Row::Text(line) => lines.push(line),
            Row::ImageTop { url, rows } => {
                if rows <= remaining_rows {
                    images.push((
                        url,
                        Rect {
                            x: content.x + 2,
                            y,
                            width: content.width.saturating_sub(3),
                            height: rows,
                        },
                    ));
                    lines.push(Line::from(""));
                } else {
                    // Drawing a partly-visible image would force a re-encode on
                    // every scroll step, so say what is there instead.
                    lines.push(placeholder(&app.palette, rows));
                }
            }
            // The top of this block scrolled off, or it is holding space under
            // an image that is being drawn over these cells.
            Row::ImageBody => lines.push(Line::from("")),
        }
        if lines.len() >= height as usize {
            break;
        }
    }

    frame.render_widget(Paragraph::new(lines), content);

    for (url, rect) in images {
        if !app.show_images {
            continue;
        }
        app.media.request(&url);
        let palette_dim = app.palette.dim();
        match app.media.status(&url) {
            Status::Ready(protocol) => {
                frame.render_stateful_widget(
                    StatefulImage::default().resize(Resize::Fit(None)),
                    rect,
                    protocol,
                );
            }
            Status::Loading => {
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled("\u{25a3} loading image", palette_dim))),
                    Rect { height: 1, ..rect },
                );
            }
            Status::Failed(reason) => {
                let text = format!("\u{25a3} image unavailable: {reason}");
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(text, palette_dim))),
                    Rect { height: 1, ..rect },
                );
            }
        }
    }

    widgets::scrollbar(frame, area, &app.palette, offset, total, focused);
}

fn placeholder(palette: &Palette, rows: u16) -> Line<'static> {
    Line::from(Span::styled(
        format!("\u{25a3} image \u{00b7} {rows} rows, scroll to view"),
        palette.dim(),
    ))
}

/// Turns the loaded messages into rows.
fn layout(app: &App, width: usize) -> Vec<Row> {
    let palette = &app.palette;
    let body_width = width.saturating_sub(2);
    let mut rows: Vec<Row> = Vec::new();
    let mut previous: Option<&Message> = None;
    let mut last_day: Option<i32> = None;

    let first_unread = first_unread_index(app);

    for (index, message) in app.timeline.iter().enumerate() {
        let selected = app.selected == Some(index);
        let when = Local.timestamp_opt(message.created_at, 0).single();

        // Day separators orient a long scrollback without a date on every line.
        if let Some(when) = when {
            let day = when.ordinal() as i32 + when.year() * 1000;
            if last_day != Some(day) {
                last_day = Some(day);
                rows.push(Row::Text(separator_line(palette, &format_day(&when), width)));
            }
        }

        if first_unread == Some(index) {
            rows.push(Row::Text(unread_line(palette, width)));
        }

        let grouped = previous.is_some_and(|prev| {
            prev.author == message.author
                && message.created_at - prev.created_at < GROUP_WINDOW
                && message.parent.is_none()
        });

        // A reply opens its own group so the quote always sits directly above
        // the author line it belongs to, never orphaned above a blank row.
        if !grouped && !rows.is_empty() && !app.compact {
            rows.push(Row::Text(Line::from("")));
        }
        if let Some(parent) = message.parent.as_deref() {
            rows.push(Row::Text(reply_line(app, parent, body_width, selected)));
        }
        if !grouped {
            rows.push(Row::Text(author_line(app, message, when, selected)));
        }

        rows.extend(body_rows(app, message, body_width, selected));

        if !message.reactions.is_empty() {
            rows.push(Row::Text(reaction_line(app, message, selected)));
        }

        if app.show_images {
            for url in text::image_links(&message.body) {
                let max = app.config.media.max_rows;
                let reserved = app.media.rows_for(url, body_width as u16, max);
                let reserved = if reserved == 0 { 3 } else { reserved };
                rows.push(Row::ImageTop {
                    url: url.to_string(),
                    rows: reserved,
                });
                for _ in 1..reserved {
                    rows.push(Row::ImageBody);
                }
            }
        }

        previous = Some(message);
    }

    rows
}

/// Index of the first message newer than the read marker, so the divider lands
/// where the eye should resume rather than at the top of the loaded page.
fn first_unread_index(app: &App) -> Option<usize> {
    let channel = app.active_channel()?;
    if channel.unread == 0 {
        return None;
    }
    let unread = channel.unread as usize;
    app.timeline.len().checked_sub(unread)
}

fn separator_line(palette: &Palette, label: &str, width: usize) -> Line<'static> {
    let label = format!(" {label} ");
    let bar = width.saturating_sub(text::width(&label) + 4);
    Line::from(vec![
        Span::styled("  ".to_string(), palette.dim()),
        Span::styled(label, palette.dim()),
        Span::styled("\u{2500}".repeat(bar), Style::new().fg(palette.surface_dim)),
    ])
}

fn unread_line(palette: &Palette, width: usize) -> Line<'static> {
    let label = " new ";
    let bar = width.saturating_sub(label.len() + 4);
    Line::from(vec![
        Span::styled("  ", palette.error()),
        Span::styled(
            label,
            Style::new().fg(palette.red).add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2500}".repeat(bar), Style::new().fg(palette.red)),
    ])
}

fn author_line(
    app: &App,
    message: &Message,
    when: Option<DateTime<Local>>,
    selected: bool,
) -> Line<'static> {
    let palette = &app.palette;
    let name = app.display_name(&message.author);
    // Colouring a name by its key makes the same person the same colour in every
    // channel, without needing a profile.
    let colour = author_colour(palette, &message.author);
    let mut spans = vec![Span::styled(
        if selected { "\u{2503} " } else { "  " },
        palette.accent_text(),
    )];

    spans.push(Span::styled(
        name,
        Style::new().fg(colour).add_modifier(Modifier::BOLD),
    ));
    if app.is_me(&message.author) {
        spans.push(Span::styled(" you", palette.pending()));
    }
    if let Some(when) = when.filter(|_| app.show_timestamps) {
        let format = if app.config.ui.clock_24h {
            "%H:%M"
        } else {
            "%-I:%M %p"
        };
        spans.push(Span::styled(
            format!("  {}", when.format(format)),
            palette.pending(),
        ));
    }
    match message.delivery {
        Some(Delivery::Sending) => spans.push(Span::styled("  sending", palette.pending())),
        Some(Delivery::Failed) => {
            spans.push(Span::styled("  \u{00d7} failed", palette.error()));
            if let Some(error) = &message.error {
                spans.push(Span::styled(format!(" \u{00b7} {error}"), palette.dim()));
            }
        }
        _ => {}
    }
    if message.edited {
        spans.push(Span::styled("  edited", palette.pending()));
    }

    let mut line = Line::from(spans);
    if selected {
        line = line.style(palette.selected());
    }
    line
}

/// A stable colour per author, drawn from the palette's accent row so that it
/// always sits inside the active theme.
fn author_colour(palette: &Palette, pubkey: &str) -> ratatui::style::Color {
    let choices = [
        palette.accent,
        palette.mauve,
        palette.green,
        palette.teal,
        palette.peach,
        palette.yellow,
        palette.blue,
    ];
    let hash = pubkey
        .bytes()
        .fold(0u32, |acc, byte| acc.wrapping_mul(31).wrapping_add(byte as u32));
    choices[(hash as usize) % choices.len()]
}

fn body_rows(app: &App, message: &Message, width: usize, selected: bool) -> Vec<Row> {
    let palette = &app.palette;
    if message.deleted {
        let mut line = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "message deleted",
                Style::new()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::ITALIC),
            ),
        ]);
        if selected {
            line = line.style(palette.selected());
        }
        return vec![Row::Text(line)];
    }

    let dimmed = message.is_pending();
    text::wrap(&message.body, width)
        .into_iter()
        .map(|raw| {
            let mut spans = vec![Span::raw("  ")];
            for segment in text::segments(&raw) {
                let (content, style) = match segment {
                    Segment::Text(t) => (t.to_string(), palette.base()),
                    Segment::Code(t) => (t.to_string(), palette.code()),
                    Segment::Link(t) => (t.to_string(), palette.link()),
                    Segment::Mention(t) => {
                        let style = if t.contains(&app.me[..8.min(app.me.len())]) {
                            palette.mention()
                        } else {
                            palette.accent_text()
                        };
                        (t.to_string(), style)
                    }
                };
                spans.push(Span::styled(content, if dimmed { palette.pending() } else { style }));
            }
            let mut line = Line::from(spans);
            if selected {
                line = line.style(palette.selected());
            }
            Row::Text(line)
        })
        .collect()
}

fn reply_line(app: &App, parent: &str, width: usize, selected: bool) -> Line<'static> {
    let palette = &app.palette;
    let quoted = app
        .message(parent)
        .map(|m| {
            format!(
                "{}: {}",
                app.display_name(&m.author),
                m.body.replace('\n', " ")
            )
        })
        .unwrap_or_else(|| "a message above".to_string());

    let mut line = Line::from(vec![
        Span::styled("  \u{250c} ", Style::new().fg(palette.surface1)),
        Span::styled(
            text::truncate_end(&quoted, width.saturating_sub(4)).into_owned(),
            palette.quote(),
        ),
    ]);
    if selected {
        line = line.style(palette.selected());
    }
    line
}

fn reaction_line(app: &App, message: &Message, selected: bool) -> Line<'static> {
    let palette = &app.palette;
    let mut spans = vec![Span::raw("  ")];
    for reaction in &message.reactions {
        // Reactions you are part of are outlined, so you can see your own vote.
        let style = if reaction.mine {
            Style::new()
                .fg(palette.accent)
                .bg(palette.surface_dim)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(palette.subtext0).bg(palette.surface_dim)
        };
        spans.push(Span::styled(
            format!(" {} {} ", reaction.emoji, reaction.count),
            style,
        ));
        spans.push(Span::raw(" "));
    }
    let mut line = Line::from(spans);
    if selected {
        line = line.style(palette.selected());
    }
    line
}

fn format_day(when: &DateTime<Local>) -> String {
    let today = Local::now();
    let days = today.date_naive().signed_duration_since(when.date_naive()).num_days();
    match days {
        0 => "today".to_string(),
        1 => "yesterday".to_string(),
        2..=6 => when.format("%A").to_string().to_lowercase(),
        _ => when.format("%-d %B %Y").to_string().to_lowercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Palette;

    #[test]
    fn authors_keep_a_stable_colour() {
        let palette = Palette::catppuccin();
        let a = author_colour(&palette, "abc123");
        let b = author_colour(&palette, "abc123");
        assert_eq!(a, b, "the same key must always render the same colour");
    }

    #[test]
    fn day_labels_are_relative_for_the_recent_past() {
        let now = Local::now();
        assert_eq!(format_day(&now), "today");
        assert_eq!(format_day(&(now - chrono::Duration::days(1))), "yesterday");
        // A week back falls through to an absolute date rather than a weekday,
        // which would be ambiguous.
        let old = now - chrono::Duration::days(30);
        assert!(format_day(&old).contains(&old.format("%Y").to_string()));
    }
}
