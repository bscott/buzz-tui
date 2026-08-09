//! Rendering.
//!
//! One entry point, [`render`], draws a whole frame from the application state
//! and writes back only the viewport measurements the dispatcher needs. Nothing
//! here mutates domain state, so a frame can be dropped or repeated freely.

pub mod overlays;
pub mod sidebar;
pub mod text;
pub mod theme;
pub mod timeline;
pub mod widgets;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::app::{App, ComposeMode, Focus};
use crate::keys::{Action, Chord, Scope};
use crate::model::ChannelKind;
use crate::net::ConnState;

/// Below this width the sidebar and member list are folded away, because two
/// columns of chrome leave no room for the conversation itself.
const NARROW: u16 = 64;
/// The composer never grows past this, however long the draft gets.
const COMPOSER_MAX_ROWS: u16 = 8;

pub fn render(frame: &mut Frame, app: &mut App) {
    let screen = frame.area();
    if screen.width < 20 || screen.height < 6 {
        frame.render_widget(
            Paragraph::new("terminal too small").style(app.palette.dim()),
            screen,
        );
        return;
    }

    let narrow = screen.width < NARROW;
    let show_sidebar = app.show_sidebar && !narrow;
    let show_members = app.show_members && !narrow && screen.width >= 96;

    // The hint bar only claims a row when it has something to say, which is
    // what keeps the conversation as tall as the terminal allows.
    let hint = hint_bar(app);
    let hint_rows = u16::from(hint.is_some());

    let [body, hint_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(hint_rows)]).areas(screen);

    let sidebar_width = app.config.ui.sidebar_width.clamp(18, 36);
    let member_width = 24;
    let [rail, centre, members] = Layout::horizontal([
        Constraint::Length(if show_sidebar { sidebar_width } else { 0 }),
        Constraint::Fill(1),
        Constraint::Length(if show_members { member_width } else { 0 }),
    ])
    .areas(body);

    if show_sidebar {
        app.viewport.sidebar_rows = rail.height;
        sidebar::render(frame, rail, app);
    }
    if show_members {
        render_members(frame, members, app);
    }

    let composer_rows = composer_height(app, centre.width);
    let [header, conversation, compose] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Fill(1),
        Constraint::Length(composer_rows),
    ])
    .areas(centre);

    render_header(frame, header, app, narrow);
    timeline::render(frame, conversation, app);
    render_composer(frame, compose, app);

    if let Some(line) = hint {
        frame.render_widget(
            Paragraph::new(line).style(Style::new().bg(app.palette.panel_bg)),
            hint_area,
        );
    }

    // Diagnostics sit above toasts so a broken config is never hidden by a
    // transient notification.
    if !app.diagnostics.is_empty() {
        let rows = app.diagnostics.len().min(3) as u16;
        widgets::diagnostics_banner(
            frame,
            Rect {
                height: rows,
                ..conversation
            },
            &app.palette,
            &app.diagnostics,
        );
    }
    widgets::toasts(frame, conversation, &app.palette, &app.toasts);

    if let Some(overlay) = app.overlay.take() {
        overlays::render(frame, app, &overlay);
        app.overlay = Some(overlay);
    } else if app.focus == Focus::Composer {
        // The cursor belongs to the composer, and only when nothing covers it.
        let inner_width = compose.width.saturating_sub(4).max(1) as usize;
        let (column, row) = app.composer.cursor_cell(inner_width);
        let x = compose.x + 2 + column;
        let y = compose.y + 1 + row;
        if x < compose.right() && y < compose.bottom() {
            frame.set_cursor_position((x, y));
        }
    }
}

fn composer_height(app: &App, width: u16) -> u16 {
    let inner = width.saturating_sub(4).max(1) as usize;
    let rows = app.composer.height(inner).clamp(1, COMPOSER_MAX_ROWS);
    rows + 2
}

// ------------------------------------------------------------------- header

fn render_header(frame: &mut Frame, area: Rect, app: &App, narrow: bool) {
    let palette = &app.palette;
    let mut left: Vec<Span> = Vec::new();

    match app.active_channel() {
        Some(channel) => {
            left.push(Span::styled(
                format!(" {} ", channel.sigil()),
                palette.accent_text(),
            ));
            left.push(Span::styled(channel.name.clone(), palette.strong()));
            if let Some(about) = channel.about.as_deref().filter(|a| !a.is_empty()) {
                let budget = area.width.saturating_sub(28) as usize;
                left.push(Span::styled(
                    format!("  \u{00b7}  {}", text::truncate_end(about, budget)),
                    palette.dim(),
                ));
            }
            if channel.kind == ChannelKind::Direct {
                left.push(Span::styled("  encrypted", palette.pending()));
            }
        }
        None => left.push(Span::styled(" buzz", palette.strong())),
    }

    let typing = app.typing_now();
    if !typing.is_empty() {
        let who = match typing.len() {
            1 => format!("{} is typing", typing[0]),
            2 => format!("{} and {} are typing", typing[0], typing[1]),
            n => format!("{n} people are typing"),
        };
        left.push(Span::styled(
            format!("  \u{00b7}  {who}\u{2026}"),
            palette.accent_text(),
        ));
    }

    frame.render_widget(Paragraph::new(Line::from(left)), area);

    // Connection state as a glyph plus a word: colour alone would be lost on a
    // monochrome terminal, and a spinner would only add motion, not meaning.
    let (glyph, label, style) = match &app.conn {
        ConnState::Ready => ("\u{25cf}", "online", palette.success()),
        ConnState::Connecting => ("\u{25d0}", "connecting", palette.warn()),
        ConnState::Authenticating => ("\u{25d0}", "authenticating", palette.warn()),
        ConnState::Offline => ("\u{25cb}", "offline", palette.dim()),
        ConnState::Failed(_) => ("\u{00d7}", "offline", palette.error()),
    };
    let mut right = vec![
        Span::styled(format!("{glyph} "), style),
        Span::styled(label, style),
    ];

    // A rejection is not transient: a key the relay will not admit stays
    // rejected, and a toast that expires after a few seconds leaves the user
    // staring at "offline" with no idea why. The reason displaces the host,
    // which is the less useful of the two once the connection has failed.
    match &app.conn {
        ConnState::Failed(reason) if !narrow => {
            right.push(Span::styled(
                format!("  {} ", failure_summary(reason)),
                palette.error(),
            ));
        }
        _ if !narrow => {
            let host = app
                .config
                .relay
                .trim_start_matches("wss://")
                .trim_start_matches("ws://");
            right.push(Span::styled(format!("  {host} "), palette.pending()));
        }
        _ => right.push(Span::raw(" ")),
    }
    frame.render_widget(Paragraph::new(Line::from(right).right_aligned()), area);
}

/// Reduces a relay failure to the part a human can act on.
///
/// Reasons arrive as layered machine prefixes — `auth rejected: restricted: not
/// a relay member` — and only the tail carries the meaning.
fn failure_summary(reason: &str) -> String {
    let tail = reason.rsplit(": ").next().unwrap_or(reason).trim();
    // Drop the errno parenthetical that adds nothing for a reader.
    let tail = tail
        .split_once(" (os error")
        .map_or(tail, |(head, _)| head)
        .trim();
    let tail = if tail.is_empty() { reason } else { tail };
    text::truncate_end(&tail.to_lowercase(), 40).into_owned()
}

// ----------------------------------------------------------------- composer

fn render_composer(frame: &mut Frame, area: Rect, app: &App) {
    let palette = &app.palette;
    let focused = app.focus == Focus::Composer;

    let (title, title_style) = match &app.compose_mode {
        ComposeMode::New => (None, palette.dim()),
        ComposeMode::Reply { to, .. } => {
            let who = app
                .timeline
                .iter()
                .find(|m| &m.id == to)
                .map(|m| app.display_name(&m.author))
                .unwrap_or_else(|| "a message".to_string());
            (
                Some(format!(" replying to {who} ")),
                palette.accent_strong(),
            )
        }
        ComposeMode::Edit { .. } => (Some(" editing ".to_string()), palette.chip_alt()),
    };

    let mut block = Block::bordered()
        .border_style(if focused {
            palette.border_focused()
        } else {
            palette.border()
        })
        .padding(Padding::horizontal(1));
    if let Some(title) = title {
        block = block.title_top(Line::from(title).style(title_style));
    }
    let cancel = app.keymap.hint(Action::Cancel);
    if !matches!(app.compose_mode, ComposeMode::New) {
        block = block.title_bottom(
            Line::from(format!(" {cancel} to cancel "))
                .right_aligned()
                .style(palette.dim()),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if app.composer.is_empty() && app.composer.text().is_empty() {
        let hint = if app.active.is_some() {
            format!(
                "message \u{2014} {} for commands, {} for keys",
                "/",
                app.keymap.hint(Action::OpenHelp)
            )
        } else {
            "open a channel to start writing".to_string()
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(hint, palette.pending()))),
            inner,
        );
        return;
    }

    let lines: Vec<Line> = app
        .composer
        .lines(inner.width.max(1) as usize)
        .into_iter()
        .map(Line::from)
        .collect();
    frame.render_widget(Paragraph::new(lines).style(palette.base()), inner);
}

// ------------------------------------------------------------------ members

fn render_members(frame: &mut Frame, area: Rect, app: &App) {
    let palette = &app.palette;
    let block = Block::new().padding(Padding::horizontal(1));
    let inner = block.inner(Rect {
        width: area.width.saturating_sub(1),
        ..area
    });
    frame.render_widget(block, area);

    let mut lines = vec![Line::from(vec![
        Span::styled(
            "members",
            Style::new()
                .fg(palette.overlay0)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("  {}", app.members.len()), palette.pending()),
    ])];

    for (pubkey, role) in app
        .members
        .iter()
        .take(inner.height.saturating_sub(1) as usize)
    {
        let presence = app
            .presence
            .get(pubkey)
            .copied()
            .unwrap_or(crate::model::Presence::Offline);
        lines.push(Line::from(vec![
            Span::styled(
                format!("{} ", presence.dot()),
                match presence {
                    crate::model::Presence::Online => palette.success(),
                    crate::model::Presence::Away => palette.warn(),
                    crate::model::Presence::Offline => palette.pending(),
                },
            ),
            Span::styled(role.sigil(), palette.accent_text()),
            Span::styled(
                text::truncate_end(
                    &app.display_name(pubkey),
                    inner.width.saturating_sub(4) as usize,
                )
                .into_owned(),
                if app.is_me(pubkey) {
                    palette.strong()
                } else {
                    palette.muted()
                },
            ),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), inner);
    widgets::separator(
        frame,
        Rect {
            x: area.x,
            width: 1,
            ..area
        },
        palette,
        false,
    );
}

// ----------------------------------------------------------------- hint bar

/// The transient mode bar. It appears while a key sequence is in flight, or
/// while the keyboard is on the conversation rather than in the composer, and
/// every string in it comes from the live keymap.
fn hint_bar(app: &App) -> Option<Line<'static>> {
    let palette = &app.palette;

    if !app.pending.is_empty() {
        let pressed = app
            .pending
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        let mut spans = vec![
            widgets::chip(&pressed.to_uppercase(), palette.chip()),
            Span::raw(" "),
        ];
        // Collapse the nine indexed channel jumps into one entry, or they
        // crowd out every command worth advertising.
        let mut hints: Vec<(Chord, Action)> = Vec::new();
        let mut jumped = false;
        for (chord, action) in &app.hints {
            if matches!(action, Action::JumpChannel(_)) && std::mem::replace(&mut jumped, true) {
                continue;
            }
            if !hints.iter().any(|(seen, _)| seen == chord) {
                hints.push((*chord, *action));
            }
        }
        for (chord, action) in hints.iter().take(12) {
            if let Action::JumpChannel(_) = action {
                spans.push(Span::styled("1..9", palette.accent_strong()));
                spans.push(Span::styled(format!(" {}  ", action.help()), palette.dim()));
                continue;
            }
            spans.push(Span::styled(chord.to_string(), palette.accent_strong()));
            spans.push(Span::styled(format!(" {}  ", action.help()), palette.dim()));
        }
        return Some(Line::from(spans));
    }

    if app.focus.scope() != Scope::Normal || app.overlay.is_some() {
        return None;
    }

    let mut spans = vec![widgets::chip("NAVIGATE", palette.chip()), Span::raw(" ")];
    for action in [
        Action::FocusComposer,
        Action::Reply,
        Action::React,
        Action::OpenSwitcher,
        Action::OpenHelp,
    ] {
        let key = app.keymap.hint(action);
        if key == "unset" {
            continue;
        }
        spans.push(Span::styled(key, palette.accent_strong()));
        spans.push(Span::styled(format!(" {}  ", action.help()), palette.dim()));
    }
    Some(Line::from(spans))
}

#[cfg(test)]
mod tests {
    use super::failure_summary;

    /// A rejection the user can act on must survive the trip from the wire to
    /// the status line; these are verbatim reasons from a real Buzz relay.
    #[test]
    fn a_failure_reason_keeps_the_part_that_means_something() {
        assert_eq!(
            failure_summary("auth rejected: restricted: not a relay member"),
            "not a relay member"
        );
        assert_eq!(
            failure_summary("connect: IO error: Connection refused (os error 111)"),
            "connection refused"
        );
        assert_eq!(
            failure_summary("auth-required: verification failed"),
            "verification failed"
        );
    }

    #[test]
    fn a_reason_without_layers_survives_intact() {
        assert_eq!(failure_summary("relay closed"), "relay closed");
        // A trailing separator must not reduce the reason to nothing.
        assert_eq!(failure_summary("timed out: "), "timed out: ");
    }

    #[test]
    fn a_long_reason_is_bounded_so_it_cannot_push_the_channel_name_off_screen() {
        let summary = failure_summary(&format!("error: {}", "x".repeat(200)));
        assert!(crate::ui::text::width(&summary) <= 40, "{summary}");
    }
}
