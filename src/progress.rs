use ratatui::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum UpdatePhase {
    Pending,
    Connecting,
    RunningBeforeCommand,
    CheckingGit,
    PullingGit,
    Rebuilding { progress: String },
    RunningAfterCommand,
    Success,
    Failed { reason: String },
}

impl UpdatePhase {
    pub fn to_string(&self) -> String {
        match self {
            UpdatePhase::Pending => "Pending".to_string(),
            UpdatePhase::Connecting => "Connecting...".to_string(),
            UpdatePhase::RunningBeforeCommand => "Running before-command...".to_string(),
            UpdatePhase::CheckingGit => "Checking git repo...".to_string(),
            UpdatePhase::PullingGit => "Pulling git updates...".to_string(),
            UpdatePhase::Rebuilding { progress } => {
                if progress.is_empty() {
                    "Rebuilding system...".to_string()
                } else {
                    format!("Rebuilding: {}", progress)
                }
            }
            UpdatePhase::RunningAfterCommand => "Running after-command...".to_string(),
            UpdatePhase::Success => "✅ Success".to_string(),
            UpdatePhase::Failed { reason } => format!("❌ Failed: {}", reason),
        }
    }

    pub fn color(&self) -> Color {
        match self {
            UpdatePhase::Pending => Color::Gray,
            UpdatePhase::Connecting
            | UpdatePhase::RunningBeforeCommand
            | UpdatePhase::CheckingGit
            | UpdatePhase::PullingGit
            | UpdatePhase::Rebuilding { .. }
            | UpdatePhase::RunningAfterCommand => Color::Yellow,
            UpdatePhase::Success => Color::Green,
            UpdatePhase::Failed { .. } => Color::Red,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, UpdatePhase::Success | UpdatePhase::Failed { .. })
    }
}

#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub hostname: String,
    pub phase: UpdatePhase,
    pub output_line: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ServerProgress {
    pub phase: UpdatePhase,
    pub full_output: String,
}

impl ServerProgress {
    pub fn new() -> Self {
        Self {
            phase: UpdatePhase::Pending,
            full_output: String::new(),
        }
    }
}

pub type ProgressMap = Arc<Mutex<HashMap<String, ServerProgress>>>;

pub fn create_progress_map(servers: &[String]) -> ProgressMap {
    let mut map = HashMap::new();
    for server in servers {
        let hostname = server.split(':').next().unwrap_or(server).to_string();
        map.insert(hostname, ServerProgress::new());
    }
    Arc::new(Mutex::new(map))
}

pub fn parse_rebuild_progress(line: &str) -> Option<String> {
    let line_lower = line.to_lowercase();

    // Detect different phases of nixos-rebuild
    if line_lower.contains("downloading") || line_lower.contains("download") {
        // Try to extract package name
        if let Some(start) = line.find('\'') {
            if let Some(end) = line[start + 1..].find('\'') {
                let pkg = &line[start + 1..start + 1 + end];
                // Shorten long package names
                if pkg.len() > 30 {
                    return Some(format!("dl: {}...", &pkg[..27]));
                }
                return Some(format!("dl: {}", pkg));
            }
        }
        return Some("downloading...".to_string());
    }

    if line_lower.contains("copying path") || line_lower.contains("copying") {
        return Some("copying paths...".to_string());
    }

    if line_lower.contains("building") {
        // Try to extract derivation info
        if line.contains("derivation") {
            if let Some(pos) = line.find(char::is_numeric) {
                let num_str: String = line[pos..].chars().take_while(|c| c.is_numeric()).collect();
                if !num_str.is_empty() {
                    return Some(format!("building {} drv", num_str));
                }
            }
        }
        return Some("building...".to_string());
    }

    if line_lower.contains("activating") || line_lower.contains("activation") {
        return Some("activating...".to_string());
    }

    if line_lower.contains("updating") && line_lower.contains("bootloader") {
        return Some("updating bootloader...".to_string());
    }

    if line_lower.contains("reloading") {
        return Some("reloading services...".to_string());
    }

    None
}

pub async fn progress_monitor_task(
    mut rx: mpsc::Receiver<ProgressUpdate>,
    progress_map: ProgressMap,
) {
    while let Some(update) = rx.recv().await {
        let mut map = progress_map.lock().unwrap();
        if let Some(server) = map.get_mut(&update.hostname) {
            server.phase = update.phase;
            if let Some(line) = update.output_line {
                server.full_output.push_str(&line);
                server.full_output.push('\n');
            }
        }
    }
}
