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
        "view" => cmd_view(&args[2..]),
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
    let mut replay_path: Option<PathBuf> = None;
    let mut delete_replay = false;
    let mut socket_arg: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--replay" => {
                index += 1;
                if index >= args.len() {
                    anyhow::bail!("--replay requires a path");
                }
                replay_path = Some(PathBuf::from(&args[index]));
            }
            "--delete-replay" => {
                delete_replay = true;
            }
            other => {
                socket_arg = Some(other.to_string());
            }
        }
        index += 1;
    }

    let socket_arg = socket_arg
        .ok_or_else(|| anyhow::anyhow!("usage: pty-server attach [--replay <path>] <name>"))?;
    let socket_path = if socket_arg.ends_with(".sock") || socket_arg.starts_with('/') {
        PathBuf::from(&socket_arg)
    } else {
        default_socket_dir().join(format!("{socket_arg}.sock"))
    };

    if !socket_path.exists() {
        anyhow::bail!("session not found: {}", socket_path.display());
    }

    // Replay frozen-state bytes from a previous incarnation of this session
    // so the user sees the prior context as scrollback before the live shell
    // starts producing output.
    if let Some(path) = replay_path.as_ref() {
        if let Ok(bytes) = std::fs::read(path) {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(&bytes).ok();
            // Always end on main screen with a separator so the new shell's
            // prompt lands below the prior content.
            stdout
                .write_all(b"\x1b[?1049l\x1b[m\r\n\x1b[2m\xe2\x94\x80 resumed \xe2\x94\x80\x1b[0m\r\n")
                .ok();
            stdout.flush().ok();
        }
        if delete_replay {
            std::fs::remove_file(path).ok();
        }
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

fn cmd_view(args: &[String]) -> anyhow::Result<()> {
    if args.is_empty() {
        anyhow::bail!("usage: pty-server view <name-or-state-path>");
    }

    let state_path = if args[0].ends_with(".state") || args[0].starts_with('/') {
        PathBuf::from(&args[0])
    } else {
        default_socket_dir().join(format!("{}.state", args[0]))
    };

    if !state_path.exists() {
        anyhow::bail!(
            "no frozen state found for '{}' at {}",
            args[0],
            state_path.display()
        );
    }

    let bytes = std::fs::read(&state_path)?;
    use std::io::Write;
    {
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(&bytes)?;
        stdout.write_all(
            b"\r\n\x1b[2m[ session ended \xe2\x80\x94 close this tab to dismiss ]\x1b[0m\r\n",
        )?;
        stdout.flush()?;
    }

    // Idle so the tab stays open showing the frozen state. Sleeping for a long
    // duration is fine; closing the terminal tab will SIGTERM us.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

fn print_usage() {
    eprintln!(
        "pty-server: persistent terminal session manager

usage:
    pty-server create <name> [--cwd <dir>] [--] [command...]
    pty-server attach <name>
    pty-server view <name>
    pty-server list
    pty-server kill <name>

examples:
    pty-server create claude-auth -- claude --dangerously-skip-permissions
    pty-server attach claude-auth
    pty-server view claude-auth
    pty-server list
    pty-server kill claude-auth"
    );
}
