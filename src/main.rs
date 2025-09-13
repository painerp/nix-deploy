use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures::future::join_all;
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use serde::Deserialize;
use ssh2::Session;
use std::{collections::HashMap, io::Read, net::TcpStream, process::Command};
use tokio::runtime::Runtime;

#[derive(Debug, Deserialize)]
struct TailscaleStatus {
    #[serde(rename = "Self")]
    self_info: TailscaleSelf,
    #[serde(rename = "Peer")]
    peers: HashMap<String, TailscalePeer>,
}

#[derive(Debug, Deserialize)]
struct TailscaleSelf {
    #[serde(rename = "HostName")]
    host_name: String,
}

#[derive(Debug, Deserialize)]
struct TailscalePeer {
    #[serde(rename = "HostName")]
    host_name: String,
    #[serde(rename = "TailscaleIPs")]
    ips: Vec<String>,
    #[serde(rename = "Online")]
    online: bool,
}

#[derive(Parser)]
#[command(version, about = "Update NixOS servers")]
struct Args {
    #[arg(short, long)]
    boot: bool,
}

struct ServerSelector {
    servers: Vec<String>,
    selected: Vec<bool>,
    state: ListState,
}

impl ServerSelector {
    fn new(servers: Vec<String>) -> Self {
        let len = servers.len();
        let mut state = ListState::default();
        state.select(Some(0));
        Self {
            servers,
            selected: vec![false; len],
            state,
        }
    }

    fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => (i + 1) % self.servers.len(),
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.servers.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    fn toggle_selected(&mut self) {
        if let Some(i) = self.state.selected() {
            self.selected[i] = !self.selected[i];
        }
    }

    fn toggle_all(&mut self) {
        let all_selected = self.selected.iter().all(|&s| s);
        self.selected = vec![!all_selected; self.servers.len()];
    }

    fn get_selected_servers(&self) -> Vec<String> {
        self.servers
            .iter()
            .zip(self.selected.iter())
            .filter_map(
                |(server, &selected)| {
                    if selected { Some(server.clone()) } else { None }
                },
            )
            .collect()
    }
}

fn get_nixos_servers() -> Result<Vec<String>> {
    let output = Command::new("tailscale")
        .arg("status")
        .arg("--json")
        .output()
        .context("Failed to execute tailscale command")?;

    let status: TailscaleStatus =
        serde_json::from_slice(&output.stdout).context("Failed to parse tailscale status JSON")?;

    let mut nixos_servers = Vec::new();
    for (_, peer) in status.peers {
        if peer.host_name.starts_with("nix") && !peer.ips.is_empty() && peer.online {
            nixos_servers.push(format!("{}:{}", peer.host_name, peer.ips[0]));
        }
    }

    nixos_servers.sort_by(|a, b| {
        let a_host = a.split(":").next().unwrap_or("");
        let b_host = b.split(":").next().unwrap_or("");
        a_host.cmp(b_host)
    });

    Ok(nixos_servers)
}

fn update_server(server_info: &str, use_boot: bool) -> Result<(String, bool, String)> {
    let parts: Vec<&str> = server_info.split(':').collect();
    if parts.len() < 2 {
        return Ok((
            server_info.to_string(),
            false,
            "Invalid server info format".to_string(),
        ));
    }

    let hostname = parts[0];
    let ip = parts[1];

    let flake_hostname = if hostname.starts_with("nix") {
        &hostname[3..]
    } else {
        hostname
    };

    let tcp = TcpStream::connect(format!("{}:22", ip))?;
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.handshake()?;

    sess.userauth_agent("root")?;

    let mut output = String::new();
    let mut success = true;

    let mut channel = sess.channel_session()?;
    channel.exec("test -d /etc/nixos/.git || echo 'No git repo found'")?;
    let mut git_check = String::new();
    channel.read_to_string(&mut git_check)?;

    if git_check.contains("No git repo found") {
        return Ok((
            hostname.to_string(),
            false,
            "No git repository found in /etc/nixos".to_string(),
        ));
    }

    let rebuild_mode = if use_boot { "boot" } else { "switch" };
    let rebuild_cmd = format!(
        "nixos-rebuild {} --flake \"/etc/nixos#{}\" --no-write-lock-file",
        rebuild_mode, flake_hostname
    );

    for cmd in &["cd /etc/nixos && git pull --verbose", &rebuild_cmd] {
        let mut channel = sess.channel_session()?;
        channel.exec(cmd)?;

        let mut buf = String::new();
        channel.read_to_string(&mut buf)?;
        output.push_str(&format!("$ {}\n{}\n", cmd, buf));

        let exit_status = channel.exit_status()?;
        if exit_status != 0 {
            success = false;
            output.push_str(&format!("Command failed with exit code: {}\n", exit_status));
            break;
        }
    }

    Ok((hostname.to_string(), success, output))
}

fn run_tui() -> Result<Vec<String>> {
    enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), EnterAlternateScreen)?;

    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;

    let nixos_servers = get_nixos_servers()?;
    let mut selector = ServerSelector::new(nixos_servers);

    let result = loop {
        terminal.draw(|frame| {
            let area = frame.area();

            let items: Vec<ListItem> = selector
                .servers
                .iter()
                .enumerate()
                .map(|(i, server)| {
                    let prefix = if selector.selected[i] { "[X] " } else { "[ ] " };
                    ListItem::new(format!("{}{}", prefix, server))
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .title("NixOS Servers")
                        .borders(Borders::ALL),
                )
                .highlight_style(Style::default().fg(Color::Yellow))
                .highlight_symbol("> ");

            frame.render_stateful_widget(list, area, &mut selector.state);

            let help_text = "\nPress Space to select, A to toggle all, Enter to confirm, Q to quit";
            let help_paragraph =
                Paragraph::new(help_text).block(Block::default().borders(Borders::NONE));

            let help_area = Rect::new(area.x, area.bottom() - 2, area.width, 2);

            frame.render_widget(help_paragraph, help_area);
        })?;

        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
                match key.code {
                    KeyCode::Char('q') => break Vec::new(), // Cancel operation
                    KeyCode::Char(' ') => selector.toggle_selected(),
                    KeyCode::Char('a') => selector.toggle_all(),
                    KeyCode::Down => selector.next(),
                    KeyCode::Up => selector.previous(),
                    KeyCode::Enter => break selector.get_selected_servers(),
                    _ => {}
                }
            }
        }
    };

    disable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), LeaveAlternateScreen)?;

    Ok(result)
}

fn main() -> Result<()> {
    let args = Args::parse();

    let selected_servers = run_tui()?;

    if selected_servers.is_empty() {
        println!("No servers selected. Exiting.");
        return Ok(());
    }

    println!("Updating selected servers: {:?}", selected_servers);

    let use_boot = args.boot;

    let rt = Runtime::new()?;
    let results = rt.block_on(async {
        let update_tasks = selected_servers.iter().map(|server| {
            let server_clone = server.clone();
            tokio::spawn(async move {
                println!("Updating server: {}", server_clone);
                match update_server(&server_clone, use_boot) {
                    Ok((hostname, success, output)) => (hostname, success, output),
                    Err(e) => (server_clone, false, format!("Error: {}", e)),
                }
            })
        });

        let task_results = join_all(update_tasks).await;

        task_results
            .into_iter()
            .map(|r| {
                r.unwrap_or_else(|e| ("Unknown".to_string(), false, format!("Task error: {}", e)))
            })
            .collect::<Vec<_>>()
    });

    println!("\n--- Update Results ---");
    for (hostname, success, output) in results {
        if success {
            println!("✅ {}: Update successful", hostname);
        } else {
            println!("❌ {}: Update failed", hostname);
            println!("Output:\n{}", output);
        }
    }

    Ok(())
}
