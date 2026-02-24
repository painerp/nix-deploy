use anyhow::Result;
use ssh2::Session;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;
use tokio::sync::mpsc;

use crate::progress::{ProgressUpdate, UpdatePhase};
use crate::ssh_executor::{execute_command_on_channel, execute_command_streaming};

pub async fn update_server_with_progress(
    server_info: &str,
    use_boot: bool,
    forward_agent: bool,
    command: Option<String>,
    run_after: bool,
    progress_tx: mpsc::Sender<ProgressUpdate>,
) -> Result<(String, bool, String)> {
    let server_info = server_info.to_string();

    // Wrap all blocking SSH operations in spawn_blocking
    tokio::task::spawn_blocking(move || {
        update_server_blocking(
            &server_info,
            use_boot,
            forward_agent,
            command,
            run_after,
            progress_tx,
        )
    })
    .await?
}

fn authenticate_ssh_session(
    sess: &Session,
    username: &str,
    hostname: &str,
    progress_tx: &mpsc::Sender<ProgressUpdate>,
) -> Result<bool> {
    let mut authenticated = false;
    let mut auth_errors = Vec::new();

    // Strategy 1: Try file-based SSH keys first
    // This works for both regular SSH and Tailscale SSH (which accepts any key)
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::Connecting,
        output_line: Some("Trying file-based SSH keys...".to_string()),
    });

    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    let key_paths = vec![
        format!("{}/.ssh/id_ed25519", home),
        format!("{}/.ssh/id_rsa", home),
        format!("{}/.ssh/id_ecdsa", home),
        format!("{}/.ssh/id_dsa", home),
    ];

    for key_path in &key_paths {
        if std::path::Path::new(&key_path).exists() {
            match sess.userauth_pubkey_file(username, None, std::path::Path::new(&key_path), None) {
                Ok(()) => {
                    if sess.authenticated() {
                        authenticated = true;
                        let _ = progress_tx.try_send(ProgressUpdate {
                            hostname: hostname.to_string(),
                            phase: UpdatePhase::Connecting,
                            output_line: Some(format!("✓ Authenticated with key: {}", key_path)),
                        });
                        return Ok(authenticated);
                    }
                }
                Err(e) => {
                    auth_errors.push(format!("Key {}: {}", key_path, e));
                }
            }
        }
    }

    // Strategy 2: Try SSH agent (for keys not available as files)
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::Connecting,
        output_line: Some("Trying SSH agent authentication...".to_string()),
    });

    // First attempt: Let libssh2 handle agent authentication automatically
    match sess.userauth_agent(username) {
        Ok(()) => {
            if sess.authenticated() {
                let _ = progress_tx.try_send(ProgressUpdate {
                    hostname: hostname.to_string(),
                    phase: UpdatePhase::Connecting,
                    output_line: Some("✓ Authenticated via SSH agent".to_string()),
                });
                return Ok(true);
            }
        }
        Err(e) => {
            auth_errors.push(format!("SSH agent automatic: {}", e));
        }
    }

    // Strategy 3: Manually iterate through agent keys
    // Some servers require specific keys that the automatic method doesn't try properly
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::Connecting,
        output_line: Some("Trying manual agent key iteration...".to_string()),
    });

    if let Ok(mut agent) = sess.agent() {
        if let Ok(()) = agent.connect() {
            if let Ok(()) = agent.list_identities() {
                if let Ok(identities) = agent.identities() {
                    let _ = progress_tx.try_send(ProgressUpdate {
                        hostname: hostname.to_string(),
                        phase: UpdatePhase::Connecting,
                        output_line: Some(format!("Found {} key(s) in agent", identities.len())),
                    });

                    for (idx, identity) in identities.iter().enumerate() {
                        if sess.authenticated() {
                            break;
                        }

                        let comment = identity.comment();
                        let _ = progress_tx.try_send(ProgressUpdate {
                            hostname: hostname.to_string(),
                            phase: UpdatePhase::Connecting,
                            output_line: Some(format!("  Trying key #{}: {}", idx + 1, comment)),
                        });

                        match agent.userauth(username, identity) {
                            Ok(()) => {
                                if sess.authenticated() {
                                    authenticated = true;
                                    let _ = progress_tx.try_send(ProgressUpdate {
                                        hostname: hostname.to_string(),
                                        phase: UpdatePhase::Connecting,
                                        output_line: Some(format!(
                                            "✓ Authenticated with agent key: {}",
                                            comment
                                        )),
                                    });
                                    break;
                                }
                            }
                            Err(e) => {
                                auth_errors.push(format!("Agent key '{}': {}", comment, e));
                            }
                        }
                    }
                }
            }
            let _ = agent.disconnect();
        }
    }

    if !authenticated {
        let error_msg = format!(
            "Failed to authenticate with SSH for {}.\n\nAttempted methods:\n{}",
            hostname,
            auth_errors.join("\n")
        );
        let _ = progress_tx.try_send(ProgressUpdate {
            hostname: hostname.to_string(),
            phase: UpdatePhase::Failed {
                reason: "SSH authentication failed".to_string(),
            },
            output_line: Some(error_msg),
        });
    }

    Ok(authenticated)
}

fn update_server_blocking(
    server_info: &str,
    use_boot: bool,
    forward_agent: bool,
    command: Option<String>,
    run_after: bool,
    progress_tx: mpsc::Sender<ProgressUpdate>,
) -> Result<(String, bool, String)> {
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

    // Send connecting phase
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::Connecting,
        output_line: Some(format!("Connecting to {}...", ip)),
    });

    // Connect to server with timeout
    let timeout = Duration::from_secs(60);
    let addr = format!("{}:22", ip)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve address: {}", ip))?;

    let tcp = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| anyhow::anyhow!("Connection timeout or failed after 60 seconds: {}", e))?;

    // Set longer timeouts for read/write operations since builds can take a while
    tcp.set_read_timeout(Some(Duration::from_secs(300)))?; // 5 minutes
    tcp.set_write_timeout(Some(Duration::from_secs(300)))?; // 5 minutes

    // Set up SSH session
    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);
    sess.set_timeout(300000); // 300 second (5 minute) timeout
    sess.handshake()?;

    // Keep blocking mode for all operations
    // The session is already in blocking mode by default after handshake
    sess.set_blocking(true);

    // Authenticate
    let username = "root";
    let authenticated = authenticate_ssh_session(&sess, username, hostname, &progress_tx)?;

    if !authenticated {
        return Ok((
            hostname.to_string(),
            false,
            "SSH authentication failed".to_string(),
        ));
    }

    let mut output = String::new();
    let mut success = true;

    // Execute before-command if provided and run_after is false (default)
    if !run_after {
        if let Some(ref cmd) = command {
            let _ = progress_tx.try_send(ProgressUpdate {
                hostname: hostname.to_string(),
                phase: UpdatePhase::RunningBeforeCommand,
                output_line: Some(format!("Running: {}", cmd)),
            });

            output.push_str(&format!("=== Running before-command ===\n"));

            let (buf, exit_status) = execute_command_on_channel(&sess, cmd, forward_agent)?;
            output.push_str(&format!("$ {}\n{}\n", cmd, buf));

            if exit_status != 0 {
                success = false;
                let error_msg = format!("Before-command failed with exit code: {}", exit_status);
                output.push_str(&error_msg);
                output.push('\n');

                let _ = progress_tx.try_send(ProgressUpdate {
                    hostname: hostname.to_string(),
                    phase: UpdatePhase::Failed {
                        reason: error_msg.clone(),
                    },
                    output_line: None,
                });

                return Ok((hostname.to_string(), success, output));
            }
        }
    }

    // Check git repo
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::CheckingGit,
        output_line: Some("Checking for git repository...".to_string()),
    });

    let (git_check, _) = execute_command_on_channel(
        &sess,
        "test -d /etc/nixos/.git || echo 'No git repo found'",
        forward_agent,
    )?;

    if git_check.contains("No git repo found") {
        let error_msg = "No git repository found in /etc/nixos".to_string();
        let _ = progress_tx.try_send(ProgressUpdate {
            hostname: hostname.to_string(),
            phase: UpdatePhase::Failed {
                reason: error_msg.clone(),
            },
            output_line: None,
        });
        return Ok((hostname.to_string(), false, error_msg));
    }

    // Git pull
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::PullingGit,
        output_line: Some("Running git pull...".to_string()),
    });

    let git_cmd = "cd /etc/nixos && git pull --verbose";
    let (buf, exit_status) =
        execute_command_streaming(&sess, git_cmd, forward_agent, &progress_tx, hostname, false)?;
    output.push_str(&format!("$ {}\n{}\n", git_cmd, buf));

    if exit_status != 0 {
        success = false;
        let error_msg = format!("Git pull failed with exit code: {}", exit_status);
        output.push_str(&error_msg);
        output.push('\n');

        let _ = progress_tx.try_send(ProgressUpdate {
            hostname: hostname.to_string(),
            phase: UpdatePhase::Failed { reason: error_msg },
            output_line: None,
        });

        return Ok((hostname.to_string(), success, output));
    }

    // nixos-rebuild
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::Rebuilding {
            progress: String::new(),
        },
        output_line: Some("Starting system rebuild...".to_string()),
    });

    let rebuild_mode = if use_boot { "boot" } else { "switch" };
    let rebuild_cmd = format!(
        "nixos-rebuild {} --flake \"/etc/nixos#{}\" --no-write-lock-file",
        rebuild_mode, flake_hostname
    );

    let (buf, exit_status) = execute_command_streaming(
        &sess,
        &rebuild_cmd,
        forward_agent,
        &progress_tx,
        hostname,
        true, // is_rebuild = true
    )?;
    output.push_str(&format!("$ {}\n{}\n", rebuild_cmd, buf));

    if exit_status != 0 {
        success = false;
        let error_msg = format!("nixos-rebuild failed with exit code: {}", exit_status);
        output.push_str(&error_msg);
        output.push('\n');

        let _ = progress_tx.try_send(ProgressUpdate {
            hostname: hostname.to_string(),
            phase: UpdatePhase::Failed { reason: error_msg },
            output_line: None,
        });

        return Ok((hostname.to_string(), success, output));
    }

    // Execute after-command if provided, run_after is true, and previous commands succeeded
    if success && run_after {
        if let Some(ref cmd) = command {
            let _ = progress_tx.try_send(ProgressUpdate {
                hostname: hostname.to_string(),
                phase: UpdatePhase::RunningAfterCommand,
                output_line: Some(format!("Running: {}", cmd)),
            });

            output.push_str(&format!("=== Running after-command ===\n"));
            let (buf, exit_status) = execute_command_on_channel(&sess, cmd, forward_agent)?;
            output.push_str(&format!("$ {}\n{}\n", cmd, buf));

            if exit_status != 0 {
                success = false;
                let error_msg = format!("After-command failed with exit code: {}", exit_status);
                output.push_str(&error_msg);
                output.push('\n');

                let _ = progress_tx.try_send(ProgressUpdate {
                    hostname: hostname.to_string(),
                    phase: UpdatePhase::Failed { reason: error_msg },
                    output_line: None,
                });

                return Ok((hostname.to_string(), success, output));
            }
        }
    }

    // Success!
    let _ = progress_tx.try_send(ProgressUpdate {
        hostname: hostname.to_string(),
        phase: UpdatePhase::Success,
        output_line: None,
    });

    Ok((hostname.to_string(), success, output))
}
