use std::path::Path;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
    Frame,
};

use crate::config::{Config, ORIGINAL_CLAUDE_PROFILE};
use crate::folder::display_path;

pub const FOLDER_WINDOW_SIZE: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LauncherSection {
    WorkingFolder,
    SelectProfile,
    AutoMode,
}

pub struct LauncherState {
    pub section: LauncherSection,
    pub folder_selected: usize,
    pub folder_scroll_offset: usize,
    pub profile_selected: usize,
    pub auto_mode: bool,
}

impl LauncherState {
    pub fn new(config: &Config, initial_target_dir: &Path) -> Self {
        let initial_str = display_path(initial_target_dir);
        let folder_selected = config
            .recent_folders
            .iter()
            .position(|f| display_path(f) == initial_str)
            .unwrap_or(0);

        let sorted_profiles = config.sorted_profiles_by_history();
        let profile_selected = if let Some(last) = &config.last_selected_profile {
            if last == ORIGINAL_CLAUDE_PROFILE {
                sorted_profiles.len()
            } else {
                sorted_profiles.iter().position(|p| p == last).unwrap_or(0)
            }
        } else {
            0
        };

        Self {
            section: LauncherSection::WorkingFolder,
            folder_selected,
            folder_scroll_offset: 0,
            profile_selected,
            auto_mode: config.auto_mode,
        }
    }

    pub fn adjust_folder_scroll(&mut self, total_folder_items: usize) {
        if self.folder_selected >= self.folder_scroll_offset + FOLDER_WINDOW_SIZE {
            self.folder_scroll_offset = self.folder_selected + 1 - FOLDER_WINDOW_SIZE;
        } else if self.folder_selected < self.folder_scroll_offset {
            self.folder_scroll_offset = self.folder_selected;
        }
        let max_offset = total_folder_items.saturating_sub(FOLDER_WINDOW_SIZE);
        self.folder_scroll_offset = self.folder_scroll_offset.min(max_offset);
    }
}

pub fn render_launcher(
    frame: &mut Frame,
    area: Rect,
    config: &Config,
    state: &LauncherState,
    target_dir: &Path,
    extra_args: &[String],
) {
    let mut lines = Vec::new();
    let pipe_style = Style::default().fg(Color::DarkGray);

    // ==========================================
    // 1. SECTION: Working Folder
    // ==========================================
    let is_folder_active = state.section == LauncherSection::WorkingFolder;
    let dot1_style = if is_folder_active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };

    let target_display = display_path(target_dir);

    lines.push(Line::from(vec![
        Span::styled("● ", dot1_style),
        Span::styled(
            "Working folder:",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {}", target_display),
            Style::default().fg(Color::Yellow),
        ),
    ]));

    // Build folder list without UNC prefixes
    let mut folder_items: Vec<String> = config
        .recent_folders
        .iter()
        .map(|f| display_path(f))
        .collect();

    if !folder_items.contains(&target_display) {
        folder_items.insert(0, target_display.clone());
    }

    let browse_idx = folder_items.len();
    folder_items.push("Browse folder... (Open OS File Dialog)".to_string());

    let total_folder_items = folder_items.len();
    let start_idx = state.folder_scroll_offset;
    let end_idx = (start_idx + FOLDER_WINDOW_SIZE).min(total_folder_items);

    if start_idx > 0 {
        lines.push(Line::from(vec![
            Span::styled("│   ", pipe_style),
            Span::styled(
                format!("▲ ({} more above)", start_idx),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    for idx in start_idx..end_idx {
        let is_selected = is_folder_active && idx == state.folder_selected;
        let is_current = idx < browse_idx && folder_items[idx] == target_display;

        let checkbox = if is_current {
            Span::styled("[✓] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        } else if is_selected {
            Span::styled("[▸] ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("[ ] ", Style::default().fg(Color::DarkGray))
        };

        let label_style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if is_current {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };

        lines.push(Line::from(vec![
            Span::styled("│   ", pipe_style),
            checkbox,
            Span::styled(folder_items[idx].clone(), label_style),
        ]));
    }

    if end_idx < total_folder_items {
        lines.push(Line::from(vec![
            Span::styled("│   ", pipe_style),
            Span::styled(
                format!("▼ ({} more below)", total_folder_items - end_idx),
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    }

    // Continuous connector
    lines.push(Line::from(Span::styled("│", pipe_style)));

    // ==========================================
    // 2. SECTION: Select Profile
    // ==========================================
    let is_profile_active = state.section == LauncherSection::SelectProfile;
    let dot2_style = if is_profile_active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };

    lines.push(Line::from(vec![
        Span::styled("● ", dot2_style),
        Span::styled(
            "Select profile:",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
    ]));

    let sorted_profiles = config.sorted_profiles_by_history();
    let original_idx = sorted_profiles.len();
    let total_profile_items = sorted_profiles.len() + 2;

    for i in 0..total_profile_items {
        let is_selected = is_profile_active && i == state.profile_selected;
        let is_current_chosen = match &config.last_selected_profile {
            Some(last) if last == ORIGINAL_CLAUDE_PROFILE => i == original_idx,
            Some(last) => i < original_idx && sorted_profiles.get(i) == Some(last),
            None => i == 0 && !sorted_profiles.is_empty(),
        };

        let checkbox = if is_current_chosen {
            Span::styled("[✓] ", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD))
        } else if is_selected {
            Span::styled("[▸] ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("[ ] ", Style::default().fg(Color::DarkGray))
        };

        let label_style = if is_selected {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else if is_current_chosen {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };

        if i < original_idx {
            let name = &sorted_profiles[i];
            let p = config.profiles.get(name);
            let detail = p
                .and_then(|prof| prof.base_url.as_deref())
                .unwrap_or("Anthropic API");

            let is_default = config.default_profile.as_deref() == Some(name.as_str());
            let default_tag = if is_default { " (default)" } else { "" };

            lines.push(Line::from(vec![
                Span::styled("│   ", pipe_style),
                checkbox,
                Span::styled(format!("{}{}", name, default_tag), label_style),
                Span::styled(format!("  — {}", detail), Style::default().fg(Color::DarkGray)),
            ]));
        } else if i == original_idx {
            lines.push(Line::from(vec![
                Span::styled("│   ", pipe_style),
                checkbox,
                Span::styled("Original Claude Code (standard environment)", label_style),
            ]));
        } else {
            lines.push(Line::from(vec![
                Span::styled("│   ", pipe_style),
                checkbox,
                Span::styled("Manage profiles... (Open full TUI manager)", label_style),
            ]));
        }
    }

    // Continuous connector
    lines.push(Line::from(Span::styled("│", pipe_style)));

    // ==========================================
    // 3. SECTION: Auto mode
    // ==========================================
    let is_auto_active = state.section == LauncherSection::AutoMode;
    let dot3_style = if is_auto_active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Green)
    };

    let (on_style, off_style) = if state.auto_mode {
        (
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD),
            Style::default().fg(Color::DarkGray),
        )
    } else {
        (
            Style::default().fg(Color::DarkGray),
            Style::default()
                .fg(Color::Black)
                .bg(Color::Red)
                .add_modifier(Modifier::BOLD),
        )
    };

    lines.push(Line::from(vec![
        Span::styled("● ", dot3_style),
        Span::styled(
            "Auto mode: ",
            Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" [● On] ", on_style),
        Span::raw("  "),
        Span::styled(" [○ Off] ", off_style),
        Span::styled(
            if state.auto_mode {
                "  (--dangerously-skip-permissions enabled)"
            } else {
                "  (prompts on permissions)"
            },
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    // Extra args if any
    if !extra_args.is_empty() {
        let args_str = extra_args.join(" ");
        lines.push(Line::from(vec![
            Span::styled("    Forwarding args: ", Style::default().fg(Color::DarkGray)),
            Span::styled(args_str, Style::default().fg(Color::Yellow)),
        ]));
    }

    frame.render_widget(Paragraph::new(lines), area);
}
