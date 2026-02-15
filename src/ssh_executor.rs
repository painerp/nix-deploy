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

    // Request pseudo-terminal to get unbuffered output
    if is_rebuild {
        channel.request_pty("xterm", None, None)?;
    }

    channel.exec(command)?;

    let mut full_output = String::new();
    let mut buffer = [0u8; 4096];
    let mut line_buffer = String::new();

    // Read from the channel in chunks
    loop {
        match channel.read(&mut buffer) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buffer[..n]).to_string();
                full_output.push_str(&chunk);

                // Strip ANSI escape codes if we're using PTY
                let display_chunk = if is_rebuild {
                    let stripped = strip_ansi_escapes::strip(chunk.as_bytes());
                    String::from_utf8_lossy(&stripped).to_string()
                } else {
                    chunk
                };

                line_buffer.push_str(&display_chunk);

                // Process complete lines
                // Handle both \n and \r\n as line terminators
                // \r without \n means the line is being overwritten (progress update)
                loop {
                    // Look for newline sequences
                    if let Some(newline_pos) = line_buffer.find('\n') {
                        // Found a complete line ending with \n
                        let mut line = line_buffer[..newline_pos].to_string();
                        line_buffer = line_buffer[newline_pos + 1..].to_string();

                        // Remove trailing \r if present (for \r\n sequences)
                        if line.ends_with('\r') {
                            line.pop();
                        }

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
                    } else if let Some(cr_pos) = line_buffer.find('\r') {
                        // Found a \r without \n - this is a progress update that overwrites the line
                        // Check if there's more content after this \r
                        if cr_pos + 1 < line_buffer.len() {
                            // There's content after \r - discard everything before \r and continue
                            line_buffer = line_buffer[cr_pos + 1..].to_string();
                            // Don't break - continue processing in case there are more \r or \n
                            continue;
                        } else {
                            // \r is at the end of buffer - wait for more data
                            break;
                        }
                    } else {
                        // No line terminators found - wait for more data
                        break;
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // No data available, sleep briefly
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Error reading from channel: {}", e));
            }
        }

        // Check if channel is done
        if channel.eof() {
            break;
        }
    }

    // Process any remaining content in line buffer
    if !line_buffer.trim().is_empty() {
        let trimmed = line_buffer.trim();
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
            let _ = progress_tx.try_send(ProgressUpdate {
                hostname: hostname.to_string(),
                phase: UpdatePhase::PullingGit,
                output_line: Some(trimmed.to_string()),
            });
        }
    }

    // Wait for channel to close and get exit status
    channel.wait_close()?;
    let exit_status = channel.exit_status()?;
    Ok((full_output, exit_status))
}
