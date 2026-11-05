use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelField {
    Fable,
    Opus,
    Sonnet,
    Haiku,
}

impl ModelField {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Fable => "Fable",
            Self::Opus => "Opus",
            Self::Sonnet => "Sonnet",
            Self::Haiku => "Haiku",
        }
    }
}

pub fn render_model_picker(
    frame: &mut Frame,
    area: Rect,
    field: ModelField,
    items: &[String],
    selected: usize,
    loading: bool,
    error: Option<&str>,
    manual_input: Option<&str>,
    spinner_frame: usize,
) {
    let popup_width = 66.min(area.width.saturating_sub(4));
    let popup_height = 18.min(area.height.saturating_sub(2));

    let popup_area = Rect {
        x: (area.width.saturating_sub(popup_width)) / 2,
        y: (area.height.saturating_sub(popup_height)) / 2,
        width: popup_width,
        height: popup_height,
    };

    frame.render_widget(Clear, popup_area);

    let title = format!(" Select model for: {} ", field.name());
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::default().fg(Color::Cyan));
    frame.render_widget(block, popup_area);

    let inner = Rect {
        x: popup_area.x + 2,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(4),
        height: popup_area.height.saturating_sub(2),
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),    // List of options
            Constraint::Length(2), // Status/loading
            Constraint::Length(1), // Help bar
        ])
        .split(inner);

    // 1. Items list: Item 0 is always "[Custom] Manual Input..."
    let mut list_items = Vec::new();

    // Option 0: Manual Input
    let is_manual_selected = selected == 0;
    let manual_text = match manual_input {
        Some(text) => format!("[Custom] Manual Input: {}", text),
        None => "[Custom] Manual Input...".to_string(),
    };
    let manual_style = if is_manual_selected {
        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Yellow)
    };
    let manual_prefix = if is_manual_selected { " ▸ " } else { "   " };
    list_items.push(ListItem::new(format!("{}{}", manual_prefix, manual_text)).style(manual_style));

    // Remaining items from fetched API
    for (i, model_name) in items.iter().enumerate() {
        let is_selected = (i + 1) == selected;
        let style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let prefix = if is_selected { " ▸ " } else { "   " };
        list_items.push(ListItem::new(format!("{}{}", prefix, model_name)).style(style));
    }

    let list = List::new(list_items);
    frame.render_widget(list, chunks[0]);

    // 2. Status / Loading / Error
    let spinners = ["-", "\\", "|", "/"];
    let spinner = spinners[spinner_frame % spinners.len()];

    let status_line = if loading {
        Line::from(vec![
            Span::styled(format!(" [{}] ", spinner), Style::default().fg(Color::Yellow)),
            Span::styled("Fetching model list from API...", Style::default().fg(Color::Yellow)),
        ])
    } else if let Some(err) = error {
        Line::from(vec![
            Span::styled(" [!] ", Style::default().fg(Color::Red)),
            Span::styled(format!("Error: {}", err), Style::default().fg(Color::Red)),
        ])
    } else if items.is_empty() {
        Line::from(vec![
            Span::styled(" (No models found from API or not connected)", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled(format!(" [ok] Found {} models", items.len()), Style::default().fg(Color::Green)),
        ])
    };
    frame.render_widget(Paragraph::new(status_line), chunks[1]);

    // 3. Help bar
    let help = if manual_input.is_some() {
        Line::from(vec![
            Span::styled("[Typing]", Style::default().fg(Color::Yellow)),
            Span::raw(" enter model name  "),
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" confirm  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red)),
            Span::raw(" cancel input"),
        ])
    } else {
        Line::from(vec![
            Span::styled("[↑↓/j/k]", Style::default().fg(Color::Yellow)),
            Span::raw(" select  "),
            Span::styled("[Enter]", Style::default().fg(Color::Green)),
            Span::raw(" confirm  "),
            Span::styled("[r]", Style::default().fg(Color::Cyan)),
            Span::raw(" refresh  "),
            Span::styled("[Esc]", Style::default().fg(Color::Red)),
            Span::raw(" cancel"),
        ])
    };
    frame.render_widget(Paragraph::new(help), chunks[2]);
}
