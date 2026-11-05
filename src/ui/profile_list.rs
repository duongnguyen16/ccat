use std::path::Path;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
    Frame,
};

use crate::config::Config;
use crate::folder::display_path;

fn mask_api_key(key: &str) -> String {
    let len = key.len();
    if len <= 8 {
        "********".to_string()
    } else {
        format!("{}...{}", &key[..4], &key[len - 4..])
    }
}

pub fn render_profile_list(
    frame: &mut Frame,
    area: Rect,
    config: &Config,
    profile_names: &[String],
    selected: usize,
    target_dir: &Path,
    warning: Option<&str>,
) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Top header (app name + target dir)
            Constraint::Min(8),    // Split pane (Profiles list + Detail view)
            Constraint::Length(1), // Warning line (if any)
            Constraint::Length(1), // Keybinding bar
        ])
        .split(area);

    // 1. Top Header
    let target_dir_str = display_path(target_dir);
    let header_block = Block::default()
        .borders(Borders::ALL)
        .style(Style::default().fg(Color::Cyan));

    let header_line = Line::from(vec![
        Span::styled(" ccat ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled("— Claude Code Profile Manager  |  Working Dir: ", Style::default().fg(Color::Gray)),
        Span::styled(target_dir_str, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
    ]);
    frame.render_widget(Paragraph::new(header_line).block(header_block), chunks[0]);

    // 2. Split Pane: Left (List) & Right (Detail)
    let body_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(35), // Profiles list
            Constraint::Percentage(65), // Profile details
        ])
        .split(chunks[1]);

    // 2a. Profiles List
    let items: Vec<ListItem> = if profile_names.is_empty() {
        vec![ListItem::new("  (No profiles configured yet. Press 'n' to create one)")
            .style(Style::default().fg(Color::DarkGray))]
    } else {
        profile_names
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let is_selected = i == selected;
                let is_default = config.default_profile.as_deref() == Some(name.as_str());

                let mut spans = Vec::new();
                let prefix = if is_selected { " ▸ " } else { "   " };
                spans.push(Span::raw(prefix));
                spans.push(Span::styled(
                    name.clone(),
                    if is_selected {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    },
                ));

                if is_default {
                    spans.push(Span::styled(
                        " (default)",
                        Style::default().fg(Color::Green).add_modifier(Modifier::ITALIC),
                    ));
                }

                let style = if is_selected {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::White)
                };

                ListItem::new(Line::from(spans)).style(style)
            })
            .collect()
    };

    let list_block = Block::default()
        .borders(Borders::ALL)
        .title(" Profiles ")
        .style(Style::default().fg(Color::White));
    frame.render_widget(List::new(items).block(list_block), body_chunks[0]);

    // 2b. Profile Details
    let selected_name = profile_names.get(selected);
    let selected_profile = selected_name.and_then(|name| config.profiles.get(name));

    let detail_title = match selected_name {
        Some(name) => format!(" Detail: {} ", name),
        None => " Detail ".to_string(),
    };

    let detail_block = Block::default()
        .borders(Borders::ALL)
        .title(detail_title)
        .style(Style::default().fg(Color::White));

    let detail_text = match selected_profile {
        Some(p) => {
            let base_url_val = p.base_url.as_deref().unwrap_or("(default - Anthropic API)");
            let api_key_val = p.api_key.as_deref().map(mask_api_key).unwrap_or_else(|| "(not set)".to_string());
            let fable_val = p.fable_model.as_deref().unwrap_or("(default: claude-fable-5[1m])");
            let opus_val = p.opus_model.as_deref().unwrap_or("(default: claude-opus-5[1m])");
            let sonnet_val = p.sonnet_model.as_deref().unwrap_or("(default: claude-sonnet-4-6)");
            let haiku_val = p.haiku_model.as_deref().unwrap_or("(default: claude-haiku-4-5)");

            vec![
                Line::from(vec![
                    Span::styled(" base_url     : ", Style::default().fg(Color::Cyan)),
                    Span::styled(base_url_val, Style::default().fg(Color::White)),
                ]),
                Line::from(vec![
                    Span::styled(" api_key      : ", Style::default().fg(Color::Cyan)),
                    Span::styled(api_key_val, Style::default().fg(Color::Yellow)),
                ]),
                Line::from(vec![
                    Span::styled(" fable_model  : ", Style::default().fg(Color::Cyan)),
                    Span::styled(fable_val, Style::default().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::styled(" opus_model   : ", Style::default().fg(Color::Cyan)),
                    Span::styled(opus_val, Style::default().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::styled(" sonnet_model : ", Style::default().fg(Color::Cyan)),
                    Span::styled(sonnet_val, Style::default().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::styled(" haiku_model  : ", Style::default().fg(Color::Cyan)),
                    Span::styled(haiku_val, Style::default().fg(Color::Green)),
                ]),
            ]
        }
        None => vec![Line::from(Span::styled(
            " Select or create a profile to view details.",
            Style::default().fg(Color::DarkGray),
        ))],
    };

    let detail_paragraph = Paragraph::new(detail_text)
        .block(detail_block)
        .wrap(Wrap { trim: false });
    frame.render_widget(detail_paragraph, body_chunks[1]);

    // 3. Warning line
    if let Some(warn) = warning {
        let warn_p = Paragraph::new(Line::from(vec![
            Span::styled(" [!] ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(warn, Style::default().fg(Color::Yellow)),
        ]));
        frame.render_widget(warn_p, chunks[2]);
    } else {
        frame.render_widget(Paragraph::new(""), chunks[2]);
    }

    // 4. Keybindings footer
    let help_bar = Paragraph::new(Line::from(vec![
        Span::styled("[↑↓/j/k]", Style::default().fg(Color::Yellow)),
        Span::raw(" select  "),
        Span::styled("[u/Enter]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw(" use claude  "),
        Span::styled("[n]", Style::default().fg(Color::Cyan)),
        Span::raw(" new  "),
        Span::styled("[e]", Style::default().fg(Color::Cyan)),
        Span::raw(" edit  "),
        Span::styled("[d]", Style::default().fg(Color::Red)),
        Span::raw(" delete  "),
        Span::styled("[D]", Style::default().fg(Color::Magenta)),
        Span::raw(" set default  "),
        Span::styled("[f]", Style::default().fg(Color::Blue)),
        Span::raw(" change dir  "),
        Span::styled("[q]", Style::default().fg(Color::Yellow)),
        Span::raw(" quit"),
    ]))
    .style(Style::default().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(help_bar, chunks[3]);
}
