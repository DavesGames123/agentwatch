//! WebSocket framing, by hand, over a blocking `TcpStream`.
//!
//! WHY A SOCKET AND NOT THE SERVER-SENT EVENTS THIS USED TO USE
//! ------------------------------------------------------------
//! SSE is one-way and text-only, and both halves of that became wrong the moment
//! the browser started driving real terminals. A keystroke has to reach the pty
//! in the same round trip a human notices, and pty output is *bytes* -- a UTF-8
//! sequence routinely splits across two `read` calls, and re-encoding it as text
//! on every hop means either base64 (a third more traffic on the burstiest
//! channel) or a decoder that corrupts a partial codepoint. Binary frames carry
//! the bytes as they came off the fd.
//!
//! WHY IT IS WRITTEN HERE
//! ----------------------
//! `Cargo.toml` calls sauron a standalone sidecar and means it. A websocket
//! crate would bring an async runtime, an http crate and a random-number crate
//! for a protocol whose server half is: read a length, unmask four bytes at a
//! time, write a length. The client half -- the part with the masking, the
//! continuation state machine and the close negotiation -- is the browser's
//! problem, not ours.
//!
//! WHAT IS DELIBERATELY NOT IMPLEMENTED
//! ------------------------------------
//! No `permessage-deflate` (the header is never negotiated, so it is never
//! expected), no fragmentation on the way *out* (frames are written whole), and
//! no client role. Reading fragmented frames *is* handled, because browsers send
//! them for large pastes.
//!
//! grep targets:
//!   fn accept       -- the 101 response that upgrades a connection
//!   struct Ws       -- one upgraded connection, split for a reader and a writer
//!   fn Ws::read     -- next application message, control frames handled inside
//!   fn Ws::send     -- one whole frame out, text or binary
//!   enum Msg        -- what a read can produce
//!   const MAX_FRAME -- the cap, and what it is protecting

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use super::sha1;

/// The largest message we will assemble. A paste into an agent's terminal is
/// the biggest legitimate thing a browser sends and is measured in kilobytes; a
/// frame claiming gigabytes is a client trying to make the server allocate on
/// its behalf, and the connection is dropped rather than obliged.
const MAX_FRAME: usize = 4 * 1024 * 1024;

const OP_CONT: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BIN: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// An application message off the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// JSON control traffic: open a pane, ack a row, resize a terminal.
    Text(String),
    /// Keystrokes for a pane. First byte is the pane index; the rest is what
    /// the pty should receive verbatim.
    Binary(Vec<u8>),
    /// The peer said goodbye, or the socket did.
    Close,
}

/// The `101 Switching Protocols` response for a client key.
///
/// Written by the caller, which is the only place that has the request headers;
/// this just spells the answer, because getting the accept key wrong fails with
/// a browser console message that says nothing about which side was at fault.
pub fn accept(client_key: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\
         \r\n",
        sha1::accept_key(client_key)
    )
}

/// One upgraded connection.
///
/// The writer is behind its own mutex and cloned to every thread that pushes at
/// this browser -- the board ticker, and one reader thread per live pty. That is
/// the whole reason this type exists rather than a bare stream: a frame must
/// reach the wire whole, and three threads interleaving their headers would
/// produce a stream no client can parse.
pub struct Ws {
    stream: TcpStream,
    out: Arc<Mutex<TcpStream>>,
}

/// The write half, handed to anything that produces output for this browser.
#[derive(Clone)]
pub struct WsOut(Arc<Mutex<TcpStream>>);

impl Ws {
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        let out = Arc::new(Mutex::new(stream.try_clone()?));
        Ok(Self { stream, out })
    }

    pub fn out(&self) -> WsOut {
        WsOut(self.out.clone())
    }

    /// The next application message. Ping/pong and close are handled here, so a
    /// caller only ever sees traffic it has to act on.
    pub fn read(&mut self) -> io::Result<Msg> {
        let mut assembled: Vec<u8> = Vec::new();
        let mut kind = 0u8;

        loop {
            let (fin, opcode, payload) = self.frame()?;
            match opcode {
                OP_CLOSE => return Ok(Msg::Close),
                OP_PING => {
                    // Answer with the same payload, as the RFC requires. A
                    // browser that gets no pong eventually drops the socket, and
                    // the tab goes blank for no visible reason.
                    WsOut(self.out.clone()).frame(OP_PONG, &payload)?;
                    continue;
                }
                OP_PONG => continue,
                OP_TEXT | OP_BIN => {
                    kind = opcode;
                    assembled = payload;
                }
                OP_CONT => assembled.extend_from_slice(&payload),
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("unknown websocket opcode {other:#x}"),
                    ))
                }
            }
            if assembled.len() > MAX_FRAME {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "message too large"));
            }
            if fin {
                return Ok(match kind {
                    OP_TEXT => Msg::Text(String::from_utf8_lossy(&assembled).into_owned()),
                    _ => Msg::Binary(assembled),
                });
            }
        }
    }

    /// One frame: fin, opcode, unmasked payload.
    fn frame(&mut self) -> io::Result<(bool, u8, Vec<u8>)> {
        let mut head = [0u8; 2];
        self.stream.read_exact(&mut head)?;
        let fin = head[0] & 0x80 != 0;
        let opcode = head[0] & 0x0F;
        let masked = head[1] & 0x80 != 0;

        let len = match head[1] & 0x7F {
            126 => {
                let mut b = [0u8; 2];
                self.stream.read_exact(&mut b)?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                self.stream.read_exact(&mut b)?;
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        if len > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }

        // RFC 6455: every client frame is masked. An unmasked one is either a
        // broken client or something that is not a browser, and the spec says to
        // fail the connection rather than guess.
        let mut key = [0u8; 4];
        if masked {
            self.stream.read_exact(&mut key)?;
        } else if len > 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "client frame was not masked"));
        }

        let mut payload = vec![0u8; len];
        self.stream.read_exact(&mut payload)?;
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= key[i % 4];
            }
        }
        Ok((fin, opcode, payload))
    }
}

impl WsOut {
    pub fn text(&self, s: &str) -> io::Result<()> {
        self.frame(OP_TEXT, s.as_bytes())
    }

    /// Pty output for a pane: the index, then the bytes exactly as they were
    /// read off the fd.
    pub fn binary(&self, pane: u8, data: &[u8]) -> io::Result<()> {
        let mut buf = Vec::with_capacity(data.len() + 1);
        buf.push(pane);
        buf.extend_from_slice(data);
        self.frame(OP_BIN, &buf)
    }

    pub fn close(&self) {
        let _ = self.frame(OP_CLOSE, &[]);
    }

    fn frame(&self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        let mut head = Vec::with_capacity(10);
        head.push(0x80 | opcode); // FIN, never fragmented on the way out
        match payload.len() {
            n if n < 126 => head.push(n as u8),
            n if n <= u16::MAX as usize => {
                head.push(126);
                head.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                head.push(127);
                head.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        // One lock for header and payload together. Splitting them is how three
        // threads produce a stream that parses as garbage.
        let mut out = self
            .0
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "websocket writer poisoned"))?;
        out.write_all(&head)?;
        out.write_all(payload)?;
        out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Frame a payload the way a browser would: masked, with a known key.
    fn client_frame(opcode: u8, payload: &[u8], fin: bool) -> Vec<u8> {
        let key = [0xAA, 0xBB, 0xCC, 0xDD];
        let mut out = vec![if fin { 0x80 | opcode } else { opcode }];
        match payload.len() {
            n if n < 126 => out.push(0x80 | n as u8),
            n => {
                out.push(0x80 | 126);
                out.extend_from_slice(&(n as u16).to_be_bytes());
            }
        }
        out.extend_from_slice(&key);
        out.extend(payload.iter().enumerate().map(|(i, b)| b ^ key[i % 4]));
        out
    }

    /// Drive `Ws::frame`'s parsing over a pair of real sockets -- the type is
    /// built on `TcpStream`, and a loopback pair is cheaper than making it
    /// generic for the sake of the test.
    fn round_trip(frames: Vec<u8>) -> Vec<Msg> {
        use std::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let writer = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(&frames).unwrap();
            c.flush().unwrap();
            // Hold the socket open until the reader has finished with it.
            std::thread::sleep(std::time::Duration::from_millis(120));
        });
        let (server, _) = listener.accept().unwrap();
        let mut ws = Ws::new(server).unwrap();
        let mut got = Vec::new();
        while let Ok(m) = ws.read() {
            let done = m == Msg::Close;
            got.push(m);
            if done {
                break;
            }
        }
        let _ = writer.join();
        got
    }

    #[test]
    fn a_masked_text_frame_arrives_unmasked() {
        let got = round_trip(client_frame(OP_TEXT, b"{\"t\":\"ack\"}", true));
        assert_eq!(got.first(), Some(&Msg::Text("{\"t\":\"ack\"}".into())));
    }

    #[test]
    fn keystrokes_arrive_as_bytes_with_their_pane_in_front() {
        // pane 3, then a carriage return -- what pressing Enter in a tab sends.
        let got = round_trip(client_frame(OP_BIN, &[3, b'\r'], true));
        assert_eq!(got.first(), Some(&Msg::Binary(vec![3, b'\r'])));
    }

    #[test]
    fn a_fragmented_paste_is_reassembled() {
        // Browsers split large pastes. Losing the tail would silently truncate
        // whatever the user pasted into an agent.
        let mut wire = client_frame(OP_BIN, &[1, b'a', b'b'], false);
        wire.extend(client_frame(OP_CONT, b"cd", false));
        wire.extend(client_frame(OP_CONT, b"ef", true));
        let got = round_trip(wire);
        assert_eq!(got.first(), Some(&Msg::Binary(vec![1, b'a', b'b', b'c', b'd', b'e', b'f'])));
    }

    #[test]
    fn a_close_frame_ends_the_conversation() {
        assert_eq!(round_trip(client_frame(OP_CLOSE, &[], true)), vec![Msg::Close]);
    }

    #[test]
    fn an_unmasked_client_frame_is_refused() {
        // Not pedantry: an unmasked frame means the peer is not a browser
        // speaking RFC 6455, and guessing at its framing is how a parser starts
        // reading payload as headers.
        let mut wire = vec![0x80 | OP_TEXT, 5];
        wire.extend_from_slice(b"hello");
        assert!(round_trip(wire).is_empty(), "the connection should fail, not parse");
    }
}
