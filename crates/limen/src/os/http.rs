//! The two requests of `limen join` (spec §10.1): HTTP/1.1 to the hub's own server over a plain socket, to an
//! address. One request per connection (`Connection: close`) and a JSON body each way: all the hub speaks, so a
//! client library would be ten crates for two requests.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(60);
/// The hub's answers are an invitation or a welcome: a few hundred bytes.
const MAX_ANSWER: u64 = 1 << 20;

#[derive(Debug, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub body: String,
}

/// [method] [path] on [authority] (`100.64.0.2:7341`, `[fd00::1]:7341`), with [body] as JSON when given.
pub fn request(authority: &str, method: &str, path: &str, body: Option<&str>) -> Result<Response, String> {
    let address = authority
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .next()
        .ok_or_else(|| format!("no address in {authority}"))?;
    let mut stream = TcpStream::connect_timeout(&address, TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(TIMEOUT)).ok();
    stream.set_write_timeout(Some(TIMEOUT)).ok();
    let body = body.unwrap_or("");
    let message = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(message.as_bytes()).map_err(|e| e.to_string())?;
    let mut raw = Vec::new();
    stream.take(MAX_ANSWER).read_to_end(&mut raw).map_err(|e| e.to_string())?;
    parse(&raw)
}

/// An HTTP/1.1 answer: its status and its body, by `Content-Length`, chunked, or up to the connection's end.
pub fn parse(raw: &[u8]) -> Result<Response, String> {
    let split = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or("the hub's answer has no headers")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("the hub's answer is not HTTP")?;
    let header = |name: &str| {
        lines
            .clone()
            .find_map(|l| l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
    };
    let rest = &raw[split + 4..];
    let body = if header("Transfer-Encoding").is_some_and(|t| t.eq_ignore_ascii_case("chunked")) {
        dechunk(rest)?
    } else if let Some(length) = header("Content-Length").and_then(|l| l.parse::<usize>().ok()) {
        rest.get(..length).ok_or("the hub's answer is shorter than it says")?.to_vec()
    } else {
        rest.to_vec()
    };
    Ok(Response { status, body: String::from_utf8_lossy(&body).into_owned() })
}

fn dechunk(mut rest: &[u8]) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    loop {
        let end = rest.windows(2).position(|w| w == b"\r\n").ok_or("a chunk without its size")?;
        let size = std::str::from_utf8(&rest[..end])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok());
        let size = size.ok_or("a chunk size that is not a number")?;
        if size == 0 {
            return Ok(body);
        }
        let chunk = rest.get(end + 2..end + 2 + size).ok_or("a chunk shorter than it says")?;
        body.extend_from_slice(chunk);
        rest = rest.get(end + 4 + size..).unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn answers_by_length_chunked_or_to_the_end() {
        let plain = parse(b"HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\n\r\n{}trailing").unwrap();
        assert_eq!(plain, Response { status: 404, body: "{}".into() });
        let chunked = parse(
            "HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n5\r\n{\"a\":\r\n5\r\n\"ñ\"}\r\n0\r\n\r\n".as_bytes(),
        )
        .unwrap();
        assert_eq!(chunked.body, "{\"a\":\"ñ\"}");
        assert_eq!(parse(b"HTTP/1.0 200 OK\r\n\r\nall of it").unwrap().body, "all of it");
        assert!(parse(b"garbage").is_err());
        assert!(parse(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nshort").is_err());
    }

    #[test]
    fn a_request_and_its_answer_over_a_socket() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            // The whole request first, as a server does: closing with unread bytes would reset the connection.
            let mut seen = Vec::new();
            let mut buffer = [0u8; 4096];
            while !seen.ends_with(b"{\"x\":1}") {
                let n = socket.read(&mut buffer).unwrap();
                seen.extend_from_slice(&buffer[..n]);
            }
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}").unwrap();
            String::from_utf8_lossy(&seen).into_owned()
        });
        let answer = request(&authority, "POST", "/join/abc", Some("{\"x\":1}")).unwrap();
        assert_eq!(answer, Response { status: 200, body: "{\"ok\":true}".into() });
        let seen = server.join().unwrap();
        assert!(seen.starts_with("POST /join/abc HTTP/1.1\r\n"), "{seen}");
        assert!(seen.contains("Content-Length: 7\r\n") && seen.ends_with("{\"x\":1}"), "{seen}");
    }
}
