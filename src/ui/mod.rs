pub mod folder_picker;
pub mod launcher;
pub mod model_picker;
pub mod profile_form;
pub mod profile_list;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

pub fn render_confirm_delete(frame: &mut Frame, area: Rect, name: &str) {
    let popup_width = 54.min(area.width.saturating_sub(4));
    let popup_height = 8.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Confirm Delete ")
        .style(Style::default().fg(Color::Red));
    frame.render_widget(block, popup_area);

    let inner = Rect {
        x: popup_area.x + 2,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(4),
        height: popup_area.height.saturating_sub(2),
    };

    let text = vec![
        Line::from(vec![
            Span::styled(" Are you sure you want to delete profile ", Style::default().fg(Color::White)),
            Span::styled(format!("\"{}\"", name), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("?", Style::default().fg(Color::White)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(" [y]", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)),
            Span::raw(" Confirm delete      "),
            Span::styled("[n / Esc]", Style::default().fg(Color::Cyan)),
            Span::raw(" Cancel"),
        ]),
    ];

    frame.render_widget(Paragraph::new(text), inner);
}
