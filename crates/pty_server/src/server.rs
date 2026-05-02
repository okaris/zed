use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process;

use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};

use crate::protocol::{Packet, PacketType};

const READ_BUF_SIZE: usize = 8192;

/// Represents a running session server that holds a PTY alive.
pub struct SessionServer {
    session_name: String,
    socket_path: PathBuf,
    pty_master: OwnedFd,
    child_pid: Pid,
}

fn openpty_libc() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = 0;
    let mut slave: libc::c_int = 0;
    let result = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe {
        (
            OwnedFd::from_raw_fd(master),
            OwnedFd::from_raw_fd(slave),
        )
    })
}

use std::os::fd::FromRawFd;

fn poll_fds(fds: &[libc::c_int], timeout_ms: i32) -> io::Result<Vec<bool>> {
    let mut pollfds: Vec<libc::pollfd> = fds
        .iter()
        .map(|&fd| libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        })
        .collect();

    let result = unsafe { libc::poll(pollfds.as_mut_ptr(), pollfds.len() as _, timeout_ms) };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(vec![false; fds.len()]);
        }
        return Err(error);
    }

    Ok(pollfds.iter().map(|pfd| pfd.revents & libc::POLLIN != 0).collect())
}

impl SessionServer {
    /// Create a new persistent session.
    ///
    /// This forks a child process running `command` inside a PTY,
    /// then listens on a unix socket for clients to attach/detach.
    ///
    /// This function daemonizes: the calling process returns immediately,
    /// and the server runs in a background process.
    pub fn create(
        session_name: &str,
        command: &[String],
        working_dir: &Path,
        socket_dir: &Path,
    ) -> anyhow::Result<PathBuf> {
        if command.is_empty() {
            anyhow::bail!("command must not be empty");
        }

        fs::create_dir_all(socket_dir)?;
        let socket_path = socket_dir.join(format!("{session_name}.sock"));

        if socket_path.exists() {
            if std::os::unix::net::UnixStream::connect(&socket_path).is_ok() {
                anyhow::bail!("session '{session_name}' is already running");
            }
            fs::remove_file(&socket_path)?;
        }

        match unsafe { unistd::fork() }? {
            ForkResult::Parent { child: _ } => {
                std::thread::sleep(std::time::Duration::from_millis(100));
                Ok(socket_path)
            }
            ForkResult::Child => {
                unistd::setsid().ok();

                let (master, slave) = openpty_libc()?;

                match unsafe { unistd::fork() }? {
                    ForkResult::Parent { child: grandchild_pid } => {
                        drop(slave);

                        let server = SessionServer {
                            session_name: session_name.to_string(),
                            socket_path: socket_path.clone(),
                            pty_master: master,
                            child_pid: grandchild_pid,
                        };

                        if let Err(error) = server.run() {
                            eprintln!("session server error: {error}");
                        }
                        process::exit(0);
                    }
                    ForkResult::Child => {
                        drop(master);
                        setup_slave_and_exec(&slave, working_dir, command);
                    }
                }
            }
        }
    }

    /// Create a session without daemonizing (for testing).
    pub fn create_foreground(
        session_name: &str,
        command: &[String],
        working_dir: &Path,
        socket_dir: &Path,
    ) -> anyhow::Result<(Self, PathBuf)> {
        if command.is_empty() {
            anyhow::bail!("command must not be empty");
        }

        fs::create_dir_all(socket_dir)?;
        let socket_path = socket_dir.join(format!("{session_name}.sock"));

        if socket_path.exists() {
            fs::remove_file(&socket_path)?;
        }

        let (master, slave) = openpty_libc()?;

        match unsafe { unistd::fork() }? {
            ForkResult::Parent { child: child_pid } => {
                drop(slave);

                let server = SessionServer {
                    session_name: session_name.to_string(),
                    socket_path: socket_path.clone(),
                    pty_master: master,
                    child_pid,
                };

                Ok((server, socket_path))
            }
            ForkResult::Child => {
                drop(master);
                setup_slave_and_exec(&slave, working_dir, command);
            }
        }
    }

    /// Main server loop: accept clients, shuttle bytes between PTY and clients.
    pub fn run(self) -> anyhow::Result<()> {
        let listener = UnixListener::bind(&self.socket_path)?;
        listener.set_nonblocking(true)?;

        let pty_fd = self.pty_master.as_raw_fd();
        let listener_fd = listener.as_raw_fd();

        let mut clients: Vec<ClientConn> = Vec::new();
        let mut pty_buf = [0u8; READ_BUF_SIZE];

        loop {
            // Check if child is still alive
            match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, status)) => {
                    let exit_packet = Packet::exit(status);
                    for client in &mut clients {
                        exit_packet.write_to(&mut client.stream).ok();
                    }
                    self.cleanup();
                    return Ok(());
                }
                Ok(WaitStatus::Signaled(_, signal, _)) => {
                    let exit_packet = Packet::exit(128 + signal as i32);
                    for client in &mut clients {
                        exit_packet.write_to(&mut client.stream).ok();
                    }
                    self.cleanup();
                    return Ok(());
                }
                _ => {}
            }

            // Check if socket file was removed (external kill signal)
            if !self.socket_path.exists() {
                signal::kill(self.child_pid, Signal::SIGTERM).ok();
                return Ok(());
            }

            // Build poll fd list: [pty, listener, client0, client1, ...]
            let mut poll_fd_list = vec![pty_fd, listener_fd];
            for client in &clients {
                poll_fd_list.push(client.stream.as_raw_fd());
            }

            let ready = poll_fds(&poll_fd_list, 1000)?;

            // Accept new client connections
            if ready.get(1).copied().unwrap_or(false) {
                if let Ok((stream, _)) = listener.accept() {
                    stream.set_nonblocking(false).ok();
                    clients.push(ClientConn { stream });
                }
            }

            // Read from PTY, send to all clients
            if ready.first().copied().unwrap_or(false) {
                match unsafe { libc::read(pty_fd, pty_buf.as_mut_ptr() as _, pty_buf.len()) } {
                    n if n <= 0 => {
                        let exit_packet = Packet::exit(0);
                        for client in &mut clients {
                            exit_packet.write_to(&mut client.stream).ok();
                        }
                        self.cleanup();
                        return Ok(());
                    }
                    n => {
                        let packet = Packet::content(&pty_buf[..n as usize]);
                        clients.retain_mut(|client| {
                            packet.write_to(&mut client.stream).is_ok()
                        });
                    }
                }
            }

            // Read from clients, handle packets
            let mut dead_clients = Vec::new();
            for (index, client) in clients.iter_mut().enumerate() {
                if !ready.get(index + 2).copied().unwrap_or(false) {
                    continue;
                }

                match Packet::read_from(&mut client.stream) {
                    Ok(packet) => match packet.packet_type {
                        PacketType::Content => {
                            let data = &packet.payload;
                            let result = unsafe {
                                libc::write(pty_fd, data.as_ptr() as _, data.len())
                            };
                            if result == -1 {
                                dead_clients.push(index);
                            }
                        }
                        PacketType::Resize => {
                            if let Some((cols, rows)) = packet.parse_resize() {
                                let ws = libc::winsize {
                                    ws_col: cols,
                                    ws_row: rows,
                                    ws_xpixel: 0,
                                    ws_ypixel: 0,
                                };
                                unsafe {
                                    libc::ioctl(pty_fd, libc::TIOCSWINSZ, &ws);
                                }
                            }
                        }
                        PacketType::Detach => {
                            dead_clients.push(index);
                        }
                        _ => {}
                    },
                    Err(_) => {
                        dead_clients.push(index);
                    }
                }
            }

            dead_clients.sort_unstable();
            for index in dead_clients.into_iter().rev() {
                clients.remove(index);
            }
        }
    }

    fn cleanup(&self) {
        fs::remove_file(&self.socket_path).ok();
    }

    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub fn child_pid(&self) -> Pid {
        self.child_pid
    }

    pub fn is_alive(socket_path: &Path) -> bool {
        std::os::unix::net::UnixStream::connect(socket_path).is_ok()
    }

    pub fn list_sessions(socket_dir: &Path) -> anyhow::Result<Vec<(String, bool)>> {
        let mut sessions = Vec::new();
        if !socket_dir.exists() {
            return Ok(sessions);
        }

        for entry in fs::read_dir(socket_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("sock") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                let alive = Self::is_alive(&path);
                sessions.push((name, alive));
            }
        }

        Ok(sessions)
    }

    pub fn kill_session(socket_path: &Path) -> anyhow::Result<()> {
        fs::remove_file(socket_path)?;
        Ok(())
    }
}

fn setup_slave_and_exec(slave: &OwnedFd, working_dir: &Path, command: &[String]) -> ! {
    let slave_fd = slave.as_raw_fd();
    unistd::setsid().ok();
    unsafe {
        libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0);
        libc::dup2(slave_fd, 0);
        libc::dup2(slave_fd, 1);
        libc::dup2(slave_fd, 2);
        if slave_fd > 2 {
            libc::close(slave_fd);
        }
    }

    std::env::set_current_dir(working_dir).ok();

    let program = std::ffi::CString::new(command[0].as_str()).expect("invalid command");
    let args: Vec<std::ffi::CString> = command
        .iter()
        .map(|arg| std::ffi::CString::new(arg.as_str()).expect("invalid arg"))
        .collect();
    let arg_ptrs: Vec<*const libc::c_char> = args
        .iter()
        .map(|arg| arg.as_ptr())
        .chain(std::iter::once(std::ptr::null()))
        .collect();

    unsafe {
        libc::execvp(program.as_ptr(), arg_ptrs.as_ptr());
    }
    eprintln!("failed to exec: {}", command[0]);
    process::exit(1);
}

struct ClientConn {
    stream: std::os::unix::net::UnixStream,
}

impl Drop for SessionServer {
    fn drop(&mut self) {
        self.cleanup();
        signal::kill(self.child_pid, Signal::SIGTERM).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn socket_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn test_create_foreground_and_connect() {
        let dir = socket_dir();
        let command = vec!["echo".into(), "hello".into()];

        let (server, socket_path) =
            SessionServer::create_foreground("test-echo", &command, Path::new("/tmp"), dir.path())
                .unwrap();

        assert!(socket_path.to_str().unwrap().contains("test-echo.sock"));
        assert_eq!(server.session_name(), "test-echo");

        let handle = std::thread::spawn(move || {
            server.run().unwrap();
        });

        std::thread::sleep(Duration::from_millis(200));

        let mut client = std::os::unix::net::UnixStream::connect(&socket_path);

        if let Ok(ref mut stream) = client {
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .ok();
            for _ in 0..10 {
                match Packet::read_from(stream) {
                    Ok(packet) if packet.packet_type == PacketType::Exit => break,
                    Ok(_) => continue,
                    Err(_) => break,
                }
            }
        }

        handle.join().unwrap();
    }

    #[test]
    fn test_list_sessions_empty() {
        let dir = socket_dir();
        let sessions = SessionServer::list_sessions(dir.path()).unwrap();
        assert!(sessions.is_empty());
    }

    #[test]
    fn test_list_sessions_with_stale_socket() {
        let dir = socket_dir();
        fs::write(dir.path().join("stale.sock"), "").unwrap();
        let sessions = SessionServer::list_sessions(dir.path()).unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].0, "stale");
        assert!(!sessions[0].1);
    }

    #[test]
    fn test_create_foreground_interactive() {
        let dir = socket_dir();
        let command = vec!["cat".into()];

        let (server, socket_path) =
            SessionServer::create_foreground("test-cat", &command, Path::new("/tmp"), dir.path())
                .unwrap();

        let handle = std::thread::spawn(move || {
            server.run().unwrap();
        });

        std::thread::sleep(Duration::from_millis(200));

        let mut stream = std::os::unix::net::UnixStream::connect(&socket_path).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .ok();

        let input = Packet::content(b"test input\n");
        input.write_to(&mut stream).unwrap();

        std::thread::sleep(Duration::from_millis(100));
        let response = Packet::read_from(&mut stream);
        if let Ok(packet) = response {
            assert_eq!(packet.packet_type, PacketType::Content);
        }

        Packet::detach().write_to(&mut stream).unwrap();
        drop(stream);

        std::thread::sleep(Duration::from_millis(100));
        let reattach = std::os::unix::net::UnixStream::connect(&socket_path);
        assert!(reattach.is_ok(), "session should still be alive after detach");

        fs::remove_file(&socket_path).ok();
        handle.join().ok();
    }
}
