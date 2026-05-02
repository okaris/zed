use std::path::PathBuf;
use std::process;

use pty_server::{SessionClient, SessionServer};

fn default_socket_dir() -> PathBuf {
    let uid = unsafe { libc::getuid() };
    let runtime_dir = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| format!("/tmp/pty-servers-{uid}"));
    PathBuf::from(runtime_dir).join("pty-servers")
}

fn default_store_path() -> PathBuf {
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            format!("{home}/.config")
        });
    PathBuf::from(config_dir).join("pty-servers/sessions.json")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "create" => cmd_create(&args[2..]),
        "attach" => cmd_attach(&args[2..]),
        "list" => cmd_list(&args[2..]),
        "kill" => cmd_kill(&args[2..]),
        "help" | "--help" | "-h" => {
            print_usage();
            Ok(())
        }
        other => {
            eprintln!("unknown command: {other}");
            print_usage();
            process::exit(1);
        }
    };

    if let Err(error) = result {
        eprintln!("error: {error}");
        process::exit(1);
    }
}

fn cmd_create(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: pty-server create <name> [--cwd <dir>] [--] <command...>");
    }

    let name = &args[0];
    let mut working_dir = std::env::current_dir()?;
    let mut command_start = 1;

    // Parse optional flags
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--cwd" => {
                index += 1;
                if index >= args.len() {
                    anyhow::bail!("--cwd requires a path");
                }
                working_dir = PathBuf::from(&args[index]);
                command_start = index + 1;
            }
            "--" => {
                command_start = index + 1;
                break;
            }
            _ => {
                command_start = index;
                break;
            }
        }
        index += 1;
    }

    let command: Vec<String> = if command_start < args.len() {
        args[command_start..].to_vec()
    } else {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
        vec![shell]
    };

    let socket_dir = default_socket_dir();
    let socket_path =
        SessionServer::create(name, &command, &working_dir, &socket_dir)?;

    println!("{}", socket_path.display());
    Ok(())
}

fn cmd_attach(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: pty-server attach <name-or-socket-path>");
    }

    let socket_path = if args[0].ends_with(".sock") || args[0].starts_with('/') {
        PathBuf::from(&args[0])
    } else {
        default_socket_dir().join(format!("{}.sock", args[0]))
    };

    if !socket_path.exists() {
        anyhow::bail!("session not found: {}", socket_path.display());
    }

    let mut client = SessionClient::connect(&socket_path)?;

    match client.run_attach()? {
        Some(status) => process::exit(status),
        None => Ok(()),
    }
}

fn cmd_list(_args: &[String]) -> anyhow::Result<()> {
    let socket_dir = default_socket_dir();
    let sessions = SessionServer::list_sessions(&socket_dir)?;

    if sessions.is_empty() {
        println!("no sessions");
        return Ok(());
    }

    for (name, alive) in &sessions {
        let status = if *alive { "running" } else { "dead" };
        println!("{name}\t{status}");
    }

    Ok(())
}

fn cmd_kill(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: pty-server kill <name>");
    }

    let socket_path = default_socket_dir().join(format!("{}.sock", args[0]));
    SessionServer::kill_session(&socket_path)?;
    println!("killed: {}", args[0]);
    Ok(())
}

fn print_usage() {
    eprintln!(
        "pty-server: persistent terminal session manager

usage:
    pty-server create <name> [--cwd <dir>] [--] [command...]
    pty-server attach <name>
    pty-server list
    pty-server kill <name>

examples:
    pty-server create claude-auth -- claude --dangerously-skip-permissions
    pty-server attach claude-auth
    pty-server list
    pty-server kill claude-auth"
    );
}
