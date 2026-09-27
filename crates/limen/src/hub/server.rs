//! The hub's HTTP server (spec §9): the strict subset of HTTP/1.1 that MCP's `POST` and the join's `GET` and `POST`
//! need, with every limit set before anything is read —connections at once, header size, body size, time— and one
//! request per connection. Nothing reaches a handler that a stranger on the network could use to exhaust the hub.

use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub struct Limits {
    /// Connections served at once; one more is answered 503 and closed.
    pub connections: usize,
    /// Request line and headers.
    pub head_bytes: usize,
    /// For the whole request to arrive: a client trickling a byte at a time can't hold a connection past it.
    pub timeout: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { connections: 64, head_bytes: 16 * 1024, timeout: Duration::from_secs(15) }
    }
}

/// A request's line and headers, all that is known before its body is read.
pub struct Head {
    pub method: String,
    pub path: String,
    pub peer: IpAddr,
    headers: Vec<(String, String)>,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

pub struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

impl Response {
    pub fn status(status: u16) -> Self {
        Response { status, headers: vec![], body: String::new() }
    }

    pub fn json(status: u16, body: String) -> Self {
        Response { status, headers: vec![("Content-Type", "application/json".into())], body }
    }

    pub fn with_header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

pub trait Handler: Send + Sync + 'static {
    /// Before the body is read: how many bytes of body this request may bring, or the answer that refuses it.
    fn admit(&self, head: &Head) -> Result<usize, Response>;

    fn answer(&self, head: &Head, body: &str) -> Response;
}

/// Serves [listener] until it fails for good; what it returns is why. Running out of descriptors, or a connection
/// aborted before it was accepted, is waited out, not a reason to stop.
pub fn serve(listener: TcpListener, handler: Arc<dyn Handler>, limits: Limits) -> io::Error {
    let limits = Arc::new(limits);
    let open = Arc::new(AtomicUsize::new(0));
    loop {
        let (mut stream, peer) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(e) if transient(&e) => {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return e,
        };
        stream.set_write_timeout(Some(limits.timeout)).ok();
        if open.fetch_add(1, Ordering::SeqCst) >= limits.connections {
            open.fetch_sub(1, Ordering::SeqCst);
            write(&mut stream, &Response::status(503));
            continue;
        }
        let (handler, limits, count) = (handler.clone(), limits.clone(), open.clone());
        let spawned = std::thread::Builder::new().spawn(move || {
            connection(stream, peer.ip(), handler.as_ref(), &limits);
            count.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            open.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

fn transient(e: &io::Error) -> bool {
    matches!(e.kind(), ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted)
        || matches!(e.raw_os_error(), Some(code) if [rustix::io::Errno::MFILE, rustix::io::Errno::NFILE, rustix::io::Errno::NOBUFS, rustix::io::Errno::NOMEM].iter().any(|e| e.raw_os_error() == code))
}

fn connection(mut stream: TcpStream, peer: IpAddr, handler: &dyn Handler, limits: &Limits) {
    let deadline = Instant::now() + limits.timeout;
    let mut buffer = Vec::with_capacity(4096);
    let end = loop {
        let found = buffer.windows(4).position(|w| w == b"\r\n\r\n");
        if found.unwrap_or(buffer.len()) > limits.head_bytes {
            return write(&mut stream, &Response::status(431));
        }
        if let Some(end) = found {
            break end;
        }
        if !read_some(&mut stream, &mut buffer, deadline) {
            return;
        }
    };
    let Some(head) = parse_head(&buffer[..end], peer) else {
        return write(&mut stream, &Response::status(400));
    };
    let max_body = match handler.admit(&head) {
        Ok(max) => max,
        Err(refused) => return write(&mut stream, &refused),
    };
    // Content-Length and only it: with Transfer-Encoding the length is not what is read, and a chunked body could
    // grow without end.
    let length = match (head.header("Transfer-Encoding"), head.header("Content-Length")) {
        (Some(_), _) => return write(&mut stream, &Response::status(411)),
        (None, Some(l)) => match l.trim().parse::<usize>() {
            Ok(l) => l,
            Err(_) => return write(&mut stream, &Response::status(400)),
        },
        (None, None) if head.method == "POST" => return write(&mut stream, &Response::status(411)),
        (None, None) => 0,
    };
    if length > max_body {
        return write(&mut stream, &Response::status(413));
    }
    let mut body = buffer.split_off(end + 4);
    while body.len() < length {
        if !read_some(&mut stream, &mut body, deadline) {
            return;
        }
    }
    body.truncate(length);
    let Ok(body) = String::from_utf8(body) else {
        return write(&mut stream, &Response::status(400));
    };
    let response = handler.answer(&head, &body);
    write(&mut stream, &response);
}

/// Reads what is there into [buffer] before [deadline]; false when the peer is gone or out of time.
fn read_some(stream: &mut TcpStream, buffer: &mut Vec<u8>, deadline: Instant) -> bool {
    let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero()) else { return false };
    stream.set_read_timeout(Some(left)).ok();
    let mut chunk = [0u8; 4096];
    match stream.read(&mut chunk) {
        Ok(0) | Err(_) => false,
        Ok(n) => {
            buffer.extend_from_slice(&chunk[..n]);
            true
        }
    }
}

fn parse_head(raw: &[u8], peer: IpAddr) -> Option<Head> {
    let text = std::str::from_utf8(raw).ok()?;
    let mut lines = text.split("\r\n");
    let mut line = lines.next()?.split(' ');
    let (method, target, version) = (line.next()?, line.next()?, line.next()?);
    if line.next().is_some() || !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return None;
    }
    let headers = lines
        .map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_string(), v.trim().to_string())))
        .collect::<Option<Vec<_>>>()?;
    let path = target.split('?').next().unwrap_or(target).to_string();
    Some(Head { method: method.into(), path, peer, headers })
}

fn write(stream: &mut TcpStream, response: &Response) {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "",
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).and_then(|_| stream.write_all(response.body.as_bytes())).ok();
    stream.shutdown(std::net::Shutdown::Write).ok();
    // What the client is still sending is read and dropped, a little and briefly: closing with it unread resets the
    // connection, and the client would never see a 413 or a 431.
    stream.set_read_timeout(Some(Duration::from_millis(500))).ok();
    let mut sink = [0u8; 4096];
    let mut drained = 0;
    while drained < 64 * 1024 {
        match stream.read(&mut sink) {
            Ok(n) if n > 0 => drained += n,
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Handler for Echo {
        fn admit(&self, head: &Head) -> Result<usize, Response> {
            match head.path.as_str() {
                "/open" => Ok(64),
                _ => Err(Response::status(401)),
            }
        }

        fn answer(&self, head: &Head, body: &str) -> Response {
            Response::json(200, format!("{} {} {body}", head.method, head.header("x-test").unwrap_or("")))
        }
    }

    fn server(limits: Limits) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || serve(listener, Arc::new(Echo), limits));
        address
    }

    fn exchange(address: &str, raw: &[u8]) -> String {
        let mut s = TcpStream::connect(address).unwrap();
        s.write_all(raw).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).ok();
        out
    }

    #[test]
    fn a_request_and_its_answer() {
        let a = server(Limits::default());
        let out = exchange(&a, b"POST /open?x=1 HTTP/1.1\r\nX-Test: yes\r\nContent-Length: 5\r\n\r\nhello");
        assert!(out.starts_with("HTTP/1.1 200 OK\r\n"), "{out}");
        assert!(out.ends_with("POST yes hello"), "{out}");
        assert!(exchange(&a, b"GET /closed HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
    }

    #[test]
    fn limits_hold_before_anything_is_read() {
        let a = server(Limits::default());
        // A length no memory could hold: refused from the header, never allocated.
        let huge = exchange(&a, b"POST /open HTTP/1.1\r\nContent-Length: 18446744073709551615\r\n\r\n");
        assert!(huge.starts_with("HTTP/1.1 413"), "{huge}");
        assert!(
            exchange(&a, b"POST /closed HTTP/1.1\r\nContent-Length: 999999999999\r\n\r\n").starts_with("HTTP/1.1 401")
        );
        assert!(exchange(&a, b"POST /open HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n").starts_with("HTTP/1.1 411"));
        assert!(exchange(&a, b"POST /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 411"));
        let long_header = format!("GET /open HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(20_000));
        assert!(exchange(&a, long_header.as_bytes()).starts_with("HTTP/1.1 431"));
        assert!(exchange(&a, b"garbage\r\n\r\n").starts_with("HTTP/1.1 400"));
        // And the server is still there.
        assert!(exchange(&a, b"GET /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn idle_connections_time_out_and_can_not_starve_the_rest() {
        let a = server(Limits { connections: 4, head_bytes: 1024, timeout: Duration::from_millis(500) });
        let idle: Vec<TcpStream> = (0..4).map(|_| TcpStream::connect(&a).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(100));
        // Full: one more is told so at once.
        assert!(exchange(&a, b"GET /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 503"));
        // The idle ones are dropped at the deadline, and the hub serves again.
        std::thread::sleep(Duration::from_millis(700));
        assert!(exchange(&a, b"GET /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
        drop(idle);
    }
}
