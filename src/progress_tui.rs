use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Paragraph, Wrap},
};
use std::time::Duration;

use crate::progress::ProgressMap;

pub struct ProgressTui {
    server_list: Vec<String>,
    selected_index: usize,
    scroll_offset: usize,
    all_complete: bool,
    ctrl_c_count: u8,
}

impl ProgressTui {
    pub fn new(servers: Vec<String>) -> Self {
        Self {
            server_list: servers,
            selected_index: 0,
            scroll_offset: 0,
            all_complete: false,
            ctrl_c_count: 0,
        }
    }

    pub fn next(&mut self) {
        if !self.server_list.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.server_list.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.server_list.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.server_list.len() - 1
            } else {
                self.selected_index - 1
            };
        }
    }

    pub fn render(&mut self, frame: &mut Frame, progress_map: &ProgressMap) {
        let area = frame.area();

        // Split the screen: 40% for server list, 60% for output
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        // Render server list
        self.render_server_list(frame, chunks[0], progress_map);

        // Render output pane
        self.render_output_pane(frame, chunks[1], progress_map);
    }

    fn render_server_list(&self, frame: &mut Frame, area: Rect, progress_map: &ProgressMap) {
        let map = progress_map.lock().unwrap();

        let items: Vec<ListItem> = self
            .server_list
            .iter()
            .enumerate()
            .map(|(i, server)| {
                let hostname = server.split(':').next().unwrap_or(server);
                let status = map
                    .get(hostname)
                    .map(|s| s.phase.to_string())
                    .unwrap_or_else(|| "Unknown".to_string());

                let color = map
                    .get(hostname)
                    .map(|s| s.phase.color())
                    .unwrap_or(Color::Gray);

                let prefix = if i == self.selected_index { "> " } else { "  " };
                let line = format!("{}{}: {}", prefix, hostname, status);

                ListItem::new(line).style(Style::default().fg(color))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .title("Server Status")
                .borders(Borders::ALL),
        );

        frame.render_widget(list, area);
    }

    fn render_output_pane(&mut self, frame: &mut Frame, area: Rect, progress_map: &ProgressMap) {
        let map = progress_map.lock().unwrap();

        let selected_server = self.server_list.get(self.selected_index);
        let output = if let Some(server) = selected_server {
            let hostname = server.split(':').next().unwrap_or(server);
            map.get(hostname)
                .map(|s| s.full_output.as_str())
                .unwrap_or("No output yet...")
        } else {
            "No server selected"
        };

        // Calculate line count for scrolling
        let line_count = output.lines().count();
        let visible_lines = (area.height.saturating_sub(2)) as usize; // Subtract borders

        // Auto-scroll to bottom if not manually scrolled
        if self.scroll_offset + visible_lines >= line_count.saturating_sub(1) {
            self.scroll_offset = line_count.saturating_sub(visible_lines).max(0);
        }

        let selected_hostname = selected_server
            .map(|s| s.split(':').next().unwrap_or(s))
            .unwrap_or("None");

        let paragraph = Paragraph::new(output)
            .block(
                Block::default()
                    .title(format!("Output: {}", selected_hostname))
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
            .scroll((self.scroll_offset as u16, 0));

        frame.render_widget(paragraph, area);
    }

    pub fn check_all_complete(&mut self, progress_map: &ProgressMap) -> bool {
        let map = progress_map.lock().unwrap();
        let all_done = self.server_list.iter().all(|server| {
            let hostname = server.split(':').next().unwrap_or(server);
            map.get(hostname)
                .map(|s| s.phase.is_terminal())
                .unwrap_or(false)
        });
        self.all_complete = all_done;
        all_done
    }

    pub fn handle_input(&mut self) -> Result<bool> {
        // Non-blocking check for input with small timeout
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Up => {
                            self.previous();
                            self.ctrl_c_count = 0; // Reset on other key
                            return Ok(false);
                        }
                        KeyCode::Down => {
                            self.next();
                            self.ctrl_c_count = 0; // Reset on other key
                            return Ok(false);
                        }
                        KeyCode::Char('c')
                            if key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::CONTROL) =>
                        {
                            self.ctrl_c_count += 1;
                            if self.ctrl_c_count >= 2 {
                                return Ok(true); // Signal to quit immediately
                            }
                            // Don't reset - let it accumulate
                            return Ok(false);
                        }
                        KeyCode::Char('q') if self.all_complete => {
                            return Ok(true); // Signal to quit
                        }
                        _ => {
                            self.ctrl_c_count = 0; // Reset on other key
                        }
                    }
                }
            }
        }
        Ok(false) // Continue running
    }
}
