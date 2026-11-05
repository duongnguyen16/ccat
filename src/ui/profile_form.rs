use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

use crate::config::Profile;

pub const FIELD_NAME: usize = 0;
pub const FIELD_BASE_URL: usize = 1;
pub const FIELD_API_KEY: usize = 2;
pub const FIELD_FABLE: usize = 3;
pub const FIELD_OPUS: usize = 4;
pub const FIELD_SONNET: usize = 5;
pub const FIELD_HAIKU: usize = 6;
pub const TOTAL_FIELDS: usize = 7;

pub fn render_profile_form(
    frame: &mut Frame,
    area: Rect,
    editing: Option<&str>,
    field_idx: usize,
    name_buf: &str,
    draft: &Profile,
    error: Option<&str>,
) {
    // Center popup dialog
    let popup_width = 76.min(area.width.saturating_sub(4));
    let popup_height = 20.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    frame.render_widget(Clear, popup_area);

    let title = match editing {
        Some(name) => format!(" Edit Profile: {} ", name),
        None => " Create New Profile ".to_string(),
    };

    let main_block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(main_block, popup_area);

    let inner = Rect {
        x: popup_area.x + 2,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(4),
        height: popup_area.height.saturating_sub(2),
    };

    let field_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2), // 0: Name
            Constraint::Length(2), // 1: Base URL
            Constraint::Length(2), // 2: API Key
            Constraint::Length(2), // 3: Fable Model
            Constraint::Length(2), // 4: Opus Model
            Constraint::Length(2), // 5: Sonnet Model
            Constraint::Length(2), // 6: Haiku Model
            Constraint::Length(1), // Error line
            Constraint::Length(1), // Keybindings
        ])
        .split(inner);

    let fields = [
        ("Profile Name", name_buf, "e.g. dev, staging, local-ollama"),
        ("Base URL", draft.base_url.as_deref().unwrap_or(""), "e.g. http://localhost:20128/v1"),
        ("API Key", draft.api_key.as_deref().unwrap_or(""), "sk-ant-... (leave blank for local/proxy)"),
        ("Fable Model", draft.fable_model.as_deref().unwrap_or(""), "e.g. claude-fable-5[1m] (Enter to select)"),
        ("Opus Model", draft.opus_model.as_deref().unwrap_or(""), "e.g. claude-opus-5[1m] (Enter to select)"),
        ("Sonnet Model", draft.sonnet_model.as_deref().unwrap_or(""), "e.g. claude-sonnet-4-6 (Enter to select)"),
        ("Haiku Model", draft.haiku_model.as_deref().unwrap_or(""), "e.g. claude-haiku-4-5 (Enter to select)"),
    ];

    for (i, (label, val, placeholder)) in fields.iter().enumerate() {
        if i >= field_layout.len() {
            break;
        }
        let is_active = i == field_idx;
        let label_style = if is_active {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };

        let val_display = if val.is_empty() {
            Span::styled(format!(" <{}>", placeholder), Style::default().fg(Color::DarkGray))
        } else {
            Span::styled(format!(" {}", val), Style::default().fg(Color::White))
        };

        let cursor = if is_active { " ▸ " } else { "   " };
        let line = Line::from(vec![
            Span::styled(cursor, label_style),
            Span::styled(format!("{:<13}:", label), label_style),
            val_display,
        ]);

        let row_style = if is_active {
            Style::default().bg(Color::Rgb(30, 40, 60))
        } else {
            Style::default()
        };

        let p = Paragraph::new(line).style(row_style);
        frame.render_widget(p, field_layout[i]);
    }

    // Error message line
    if let Some(err) = error {
        let err_p = Paragraph::new(Line::from(vec![
            Span::styled(" ⚠ ", Style::default().fg(Color::Red)),
            Span::styled(err, Style::default().fg(Color::Red)),
        ]));
        frame.render_widget(err_p, field_layout[7]);
    }

    // Help Bar
    let help_line = Line::from(vec![
        Span::styled("[Tab/Shift+Tab]", Style::default().fg(Color::Yellow)),
        Span::raw(" switch field  "),
        Span::styled("[Enter]", Style::default().fg(Color::Cyan)),
        Span::raw(" select model  "),
        Span::styled("[Ctrl+S]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" Save  "),
        Span::styled("[Esc]", Style::default().fg(Color::Red)),
        Span::raw(" Cancel"),
    ]);
    frame.render_widget(Paragraph::new(help_line), field_layout[8]);
}
