//! The hub's HTTP server (spec §9): the strict subset of HTTP/1.1 that MCP's `POST` and the join's `GET` and `POST`
//! need, with every limit set before anything is read —connections at once, header size, body size, time— and one
//! request per connection. Nothing reaches a handler that a stranger on the network could use to exhaust the hub.

use rustix::io::Errno;
use std::io::{self, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
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
        self.headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
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

/// Serves [listener] until it fails for good; what it returns is why.
pub fn serve(listener: &TcpListener, handler: &Arc<dyn Handler>, limits: Limits) -> io::Error {
    let limits = Arc::new(limits);
    let open = Arc::new(AtomicUsize::new(0));
    loop {
        let (mut stream, peer) = match accept(listener) {
            Ok(accepted) => accepted,
            Err(error) => return error,
        };
        stream.set_write_timeout(Some(limits.timeout)).ok();
        let Some(slot) = ConnectionSlot::take(&open, limits.connections) else {
            send(&mut stream, &Response::status(503));
            continue;
        };
        let (handler, limits) = (handler.clone(), limits.clone());
        // A thread that can't start drops its closure: the connection closes and the slot is given back.
        std::thread::Builder::new()
            .spawn(move || {
                connection(stream, peer.ip(), handler.as_ref(), &limits);
                drop(slot);
            })
            .ok();
    }
}

/// One of [Limits::connections], given back when dropped, however its connection ended.
struct ConnectionSlot(Arc<AtomicUsize>);

impl ConnectionSlot {
    fn take(open: &Arc<AtomicUsize>, max: usize) -> Option<Self> {
        let already_open = open.fetch_add(1, Ordering::SeqCst);
        let slot = ConnectionSlot(open.clone());
        (already_open < max).then_some(slot)
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The next connection. Running out of descriptors, or a connection aborted before it was accepted, is waited out,
/// not a reason to stop.
fn accept(listener: &TcpListener) -> io::Result<(TcpStream, SocketAddr)> {
    loop {
        match listener.accept() {
            Err(error) if is_transient(&error) => std::thread::sleep(Duration::from_millis(100)),
            accepted => return accepted,
        }
    }
}

fn is_transient(error: &io::Error) -> bool {
    const EXHAUSTED: [Errno; 4] = [Errno::MFILE, Errno::NFILE, Errno::NOBUFS, Errno::NOMEM];
    matches!(error.kind(), ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted)
        || error.raw_os_error().is_some_and(|code| EXHAUSTED.iter().any(|errno| errno.raw_os_error() == code))
}

/// Why a request got no answer from its handler.
enum Stop {
    /// Refused with this answer.
    Refused(Response),
    /// The peer left or ran out of time: there is nobody to answer.
    Gone,
}

impl From<Response> for Stop {
    fn from(refusal: Response) -> Self {
        Stop::Refused(refusal)
    }
}

fn connection(mut stream: TcpStream, peer: IpAddr, handler: &dyn Handler, limits: &Limits) {
    match respond(&mut stream, peer, handler, limits) {
        Ok(response) | Err(Stop::Refused(response)) => send(&mut stream, &response),
        Err(Stop::Gone) => {}
    }
}

/// Reads one request and asks [handler] for its answer, each limit checked before what it bounds is read.
fn respond(stream: &mut TcpStream, peer: IpAddr, handler: &dyn Handler, limits: &Limits) -> Result<Response, Stop> {
    let deadline = Instant::now() + limits.timeout;
    let (raw_head, body_start) = read_head(stream, limits.head_bytes, deadline)?;
    let head = parse_head(&raw_head, peer).ok_or_else(|| Response::status(400))?;
    let max_body = handler.admit(&head)?;
    let length = body_length(&head)?;
    if length > max_body {
        return Err(Stop::Refused(Response::status(413)));
    }
    let body = read_body(stream, body_start, length, deadline)?;
    Ok(handler.answer(&head, &body))
}

/// The head's bytes, up to the blank line that ends it, and what came after them: the start of the body.
fn read_head(stream: &mut TcpStream, max_bytes: usize, deadline: Instant) -> Result<(Vec<u8>, Vec<u8>), Stop> {
    const BLANK_LINE: &[u8] = b"\r\n\r\n";
    let mut buffer = Vec::with_capacity(4096);
    loop {
        let end = buffer.windows(BLANK_LINE.len()).position(|window| window == BLANK_LINE);
        if end.unwrap_or(buffer.len()) > max_bytes {
            return Err(Stop::Refused(Response::status(431)));
        }
        if let Some(end) = end {
            let body_start = buffer.split_off(end + BLANK_LINE.len());
            buffer.truncate(end);
            return Ok((buffer, body_start));
        }
        if !read_some(stream, &mut buffer, deadline) {
            return Err(Stop::Gone);
        }
    }
}

/// `Content-Length` and only it: with `Transfer-Encoding` the length is not what is read, and a chunked body could
/// grow without end.
fn body_length(head: &Head) -> Result<usize, Response> {
    match (head.header("Transfer-Encoding"), head.header("Content-Length")) {
        (Some(_), _) => Err(Response::status(411)),
        (None, Some(length)) => length.trim().parse().map_err(|_| Response::status(400)),
        (None, None) if head.method == "POST" => Err(Response::status(411)),
        (None, None) => Ok(0),
    }
}

/// The body of [length] bytes, [received] being what arrived with the head.
fn read_body(stream: &mut TcpStream, mut received: Vec<u8>, length: usize, deadline: Instant) -> Result<String, Stop> {
    while received.len() < length {
        if !read_some(stream, &mut received, deadline) {
            return Err(Stop::Gone);
        }
    }
    received.truncate(length);
    String::from_utf8(received).map_err(|_| Stop::Refused(Response::status(400)))
}

/// Reads what is there into [buffer] before [deadline]; false when the peer is gone or out of time.
fn read_some(stream: &mut TcpStream, buffer: &mut Vec<u8>, deadline: Instant) -> bool {
    let Some(left) = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()) else {
        return false;
    };
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
    let mut request_line = lines.next()?.split(' ');
    let (method, target, version) = (request_line.next()?, request_line.next()?, request_line.next()?);
    if request_line.next().is_some() || !version.starts_with("HTTP/1.") || !target.starts_with('/') {
        return None;
    }
    let headers = lines
        .map(|line| line.split_once(':').map(|(name, value)| (name.trim().to_string(), value.trim().to_string())))
        .collect::<Option<Vec<_>>>()?;
    let path = target.split('?').next().unwrap_or(target).to_string();
    Some(Head { method: method.into(), path, peer, headers })
}

fn send(stream: &mut TcpStream, response: &Response) {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
        response.status,
        reason_phrase(response.status),
        response.body.len()
    );
    for (name, value) in &response.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(response.body.as_bytes())).ok();
    stream.shutdown(std::net::Shutdown::Write).ok();
    drain(stream);
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
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
    }
}

/// Reads and drops what the client is still sending, a little and briefly: closing with it unread resets the
/// connection, and the client would never see a 413 or a 431.
fn drain(stream: &mut TcpStream) {
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

    /// Starts a server on a free port and returns its address.
    fn server(limits: Limits) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let handler: Arc<dyn Handler> = Arc::new(Echo);
        std::thread::spawn(move || serve(&listener, &handler, limits));
        address
    }

    fn exchange(address: &str, raw: &[u8]) -> String {
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(raw).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).ok();
        answer
    }

    #[test]
    fn a_request_and_its_answer() {
        let address = server(Limits::default());
        let answer = exchange(&address, b"POST /open?x=1 HTTP/1.1\r\nX-Test: yes\r\nContent-Length: 5\r\n\r\nhello");
        assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
        assert!(answer.ends_with("POST yes hello"), "{answer}");
        assert!(exchange(&address, b"GET /closed HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 401"));
    }

    #[test]
    fn limits_hold_before_anything_is_read() {
        let address = server(Limits::default());
        // A length no memory could hold: refused from the header, never allocated.
        let huge = exchange(&address, b"POST /open HTTP/1.1\r\nContent-Length: 18446744073709551615\r\n\r\n");
        assert!(huge.starts_with("HTTP/1.1 413"), "{huge}");
        assert!(
            exchange(&address, b"POST /closed HTTP/1.1\r\nContent-Length: 999999999999\r\n\r\n")
                .starts_with("HTTP/1.1 401")
        );
        assert!(
            exchange(&address, b"POST /open HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
                .starts_with("HTTP/1.1 411")
        );
        assert!(exchange(&address, b"POST /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 411"));
        let long_header = format!("GET /open HTTP/1.1\r\nX: {}\r\n\r\n", "a".repeat(20_000));
        assert!(exchange(&address, long_header.as_bytes()).starts_with("HTTP/1.1 431"));
        assert!(exchange(&address, b"garbage\r\n\r\n").starts_with("HTTP/1.1 400"));
        // And the server is still there.
        assert!(exchange(&address, b"GET /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn idle_connections_time_out_and_can_not_starve_the_rest() {
        let address = server(Limits { connections: 4, head_bytes: 1024, timeout: Duration::from_millis(500) });
        let idle: Vec<TcpStream> = (0..4).map(|_| TcpStream::connect(&address).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(100));
        // Full: one more is told so at once.
        assert!(exchange(&address, b"GET /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 503"));
        // The idle ones are dropped at the deadline, and the hub serves again.
        std::thread::sleep(Duration::from_millis(700));
        assert!(exchange(&address, b"GET /open HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 200"));
        drop(idle);
    }
}
