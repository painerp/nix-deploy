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

    // Add timeout to SSH connection (30 seconds)
    let timeout = Duration::from_secs(30);
    let addr = format!("{}:22", ip)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("Failed to resolve address: {}", ip))?;

    let tcp = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| anyhow::anyhow!("Connection timeout or failed after 30 seconds: {}", e))?;

    // Set read/write timeouts on the socket
    tcp.set_read_timeout(Some(Duration::from_secs(30)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(30)))?;

    let mut sess = Session::new()?;
    sess.set_tcp_stream(tcp);

    // Set timeout for SSH handshake and operations (30 seconds in milliseconds)
    sess.set_timeout(30000);
    sess.handshake()?;

    // Try multiple authentication methods
    let username = "root";
    let mut authenticated = false;

    // First, try SSH agent authentication
    if let Ok(()) = sess.userauth_agent(username) {
        authenticated = true;
    }

    // If agent auth failed, try public key authentication from default locations
    if !authenticated {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        let key_paths = vec![
            format!("{}/.ssh/id_ed25519", home),
            format!("{}/.ssh/id_rsa", home),
            format!("{}/.ssh/id_ecdsa", home),
        ];

        for key_path in key_paths {
            if std::path::Path::new(&key_path).exists() {
                if let Ok(()) =
                    sess.userauth_pubkey_file(username, None, std::path::Path::new(&key_path), None)
                {
                    authenticated = true;
                    break;
                }
            }
        }
    }

    if !authenticated {
        return Ok((
            hostname.to_string(),
            false,
            "Failed to authenticate with SSH. Please ensure SSH agent is running or SSH keys are available.".to_string(),
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
