//! Enough HTTP to hand a browser the page and then get out of the way.
//!
//! Four static files and one upgrade. Everything that matters afterwards happens
//! on the websocket, so this file's job is to be small, correct, and finished --
//! it is not a framework and there is nothing to grow here.
//!
//! WHY THE ASSETS ARE BAKED IN
//! ---------------------------
//! The page, the terminal emulator and its stylesheet are `include_str!`d into
//! the binary. `sauron` is a single file you copy onto your PATH; a version that
//! needed a sibling `assets/` directory would work from the checkout and fail
//! everywhere else, and a page served from a *different* build than the wire it
//! is talking to is the bug class this avoids entirely.
//!
//! grep targets:
//!   fn spawn     -- bind, then a thread per connection forever
//!   fn handle    -- one connection: parse, route, respond or upgrade
//!   fn request   -- request line + the two headers this cares about
//!   fn respond   -- a complete small response, then close
//!   const MAX_HEAD -- the cap, and what it is protecting

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

use super::{connection, State, FIT_JS, PAGE, XTERM_CSS, XTERM_JS};

/// How much request head we will read before giving up. A browser's GET is well
/// under a kilobyte; anything larger is a client that has lost the plot or is
/// trying to make us allocate on its behalf.
const MAX_HEAD: usize = 16 * 1024;

pub fn spawn(listener: TcpListener, state: Arc<State>, label: String) {
    let page: Arc<str> = Arc::from(PAGE.replace("{{LABEL}}", &escape_html(&label)));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let (state, page) = (state.clone(), page.clone());
            std::thread::spawn(move || {
                let _ = handle(stream, state, &page);
            });
        }
    });
}

fn handle(mut stream: TcpStream, state: Arc<State>, page: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let Some(req) = request(&mut reader)? else {
        return respond(&mut stream, 400, "text/plain", b"bad request");
    };

    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            respond(&mut stream, 200, "text/html; charset=utf-8", page.as_bytes())
        }
        ("GET", "/vendor/xterm.js") => {
            respond(&mut stream, 200, "text/javascript; charset=utf-8", XTERM_JS.as_bytes())
        }
        ("GET", "/vendor/addon-fit.js") => {
            respond(&mut stream, 200, "text/javascript; charset=utf-8", FIT_JS.as_bytes())
        }
        ("GET", "/vendor/xterm.css") => {
            respond(&mut stream, 200, "text/css; charset=utf-8", XTERM_CSS.as_bytes())
        }
        ("GET", "/ws") => match req.ws_key {
            // The connection stops being HTTP here and this call owns it until
            // the browser goes away.
            Some(key) => connection(stream, &key, state),
            None => respond(&mut stream, 400, "text/plain", b"expected a websocket upgrade"),
        },
        ("GET", "/favicon.ico") => respond(&mut stream, 204, "image/x-icon", b""),
        _ => respond(&mut stream, 404, "text/plain", b"not found"),
    }
}

struct Request {
    method: String,
    path: String,
    ws_key: Option<String>,
}

fn request(reader: &mut BufReader<TcpStream>) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    let mut read = reader.take(MAX_HEAD as u64).read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
        return Ok(None);
    };
    let (method, path) = (method.to_string(), strip_query(path).to_string());

    let mut ws_key = None;
    loop {
        let mut header = String::new();
        let n = reader
            .take((MAX_HEAD.saturating_sub(read)) as u64)
            .read_line(&mut header)?;
        if n == 0 {
            return Ok(None);
        }
        read += n;
        if header == "\r\n" || header == "\n" {
            break;
        }
        if let Some((k, val)) = header.split_once(':') {
            if k.eq_ignore_ascii_case("sec-websocket-key") {
                ws_key = Some(val.trim().to_string());
            }
        }
        if read >= MAX_HEAD {
            return Ok(None);
        }
    }
    Ok(Some(Request { method, path, ws_key }))
}

fn strip_query(path: &str) -> &str {
    path.split(['?', '#']).next().unwrap_or("/")
}

fn respond(stream: &mut TcpStream, code: u16, mime: &str, body: &[u8]) -> std::io::Result<()> {
    let reason = match code {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\n\
         Content-Type: {mime}\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// The repo label goes into the page's `<title>`, and a repo may be called
/// anything a directory may be called. Escaped rather than trusted: a checkout
/// named `<script>` is a silly name, not a licence to run it.
fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}
