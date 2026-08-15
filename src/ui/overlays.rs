//! Overlay rendering.
//!
//! Every overlay is drawn through the same modal shell so that the border, the
//! fill, and the dimmed background never vary between them. The background is
//! dimmed by walking the buffer rather than by painting a shade, which keeps
//! the effect correct on any theme and on terminals without transparency.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui_image::{Resize, StatefulImage};

use super::{text, widgets};
use crate::app::App;
use crate::approval::{ChoiceKind, Request};
use crate::media::Status;
use crate::overlay::{Approval, Confirm, Help, Overlay, Picker, Prompt, Search};

pub fn render(frame: &mut Frame, app: &mut App, overlay: &Overlay) {
    if overlay.is_modal() {
        widgets::dim_background(frame);
    }
    let title = overlay.title();
    match overlay {
        Overlay::Help(help) => render_help(frame, app, help, &title),
        Overlay::Picker(picker) => render_picker(frame, app, picker),
        Overlay::Prompt(prompt) => render_prompt(frame, app, prompt),
        Overlay::Confirm(confirm) => render_confirm(frame, app, confirm),
        Overlay::Approval(approval) => render_approval(frame, app, approval, &title),
        Overlay::Search(search) => render_search(frame, app, search),
        Overlay::Profile(pubkey) => render_profile(frame, app, pubkey),
        Overlay::Image(url) => render_image(frame, app, url),
    }
}

fn render_help(frame: &mut Frame, app: &App, help: &Help, title: &str) {
    let palette = &app.palette;
    let screen = frame.area();
    let width = 78.min(screen.width.saturating_sub(4));
    let height = 26.min(screen.height.saturating_sub(2));
    let Some(area) = widgets::modal(
        frame,
        palette,
        title,
        width,
        height,
        Some("/ to filter \u{00b7} esc to close"),
    ) else {
        return;
    };

    let [filter, list] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);

    let query = if help.query.text.is_empty() {
        Span::styled("type to filter", palette.pending())
    } else {
        Span::styled(help.query.text.clone(), palette.base())
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("/ ", palette.accent_strong()),
            query,
        ])),
        Rect {
            height: 1,
            ..filter
        },
    );

    if help.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " no matching keybinds",
                Style::new().fg(palette.overlay1),
            ))),
            list,
        );
        return;
    }

    // A single key column keeps the descriptions aligned across every group.
    let gutter = help.key_width().min(18);
    let rows = help.visible();
    let mut lines: Vec<Line> = Vec::new();
    for (heading, row) in rows.iter().skip(help.scroll) {
        if let Some(group) = heading {
            if !lines.is_empty() {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(Span::styled(
                group.label().to_string(),
                palette.accent_strong(),
            )));
        }
        // The action name is what goes in keys.toml, so showing it turns the
        // reference into rebinding documentation.
        let described = format!("{:<28}", row.description);
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<gutter$}", row.keys, gutter = gutter),
                Style::new().fg(palette.mauve).add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(described, palette.muted()),
            Span::styled(row.action.clone(), palette.pending()),
        ]));
        if lines.len() >= list.height as usize {
            break;
        }
    }
    frame.render_widget(Paragraph::new(lines), list);
}

fn render_picker(frame: &mut Frame, app: &App, picker: &Picker) {
    let palette = &app.palette;
    let screen = frame.area();
    let width = 62.min(screen.width.saturating_sub(4));
    let height = (picker.filtered.len() as u16 + 5).clamp(8, 22.min(screen.height));
    let hint = format!("enter to {} \u{00b7} esc to close", picker.accept_hint());
    let Some(area) = widgets::modal(frame, palette, &picker.title, width, height, Some(&hint))
    else {
        return;
    };

    let [filter, list] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
    let query = if picker.query.text.is_empty() {
        Span::styled("type to filter", palette.pending())
    } else {
        Span::styled(picker.query.text.clone(), palette.base())
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("\u{203a} ", palette.accent_strong()),
            query,
        ])),
        Rect {
            height: 1,
            ..filter
        },
    );

    if picker.filtered.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " nothing matches",
                Style::new().fg(palette.overlay1),
            ))),
            list,
        );
        return;
    }

    // Keep the cursor inside the window without letting the list jump.
    let height = list.height as usize;
    let offset = picker.cursor.saturating_sub(height.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for (row, &index) in picker.filtered.iter().enumerate().skip(offset).take(height) {
        let item = &picker.items[index];
        let selected = row == picker.cursor;
        let style = if selected {
            palette.selected()
        } else {
            Style::new().fg(palette.subtext0)
        };
        let mut spans = vec![
            Span::styled(
                if selected { " \u{25b8} " } else { "   " },
                palette.accent_text(),
            ),
            Span::styled(
                text::truncate_end(&item.label, list.width.saturating_sub(12) as usize)
                    .into_owned(),
                style,
            ),
        ];
        if let Some(badge) = &item.badge {
            spans.push(Span::styled(format!("  {badge}"), palette.accent_strong()));
        }
        if let Some(detail) = &item.detail {
            spans.push(Span::styled(
                format!("  {}", text::truncate_end(detail, 24)),
                palette.pending(),
            ));
        }
        lines.push(Line::from(spans).style(if selected {
            palette.selected()
        } else {
            Style::new()
        }));
    }
    frame.render_widget(Paragraph::new(lines), list);
}

fn render_prompt(frame: &mut Frame, app: &App, prompt: &Prompt) {
    let palette = &app.palette;
    let Some(area) = widgets::modal(
        frame,
        palette,
        prompt.kind.title(),
        56,
        if prompt.error.is_some() { 7 } else { 6 },
        Some("enter to confirm \u{00b7} esc to cancel"),
    ) else {
        return;
    };

    let [field, error] = Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(area);

    let block = Block::bordered().border_style(palette.border_focused());
    let inner = block.inner(field);
    frame.render_widget(block, field);

    let content = if prompt.value.is_empty() {
        Line::from(Span::styled(prompt.kind.placeholder(), palette.pending()))
    } else {
        Line::from(Span::styled(prompt.value.clone(), palette.base()))
    };
    frame.render_widget(Paragraph::new(content), inner);
    frame.set_cursor_position((
        inner.x + text::width(&prompt.value).min(inner.width.saturating_sub(1) as usize) as u16,
        inner.y,
    ));

    if let Some(message) = &prompt.error {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(message.clone(), palette.error()))),
            error,
        );
    }
}

fn render_confirm(frame: &mut Frame, app: &App, confirm: &Confirm) {
    let palette = &app.palette;
    let Some(area) = widgets::modal(frame, palette, "confirm", 58, 8, None) else {
        return;
    };

    let [question, detail, _gap, actions] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Fill(1),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            confirm.action.question(),
            palette.strong(),
        ))),
        question,
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            confirm.action.detail(),
            palette.dim(),
        )))
        .wrap(Wrap { trim: true }),
        detail,
    );

    // The safe answer is highlighted by default, so a reflexive enter is safe.
    let yes = if confirm.yes {
        palette.chip()
    } else {
        Style::new().fg(palette.subtext0)
    };
    let no = if confirm.yes {
        Style::new().fg(palette.subtext0)
    } else {
        palette.chip()
    };
    frame.render_widget(
        Paragraph::new(
            Line::from(vec![
                Span::styled(" y  yes ", yes),
                Span::raw("  "),
                Span::styled(" n  no ", no),
            ])
            .right_aligned(),
        ),
        actions,
    );
}

/// An agent's permission request, with the operation quoted verbatim so the
/// decision is made against what was actually asked rather than a paraphrase.
fn render_approval(frame: &mut Frame, app: &App, approval: &Approval, title: &str) {
    let palette = &app.palette;
    let screen = frame.area();
    let width = 66.min(screen.width.saturating_sub(4));

    let (subject, detail) = match &approval.request {
        Request::Command { command, reason } => (
            command
                .clone()
                .unwrap_or_else(|| "an operation the agent did not spell out".to_string()),
            reason.clone(),
        ),
        Request::Question { question, .. } => (question.clone(), None),
    };

    // The modal is sized to its contents so a long reason is not clipped and a
    // short one leaves no dead space above the choices.
    let inner_width = width.saturating_sub(4).max(8) as usize;
    let subject_height = text::wrapped_height(&subject, inner_width).min(6) as u16;
    let detail_height = detail
        .as_deref()
        .map(|detail| text::wrapped_height(detail, inner_width).min(5) as u16)
        .unwrap_or(0);
    let choices_height = approval.choices.len() as u16;
    let height = 2 + subject_height + detail_height + 1 + choices_height;

    let Some(area) = widgets::modal(frame, palette, title, width, height, Some(approval.hint()))
    else {
        return;
    };

    let [subject_area, detail_area, _gap, choices_area] = Layout::vertical([
        Constraint::Length(subject_height),
        Constraint::Length(detail_height),
        Constraint::Length(1),
        Constraint::Fill(1),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(subject.as_str())
            .style(palette.strong())
            .wrap(Wrap { trim: true }),
        subject_area,
    );
    if let Some(detail) = detail.as_deref() {
        frame.render_widget(
            Paragraph::new(detail)
                .style(palette.dim())
                .wrap(Wrap { trim: true }),
            detail_area,
        );
    }

    let rows: Vec<Line> = approval
        .choices
        .iter()
        .enumerate()
        .map(|(index, choice)| {
            let chosen = index == approval.selected;
            // Granting is styled as a warning even when selected: the colour is
            // part of the information, not decoration.
            let style = match (chosen, choice.kind) {
                (true, ChoiceKind::Deny) => palette.chip(),
                (true, _) => palette.warn().add_modifier(Modifier::REVERSED),
                (false, _) => Style::new().fg(palette.subtext0),
            };
            Line::from(vec![
                Span::styled(if chosen { " > " } else { "   " }, style),
                Span::styled(format!("{} ", index + 1), palette.dim()),
                Span::styled(format!("{} ", choice.label), style),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), choices_area);
}

fn render_search(frame: &mut Frame, app: &App, search: &Search) {
    let palette = &app.palette;
    let screen = frame.area();
    let Some(area) = widgets::modal(
        frame,
        palette,
        "search",
        screen.width.saturating_sub(8).min(96),
        screen.height.saturating_sub(4).min(26),
        Some("enter to jump \u{00b7} esc to close"),
    ) else {
        return;
    };

    let [filter, list] = Layout::vertical([Constraint::Length(2), Constraint::Fill(1)]).areas(area);
    let scope = search
        .scope
        .as_ref()
        .and_then(|id| app.channels.iter().find(|c| &c.id == id))
        .map(|c| format!(" in {}{}", c.sigil(), c.name))
        .unwrap_or_default();
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("\u{1f50d} ", palette.accent_strong()),
            Span::styled(search.query.text.clone(), palette.base()),
            Span::styled(scope, palette.pending()),
        ])),
        Rect {
            height: 1,
            ..filter
        },
    );

    if search.results.is_empty() {
        let message = if search.query.text.trim().is_empty() {
            "type to search this community"
        } else if search.waiting {
            "searching\u{2026}"
        } else {
            "nothing found"
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" {message}"),
                Style::new().fg(palette.overlay1),
            ))),
            list,
        );
        return;
    }

    let height = list.height as usize / 2;
    let offset = search.cursor.saturating_sub(height.saturating_sub(1));
    let mut lines: Vec<Line> = Vec::new();
    for (row, message) in search.results.iter().enumerate().skip(offset) {
        let selected = row == search.cursor;
        let where_ = app
            .channels
            .iter()
            .find(|c| c.id == message.channel)
            .map(|c| format!("{}{}", c.sigil(), c.name))
            .unwrap_or_else(|| message.channel.clone());
        lines.push(
            Line::from(vec![
                Span::styled(
                    if selected { " \u{25b8} " } else { "   " },
                    palette.accent_text(),
                ),
                Span::styled(app.display_name(&message.author), palette.accent_strong()),
                Span::styled(format!("  {where_}"), palette.pending()),
            ])
            .style(if selected {
                palette.selected()
            } else {
                Style::new()
            }),
        );
        lines.push(
            Line::from(Span::styled(
                format!(
                    "     {}",
                    text::truncate_end(
                        &message.body.replace('\n', " "),
                        list.width.saturating_sub(6) as usize
                    )
                ),
                palette.muted(),
            ))
            .style(if selected {
                palette.selected()
            } else {
                Style::new()
            }),
        );
        if lines.len() >= list.height as usize {
            break;
        }
    }
    frame.render_widget(Paragraph::new(lines), list);
}

fn render_profile(frame: &mut Frame, app: &mut App, pubkey: &str) {
    let Some(area) = widgets::modal(frame, &app.palette, "profile", 62, 14, Some("esc to close"))
    else {
        return;
    };

    // The avatar is the one place a portrait is worth real pixels, so it gets
    // the terminal's graphics protocol when there is one.
    let avatar = app
        .config
        .ui
        .avatars
        .then(|| {
            app.profiles
                .get(pubkey)
                .and_then(|profile| profile.picture.clone())
        })
        .flatten()
        .map(|picture| app.config.resolve_media(&picture));
    let avatar_columns = if avatar.is_some() { 14 } else { 0 };
    let [portrait, details] =
        Layout::horizontal([Constraint::Length(avatar_columns), Constraint::Fill(1)]).areas(area);

    if let Some(url) = avatar {
        app.media.request(&url);
        let rect = Rect {
            height: portrait.height.min(7),
            width: portrait.width.saturating_sub(2),
            ..portrait
        };
        match app.media.status(&url) {
            Status::Ready(protocol) => frame.render_stateful_widget(
                StatefulImage::default().resize(Resize::Fit(None)),
                rect,
                protocol,
            ),
            Status::Loading | Status::Failed(_) => frame.render_widget(
                Paragraph::new(Line::from(Span::styled("\u{25a3}", app.palette.pending()))),
                Rect { height: 1, ..rect },
            ),
        }
    }

    let palette = &app.palette;
    let profile = app.profiles.get(pubkey);
    let mut lines = vec![
        Line::from(Span::styled(app.display_name(pubkey), palette.strong())),
        Line::from(""),
    ];
    if let Some(nip05) = profile.and_then(|p| p.nip05.as_deref()) {
        lines.push(Line::from(vec![
            Span::styled("verified  ", palette.dim()),
            Span::styled(nip05.to_string(), palette.success()),
        ]));
    }
    if let Some(about) = profile.and_then(|p| p.about.as_deref()) {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(about.to_string(), palette.muted())));
    }
    lines.push(Line::from(""));

    // A full npub is 63 characters, which no panel can hold; eliding the middle
    // keeps both ends, and those are the parts people actually compare.
    let npub = nostr::key::PublicKey::from_hex(pubkey)
        .ok()
        .and_then(|key| {
            use nostr::nips::nip19::ToBech32;
            key.to_bech32().ok()
        })
        .unwrap_or_else(|| pubkey.to_string());
    lines.push(Line::from(vec![
        Span::styled("key  ", palette.dim()),
        Span::styled(
            text::middle_elide(&npub, details.width.saturating_sub(6) as usize).into_owned(),
            palette.pending(),
        ),
    ]));
    if app.is_me(pubkey) {
        lines.push(Line::from(Span::styled(
            "this is you",
            palette.accent_text(),
        )));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), details);
}

fn render_image(frame: &mut Frame, app: &mut App, url: &str) {
    let palette = &app.palette;
    let screen = frame.area();
    let Some(area) = widgets::modal(
        frame,
        palette,
        "image",
        screen.width.saturating_sub(6),
        screen.height.saturating_sub(3),
        Some("esc to close"),
    ) else {
        return;
    };
    let url = url.to_string();
    app.media.request(&url);
    match app.media.status(&url) {
        Status::Ready(protocol) => {
            frame.render_stateful_widget(
                StatefulImage::default().resize(Resize::Fit(None)),
                area,
                protocol,
            );
        }
        Status::Loading => {
            widgets::empty_state(frame, area, palette, "loading", &url, &[]);
        }
        Status::Failed(reason) => {
            let reason = reason.to_string();
            widgets::empty_state(frame, area, palette, "image unavailable", &reason, &[]);
        }
    }
}
