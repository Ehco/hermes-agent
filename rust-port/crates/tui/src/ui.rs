//! Thin ratatui renderer over the view-models — all logic (streaming deltas,
//! autocomplete filtering, key mapping, scroll math) lives in the testable
//! modules; this file only lays out panes and draws Lines.

use crate::autocomplete::Autocomplete;
use crate::editor::Editor;
use crate::model::Transcript;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;

/// Draw one frame: status bar, transcript, completion popup (floats above the
/// editor), editor.
pub fn draw(
    f: &mut Frame<'_>,
    transcript: &Transcript,
    editor: &Editor,
    ac: &Autocomplete,
    status: &str,
) {
    let area = f.area();
    let rows = Layout::vertical([
        Constraint::Length(1), // status
        Constraint::Min(3),    // transcript
        Constraint::Length(3), // editor (3 visible lines; scrollable buffer)
    ])
    .split(area);

    f.render_widget(
        Paragraph::new(Span::styled(
            status,
            Style::default().fg(Color::Black).bg(Color::DarkGray),
        )),
        rows[0],
    );

    let width = rows[1].width as usize;
    let height = rows[1].height as usize;
    let all = transcript.to_lines(width);
    let (start, end) = transcript.visible_range(all.len(), height);
    let visible: Vec<Line> = all[start..end.min(all.len())].to_vec();
    f.render_widget(Paragraph::new(visible), rows[1]);

    draw_editor(f, rows[2], editor);

    if ac.is_open() {
        draw_popup(f, rows[2], ac);
    }
}

fn draw_editor(f: &mut Frame<'_>, area: Rect, editor: &Editor) {
    f.render_widget(Clear, area);
    let text = if editor.text().is_empty() {
        Line::from(Span::styled(
            "type a message — /help for commands",
            Style::default().fg(Color::DarkGray),
        ))
    } else {
        Line::from(editor.text().to_string())
    };
    f.render_widget(
        Paragraph::new(text).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue)),
        ),
        area,
    );
}

fn draw_popup(f: &mut Frame<'_>, editor_area: Rect, ac: &Autocomplete) {
    let n = ac.candidates().len().min(10) as u16;
    let height = n.min(editor_area.y);
    if height == 0 {
        return;
    }
    let popup = Rect {
        x: editor_area.x,
        y: editor_area.y.saturating_sub(height),
        width: editor_area.width,
        height,
    };
    let rows: Vec<Line> = ac
        .candidates()
        .iter()
        .enumerate()
        .take(n as usize)
        .map(|(i, c)| {
            let marker = if i == ac.selected() { "› " } else { "  " };
            let style = if i == ac.selected() {
                Style::default().fg(Color::Black).bg(Color::Cyan)
            } else {
                Style::default().fg(Color::Cyan)
            };
            let hint = if c.def.args_hint.is_empty() {
                String::new()
            } else {
                format!(" {}", c.def.args_hint)
            };
            Line::from(Span::styled(
                format!("{marker}/{}{hint} — {}", c.canonical(), c.def.description),
                style,
            ))
        })
        .collect();
    f.render_widget(
        Clear,
        popup,
    );
    f.render_widget(
        Paragraph::new(rows),
        popup,
    );
}
