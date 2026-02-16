use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
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
    auto_scroll: bool,
    max_scroll: usize,
    server_list_area: Rect,
    output_area: Rect,
}

impl ProgressTui {
    pub fn new(servers: Vec<String>) -> Self {
        Self {
            server_list: servers,
            selected_index: 0,
            scroll_offset: 0,
            all_complete: false,
            ctrl_c_count: 0,
            auto_scroll: true,
            max_scroll: 0,
            server_list_area: Rect::default(),
            output_area: Rect::default(),
        }
    }

    pub fn next(&mut self) {
        if !self.server_list.is_empty() {
            self.selected_index = (self.selected_index + 1) % self.server_list.len();
            self.scroll_offset = 0;
            self.auto_scroll = true;
        }
    }

    pub fn previous(&mut self) {
        if !self.server_list.is_empty() {
            self.selected_index = if self.selected_index == 0 {
                self.server_list.len() - 1
            } else {
                self.selected_index - 1
            };
            self.scroll_offset = 0;
            self.auto_scroll = true;
        }
    }

    pub fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset = self.scroll_offset.saturating_sub(5);
            self.auto_scroll = false;
        }
    }

    pub fn scroll_down(&mut self) {
        if self.scroll_offset < self.max_scroll {
            self.scroll_offset = (self.scroll_offset + 5).min(self.max_scroll);
            // Re-enable auto-scroll if we're at the bottom
            if self.scroll_offset >= self.max_scroll {
                self.auto_scroll = true;
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, progress_map: &ProgressMap) {
        let area = frame.area();

        // Split the screen: 40% for server list, 60% for output
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(area);

        // Store areas for mouse click detection
        self.server_list_area = chunks[0];
        self.output_area = chunks[1];

        // Lock the progress map once for the entire render
        let map = progress_map.lock().unwrap();

        // Render server list
        self.render_server_list(frame, chunks[0], &map);

        // Render output pane
        self.render_output_pane(frame, chunks[1], &map);
    }

    fn render_server_list(
        &self,
        frame: &mut Frame,
        area: Rect,
        map: &std::collections::HashMap<String, crate::progress::ServerProgress>,
    ) {
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

    fn render_output_pane(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        map: &std::collections::HashMap<String, crate::progress::ServerProgress>,
    ) {
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

        // Calculate max scroll position
        self.max_scroll = line_count.saturating_sub(visible_lines);

        // Auto-scroll to bottom if enabled
        if self.auto_scroll {
            self.scroll_offset = self.max_scroll;
        }

        let selected_hostname = selected_server
            .map(|s| s.split(':').next().unwrap_or(s))
            .unwrap_or("None");

        let scroll_indicator = if self.auto_scroll {
            ""
        } else {
            " [Manual Scroll - PgDn to resume auto-scroll]"
        };

        let paragraph = Paragraph::new(output)
            .block(
                Block::default()
                    .title(format!("Output: {}{}", selected_hostname, scroll_indicator))
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
        // Very short poll timeout for responsive input
        // This is the only delay in the main loop, so keep it minimal
        if event::poll(Duration::from_millis(10))? {
            match event::read()? {
                Event::Key(key) => {
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
                            KeyCode::PageUp => {
                                self.scroll_up();
                                self.ctrl_c_count = 0;
                                return Ok(false);
                            }
                            KeyCode::PageDown => {
                                self.scroll_down();
                                self.ctrl_c_count = 0;
                                return Ok(false);
                            }
                            KeyCode::Char('c')
                                if key
                                    .modifiers
                                    .contains(crossterm::event::KeyModifiers::CONTROL) =>
                            {
                                self.ctrl_c_count += 1;
                                // If all complete, quit immediately
                                // Otherwise require 2 presses to force quit
                                if self.all_complete || self.ctrl_c_count >= 2 {
                                    return Ok(true); // Signal to quit
                                }
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
                Event::Mouse(mouse) => {
                    // Skip mouse move and drag events early - they generate tons of events
                    if !matches!(
                        mouse.kind,
                        MouseEventKind::Down(_)
                            | MouseEventKind::ScrollUp
                            | MouseEventKind::ScrollDown
                    ) {
                        return Ok(false);
                    }

                    self.ctrl_c_count = 0; // Reset on mouse events
                    match mouse.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            // Check if click is in server list area
                            if mouse.column >= self.server_list_area.x
                                && mouse.column
                                    < self.server_list_area.x + self.server_list_area.width
                                && mouse.row >= self.server_list_area.y
                                && mouse.row
                                    < self.server_list_area.y + self.server_list_area.height
                            {
                                // Calculate which server was clicked (accounting for border)
                                let relative_y =
                                    mouse.row.saturating_sub(self.server_list_area.y + 1);
                                if relative_y < self.server_list.len() as u16 {
                                    self.selected_index = relative_y as usize;
                                    self.scroll_offset = 0;
                                    self.auto_scroll = true;
                                }
                            }
                        }
                        MouseEventKind::ScrollUp => {
                            // Check if mouse is over output area for scrolling
                            if mouse.column >= self.output_area.x
                                && mouse.column < self.output_area.x + self.output_area.width
                                && mouse.row >= self.output_area.y
                                && mouse.row < self.output_area.y + self.output_area.height
                            {
                                self.scroll_up();
                            }
                        }
                        MouseEventKind::ScrollDown => {
                            // Check if mouse is over output area for scrolling
                            if mouse.column >= self.output_area.x
                                && mouse.column < self.output_area.x + self.output_area.width
                                && mouse.row >= self.output_area.y
                                && mouse.row < self.output_area.y + self.output_area.height
                            {
                                self.scroll_down();
                            }
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        Ok(false) // Continue running
    }
}
