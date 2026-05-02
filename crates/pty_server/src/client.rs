use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::protocol::{Packet, PacketType};

/// Client that attaches to a running session server.
pub struct SessionClient {
    stream: UnixStream,
}

impl SessionClient {
    pub fn connect(socket_path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        Ok(Self { stream })
    }

    pub fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }

    pub fn send_content(&mut self, data: &[u8]) -> io::Result<()> {
        Packet::content(data).write_to(&mut self.stream)
    }

    pub fn send_resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        Packet::resize(cols, rows).write_to(&mut self.stream)
    }

    pub fn send_detach(&mut self) -> io::Result<()> {
        Packet::detach().write_to(&mut self.stream)
    }

    pub fn recv(&mut self) -> io::Result<Packet> {
        Packet::read_from(&mut self.stream)
    }

    /// Run the attach loop: bridge stdin/stdout with the session socket.
    pub fn run_attach(&mut self) -> anyhow::Result<Option<i32>> {
        let stdin_fd = io::stdin().as_raw_fd();
        let socket_fd = self.stream.as_raw_fd();

        // Save and set raw mode
        let mut original_termios: libc::termios = unsafe { std::mem::zeroed() };
        unsafe { libc::tcgetattr(stdin_fd, &mut original_termios) };

        let mut raw = original_termios;
        unsafe { libc::cfmakeraw(&mut raw) };
        unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw) };

        let result = self.attach_loop(stdin_fd, socket_fd);

        // Restore terminal
        unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &original_termios) };

        result
    }

    fn attach_loop(&mut self, stdin_fd: i32, socket_fd: i32) -> anyhow::Result<Option<i32>> {
        let mut buf = [0u8; 8192];

        if let Ok(size) = terminal_size() {
            self.send_resize(size.0, size.1)?;
        }

        loop {
            let mut pollfds = [
                libc::pollfd {
                    fd: stdin_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: socket_fd,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];

            let result = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, -1) };
            if result == -1 {
                continue; // EINTR
            }

            // stdin → session
            if pollfds[0].revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as _, buf.len()) };
                if n <= 0 {
                    return Ok(None);
                }
                self.send_content(&buf[..n as usize])?;
            }

            // session → stdout
            if pollfds[1].revents & libc::POLLIN != 0 {
                match self.recv() {
                    Ok(packet) => match packet.packet_type {
                        PacketType::Content => {
                            io::stdout().write_all(&packet.payload)?;
                            io::stdout().flush()?;
                        }
                        PacketType::Exit => {
                            return Ok(packet.parse_exit_status());
                        }
                        _ => {}
                    },
                    Err(_) => {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

fn terminal_size() -> io::Result<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let result = unsafe { libc::ioctl(0, libc::TIOCGWINSZ, &mut ws) };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok((ws.ws_col, ws.ws_row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::SessionServer;
    use std::time::Duration;

    #[test]
    fn test_client_connect_and_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let command = vec!["cat".into()];

        let (server, socket_path) = SessionServer::create_foreground(
            "test-client",
            &command,
            Path::new("/tmp"),
            dir.path(),
        )
        .unwrap();

        let handle = std::thread::spawn(move || {
            server.run().unwrap();
        });

        std::thread::sleep(Duration::from_millis(200));

        // First client: send data, verify echo, detach
        let mut client = SessionClient::connect(&socket_path).unwrap();
        client.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        client.send_content(b"hello\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let packet = client.recv().unwrap();
        assert_eq!(packet.packet_type, PacketType::Content);

        client.send_detach().unwrap();
        drop(client);

        // Second client: verify session survived detach
        std::thread::sleep(Duration::from_millis(100));
        let mut client2 = SessionClient::connect(&socket_path).unwrap();
        client2.set_read_timeout(Some(Duration::from_secs(2))).unwrap();

        client2.send_content(b"still alive\n").unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let packet = client2.recv().unwrap();
        assert_eq!(packet.packet_type, PacketType::Content);

        // Clean up
        drop(client2);
        std::fs::remove_file(&socket_path).ok();
        handle.join().ok();
    }
}
