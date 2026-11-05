use std::path::Path;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph},
    Frame,
};

use crate::folder::DirEntryItem;

pub fn render_folder_picker(
    frame: &mut Frame,
    area: Rect,
    current_path: &Path,
    entries: &[DirEntryItem],
    selected: usize,
    error: Option<&str>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header with current path
            Constraint::Min(5),    // Folder list
            Constraint::Length(1), // Error line (if any) or hint
            Constraint::Length(1), // Help / keybindings bar
        ])
        .split(area);

    // 1. Current Path Header
    let path_display = current_path.to_string_lossy();
    let header_block = Block::default()
        .borders(Borders::ALL)
        .title(" Select Working Directory ")
        .style(Style::default().fg(Color::Cyan));

    let header_text = Paragraph::new(Line::from(vec![
        Span::styled(" Current Path: ", Style::default().fg(Color::Gray)),
        Span::styled(path_display, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
    ]))
    .block(header_block);
    frame.render_widget(header_text, chunks[0]);

    // 2. Folder List
    let items: Vec<ListItem> = entries
        .iter()
        .enumerate()
        .map(|(i, entry)| {
            let is_selected = i == selected;
            let icon = if entry.is_parent { "📁 .." } else { "📁 " };
            let display_name = if entry.is_parent {
                ".. (Parent directory)".to_string()
            } else {
                format!("{}{}", icon, entry.name)
            };

            let style = if is_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else if entry.is_parent {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::White)
            };

            let prefix = if is_selected { " ▸ " } else { "   " };
            ListItem::new(format!("{}{}", prefix, display_name)).style(style)
        })
        .collect();

    let list_title = format!(" Subdirectories ({}) ", entries.len());
    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(list_title)
        .style(Style::default().fg(Color::White));

    let list_widget = List::new(items).block(list_block);
    frame.render_widget(list_widget, chunks[1]);

    // 3. Error or status message
    if let Some(err_msg) = error {
        let err_widget = Paragraph::new(Line::from(vec![
            Span::styled(" ⚠ ", Style::default().fg(Color::Red)),
            Span::styled(err_msg, Style::default().fg(Color::Red)),
        ]));
        frame.render_widget(err_widget, chunks[2]);
    } else {
        let hint_widget = Paragraph::new(Line::from(vec![
            Span::styled(" Press ", Style::default().fg(Color::DarkGray)),
            Span::styled("Space", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" to select this directory, or ", Style::default().fg(Color::DarkGray)),
            Span::styled("Enter", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(" to open subdirectory.", Style::default().fg(Color::DarkGray)),
        ]));
        frame.render_widget(hint_widget, chunks[2]);
    }

    // 4. Help / Keybindings bar
    let help_bar = Paragraph::new(Line::from(vec![
        Span::styled("[↑↓/j/k]", Style::default().fg(Color::Yellow)),
        Span::raw(" navigate  "),
        Span::styled("[Enter]", Style::default().fg(Color::Yellow)),
        Span::raw(" open folder  "),
        Span::styled("[Space]", Style::default().fg(Color::Green)),
        Span::raw(" select directory  "),
        Span::styled("[q/Esc]", Style::default().fg(Color::Yellow)),
        Span::raw(" quit"),
    ]))
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(help_bar, chunks[3]);
}
