//! Shared rendering primitives.
//!
//! These exist so that focus, selection, scrolling, and dismissal look and
//! behave identically everywhere. A modal that dims the background differently
//! from the next modal reads as two applications.

use ratatui::Frame;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::symbols;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph, Wrap};

use super::text;
use super::theme::Palette;

/// Track and thumb glyphs. A thicker thumb marks the focused pane, which reads
/// at a glance without spending a colour on it.
const TRACK: &str = "\u{2595}";
const THUMB_FOCUSED: &str = "\u{2590}";
const THUMB: &str = "\u{2595}";

/// Draws a proportional scrollbar in the rightmost column of `area`, and
/// nothing at all when the content already fits. A scrollbar that is always
/// present but sometimes meaningless trains people to ignore it.
pub fn scrollbar(
    frame: &mut Frame,
    area: Rect,
    palette: &Palette,
    offset: u16,
    content: u16,
    focused: bool,
) {
    if area.width == 0 || area.height == 0 || content <= area.height {
        return;
    }
    let column = area.right().saturating_sub(1);
    let height = area.height;
    let max_offset = content.saturating_sub(height).max(1);
    let offset = offset.min(max_offset);

    // At least one row of thumb, however long the history gets.
    let thumb = ((height as u32 * height as u32) / content as u32).max(1) as u16;
    let travel = height.saturating_sub(thumb);
    // Offset counts rows scrolled up from the bottom, so invert it: the thumb
    // sits at the bottom when we are following the newest message.
    let position =
        travel.saturating_sub((offset as u32 * travel as u32 / max_offset as u32) as u16);

    let track_style = Style::new().fg(if focused {
        palette.overlay1
    } else {
        palette.overlay0
    });
    let thumb_glyph = if focused { THUMB_FOCUSED } else { THUMB };

    let buffer = frame.buffer_mut();
    for row in 0..height {
        let y = area.y + row;
        let (glyph, style) = if row >= position && row < position + thumb {
            (thumb_glyph, track_style)
        } else {
            (TRACK, Style::new().fg(palette.surface_dim))
        };
        if let Some(cell) = buffer.cell_mut((column, y)) {
            cell.set_symbol(glyph);
            cell.set_style(style);
        }
    }
}

/// Stamps a single-column separator down the right edge of a panel, turning it
/// accent while that panel holds the keyboard. One cell of chrome replaces a
/// full focused border.
pub fn separator(frame: &mut Frame, area: Rect, palette: &Palette, focused: bool) {
    if area.width == 0 {
        return;
    }
    let column = area.right().saturating_sub(1);
    let style = Style::new().fg(if focused {
        palette.accent
    } else {
        palette.surface_dim
    });
    let buffer = frame.buffer_mut();
    for y in area.top()..area.bottom() {
        if let Some(cell) = buffer.cell_mut((column, y)) {
            cell.set_symbol("\u{2502}");
            cell.set_style(style);
        }
    }
}

/// Dims every cell already drawn, so an overlay reads as being in front of the
/// interface rather than pasted onto it. Cheaper and steadier than painting a
/// translucent shade, and it survives any theme.
pub fn dim_background(frame: &mut Frame) {
    for cell in frame.buffer_mut().content.iter_mut() {
        cell.modifier.insert(Modifier::DIM);
    }
}

/// A centred modal frame: clears what is underneath, fills the panel colour and
/// draws an accent border. Returns the interior, or `None` when the terminal is
/// too small to draw anything honest.
pub fn modal(
    frame: &mut Frame,
    palette: &Palette,
    title: &str,
    width: u16,
    height: u16,
    footer: Option<&str>,
) -> Option<Rect> {
    let screen = frame.area();
    if screen.width < 8 || screen.height < 6 {
        return None;
    }
    let width = width.min(screen.width.saturating_sub(4)).max(8);
    let height = height.min(screen.height.saturating_sub(2)).max(4);
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(Flex::Center)
        .areas(screen);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(Flex::Center)
        .areas(area);

    frame.render_widget(Clear, area);
    let mut block = Block::bordered()
        .border_set(symbols::border::PLAIN)
        .border_style(Style::new().fg(palette.accent))
        .style(palette.panel())
        .padding(Padding::horizontal(1))
        .title_top(Line::from(format!(" {title} ")).style(palette.accent_strong()));
    if let Some(footer) = footer {
        block = block.title_bottom(
            Line::from(format!(" {footer} "))
                .right_aligned()
                .style(palette.dim()),
        );
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    Some(inner)
}

/// The three-part empty state: what is missing, what the concept means, and the
/// key that fixes it. The key is passed in from the live keymap so it can never
/// document a binding the user has rebound away.
pub fn empty_state(
    frame: &mut Frame,
    area: Rect,
    palette: &Palette,
    what: &str,
    why: &str,
    how: &[(&str, &str)],
) {
    if area.height < 3 || area.width < 12 {
        return;
    }
    let mut lines = vec![
        Line::from(Span::styled(what.to_string(), palette.strong())),
        Line::from(""),
        Line::from(Span::styled(why.to_string(), palette.dim())),
    ];
    if !how.is_empty() {
        lines.push(Line::from(""));
    }
    for (key, description) in how {
        lines.push(Line::from(vec![
            Span::styled(format!("{key:>10}"), palette.accent_strong()),
            Span::raw("  "),
            Span::styled((*description).to_string(), palette.muted()),
        ]));
    }

    // `why` is prose and may wrap, so measure it rather than assuming one row.
    let wrapped = text::wrapped_height(why, area.width.saturating_sub(4) as usize);
    let height = (lines.len() + wrapped.saturating_sub(1)) as u16;
    let top = area.y + area.height.saturating_sub(height) / 2;
    let inner = Rect {
        x: area.x + 2,
        y: top,
        width: area.width.saturating_sub(4),
        height: height.min(area.height),
    };
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

/// A labelled chip: inverse text on a solid background, used for mode markers.
pub fn chip<'a>(label: &str, style: Style) -> Span<'a> {
    Span::styled(format!(" {label} "), style)
}

/// Right-aligned in-band warnings for configuration problems. Deliberately not
/// a modal: a mistyped keybinding should not block the client from starting.
pub fn diagnostics_banner(frame: &mut Frame, area: Rect, palette: &Palette, problems: &[String]) {
    if problems.is_empty() || area.height == 0 {
        return;
    }
    let style = Style::new()
        .bg(palette.yellow)
        .fg(palette.contrast_fg())
        .add_modifier(Modifier::BOLD);
    let rows = problems.len().min(area.height as usize);
    for (index, problem) in problems.iter().take(rows).enumerate() {
        let text = format!(
            " {} ",
            text::truncate_end(problem, area.width.saturating_sub(2) as usize)
        );
        let line = Line::from(Span::styled(text, style)).right_aligned();
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                y: area.y + index as u16,
                height: 1,
                ..area
            },
        );
    }
}

/// Toasts: small bordered boxes stacked in the bottom-right corner. Never
/// full-width, because they must not look like part of the conversation.
pub fn toasts(frame: &mut Frame, area: Rect, palette: &Palette, toasts: &[crate::app::Toast]) {
    use crate::app::ToastKind;
    if toasts.is_empty() {
        return;
    }
    let mut bottom = area.bottom();
    for toast in toasts.iter().rev() {
        let dot_style = match toast.kind {
            ToastKind::Info => palette.accent_text(),
            ToastKind::Success => palette.success(),
            ToastKind::Warn => palette.warn(),
            ToastKind::Error => palette.error(),
        };
        let detail_width = toast.detail.as_deref().map_or(0, text::width);
        let width = (text::width(&toast.title).max(detail_width) + 6)
            .min(area.width.saturating_sub(2) as usize)
            .max(12) as u16;
        let height = if toast.detail.is_some() { 4 } else { 3 };
        if bottom < area.y + height {
            break;
        }

        let rect = Rect {
            x: area.right().saturating_sub(width + 1),
            y: bottom - height,
            width,
            height,
        };
        frame.render_widget(Clear, rect);
        let block = Block::bordered()
            .border_set(symbols::border::PLAIN)
            .border_style(Style::new().fg(palette.overlay0))
            .style(palette.panel());
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let budget = inner.width.saturating_sub(2) as usize;
        let mut lines = vec![Line::from(vec![
            Span::styled("\u{25cf} ", dot_style),
            Span::styled(
                text::truncate_end(&toast.title, budget).into_owned(),
                palette.strong(),
            ),
        ])];
        if let Some(detail) = &toast.detail {
            lines.push(Line::from(Span::styled(
                format!("  {}", text::truncate_end(detail, budget)),
                palette.dim(),
            )));
        }
        frame.render_widget(Paragraph::new(lines), inner);
        bottom = rect.y;
    }
}
