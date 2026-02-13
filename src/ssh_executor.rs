use anyhow::Result;
use ssh2::Session;
use std::io::Read;
use tokio::sync::mpsc;

use crate::progress::{parse_rebuild_progress, ProgressUpdate, UpdatePhase};

pub fn execute_command_on_channel(
    sess: &Session,
    command: &str,
    forward_agent: bool,
) -> Result<(String, i32)> {
    let mut channel = sess.channel_session()?;

    // Request agent forwarding on this channel if enabled
    if forward_agent {
        channel.request_auth_agent_forwarding()?;
    }

    channel.exec(command)?;

    let mut output = String::new();
    channel.read_to_string(&mut output)?;

    let exit_status = channel.exit_status()?;

    Ok((output, exit_status))
}

pub fn execute_command_streaming(
    sess: &Session,
    command: &str,
    forward_agent: bool,
    progress_tx: &mpsc::Sender<ProgressUpdate>,
    hostname: &str,
    is_rebuild: bool,
) -> Result<(String, i32)> {
    let mut channel = sess.channel_session()?;

    // Request agent forwarding on this channel if enabled
    if forward_agent {
        channel.request_auth_agent_forwarding()?;
    }

    channel.exec(command)?;

    let mut full_output = String::new();
    let mut buffer = [0u8; 8192];
    let mut line_buffer = String::new();

    loop {
        match channel.read(&mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buffer[..n]).to_string();
                full_output.push_str(&chunk);
                line_buffer.push_str(&chunk);

                // Process complete lines
                while let Some(newline_pos) = line_buffer.find('\n') {
                    let line = line_buffer[..newline_pos].to_string();
                    line_buffer = line_buffer[newline_pos + 1..].to_string();

                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        // Parse rebuild progress if this is a rebuild command
                        if is_rebuild {
                            if let Some(progress) = parse_rebuild_progress(trimmed) {
                                let _ = progress_tx.try_send(ProgressUpdate {
                                    hostname: hostname.to_string(),
                                    phase: UpdatePhase::Rebuilding { progress },
                                    output_line: Some(trimmed.to_string()),
                                });
                            } else {
                                let _ = progress_tx.try_send(ProgressUpdate {
                                    hostname: hostname.to_string(),
                                    phase: UpdatePhase::Rebuilding {
                                        progress: String::new(),
                                    },
                                    output_line: Some(trimmed.to_string()),
                                });
                            }
                        } else {
                            // For non-rebuild commands, just send the output line
                            let _ = progress_tx.try_send(ProgressUpdate {
                                hostname: hostname.to_string(),
                                phase: UpdatePhase::PullingGit,
                                output_line: Some(trimmed.to_string()),
                            });
                        }
                    }
                }
            }
            Err(e) => {
                // Check if it's just EOF
                if channel.eof() {
                    break;
                }
                return Err(anyhow::anyhow!("Error reading from channel: {}", e));
            }
        }
    }

    // Send any remaining content in line buffer
    if !line_buffer.trim().is_empty() {
        let _ = progress_tx.try_send(ProgressUpdate {
            hostname: hostname.to_string(),
            phase: if is_rebuild {
                UpdatePhase::Rebuilding {
                    progress: String::new(),
                }
            } else {
                UpdatePhase::PullingGit
            },
            output_line: Some(line_buffer.trim().to_string()),
        });
    }

    let exit_status = channel.exit_status()?;
    Ok((full_output, exit_status))
}
