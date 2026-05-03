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
        let mut stdin_filter = StdinFilter::new();

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

            // stdin → session, with terminal-response sequences filtered out so
            // they never reach the daemon's PTY (where they'd be echoed and
            // pollute scrollback).
            if pollfds[0].revents & libc::POLLIN != 0 {
                let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr() as _, buf.len()) };
                if n <= 0 {
                    return Ok(None);
                }
                let filtered = stdin_filter.filter(&buf[..n as usize]);
                if !filtered.is_empty() {
                    self.send_content(&filtered)?;
                }
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

/// A streaming filter that strips terminal-response escape sequences from
/// stdin before they reach the daemon. Without this, the host terminal's
/// answers to capability queries (DA1, DECRPM, cursor-position reports, etc.)
/// flow into the PTY where they get echoed back as garbled text. The filter
/// holds partial sequences across `filter` calls so a sequence split across
/// reads is still recognized.
struct StdinFilter {
    pending: Vec<u8>,
}

#[derive(Copy, Clone)]
enum CsiKind {
    /// `\x1b[?...` — private mode CSI; almost always a terminal response.
    Private,
    /// `\x1b[<n>;<n>R` — cursor-position report (DSR).
    CursorPosition,
}

impl StdinFilter {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    /// Process an input chunk, returning the bytes that should be forwarded
    /// to the daemon. Terminal-response sequences are dropped; user input
    /// (regular keys, arrows, function keys, mouse events, paste) passes
    /// through unchanged.
    fn filter(&mut self, input: &[u8]) -> Vec<u8> {
        let mut buf = Vec::with_capacity(self.pending.len() + input.len());
        buf.append(&mut self.pending);
        buf.extend_from_slice(input);

        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while i < buf.len() {
            if buf[i] != 0x1B {
                out.push(buf[i]);
                i += 1;
                continue;
            }

            // Possible escape sequence. Try to recognize it; if incomplete,
            // stash for the next call.
            match parse_csi(&buf[i..]) {
                CsiResult::NeedMore => {
                    self.pending.extend_from_slice(&buf[i..]);
                    return out;
                }
                CsiResult::Drop(consumed) => {
                    i += consumed;
                }
                CsiResult::Pass(consumed) => {
                    out.extend_from_slice(&buf[i..i + consumed]);
                    i += consumed;
                }
            }
        }
        out
    }
}

enum CsiResult {
    /// Sequence parsed; drop these bytes (don't forward).
    Drop(usize),
    /// Sequence parsed; forward these bytes.
    Pass(usize),
    /// Buffer ended mid-sequence; need more bytes to decide.
    NeedMore,
}

/// Try to recognize an escape sequence starting at `bytes[0] == 0x1B`. Returns
/// how many bytes were consumed and whether to drop or forward them.
fn parse_csi(bytes: &[u8]) -> CsiResult {
    debug_assert!(bytes.first() == Some(&0x1B));
    if bytes.len() < 2 {
        return CsiResult::NeedMore;
    }
    // Only CSI sequences (ESC [) are candidates for response-stripping.
    // Everything else (ESC O for SS3, ESC ] for OSC, plain ESC for keymap)
    // is user input — pass through.
    if bytes[1] != b'[' {
        return CsiResult::Pass(2);
    }
    if bytes.len() < 3 {
        return CsiResult::NeedMore;
    }

    // Detect CSI variant.
    let (kind, params_start) = match bytes[2] {
        b'?' => (CsiKind::Private, 3),
        // Numeric-leading CSI like `\x1b[<n>;<n>R` (cursor-position report).
        // We only flag it as response-shaped if the final byte is 'R'.
        c if c.is_ascii_digit() => (CsiKind::CursorPosition, 2),
        _ => return pass_csi(bytes),
    };

    let mut j = params_start;
    while j < bytes.len() {
        let c = bytes[j];
        if c.is_ascii_digit() || c == b';' || c == b':' || c == b' ' {
            j += 1;
            continue;
        }
        // Intermediate byte ($, ", etc.) is part of the sequence; keep going.
        if (0x20..=0x2F).contains(&c) {
            j += 1;
            continue;
        }
        // Final byte (0x40..=0x7E ends a CSI).
        if (0x40..=0x7E).contains(&c) {
            let consumed = j + 1;
            let drop = match kind {
                CsiKind::Private => true,
                CsiKind::CursorPosition => c == b'R',
            };
            return if drop {
                CsiResult::Drop(consumed)
            } else {
                CsiResult::Pass(consumed)
            };
        }
        // Unexpected byte mid-sequence; bail out and pass the lone ESC[.
        return CsiResult::Pass(2);
    }
    CsiResult::NeedMore
}

/// Pass through a CSI sequence we don't classify (arrows, mouse, paste, etc.).
fn pass_csi(bytes: &[u8]) -> CsiResult {
    let mut j = 2;
    while j < bytes.len() {
        let c = bytes[j];
        if c.is_ascii_digit() || c == b';' || c == b':' || c == b' ' {
            j += 1;
            continue;
        }
        if (0x20..=0x2F).contains(&c) {
            j += 1;
            continue;
        }
        if (0x40..=0x7E).contains(&c) {
            return CsiResult::Pass(j + 1);
        }
        return CsiResult::Pass(2);
    }
    CsiResult::NeedMore
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
