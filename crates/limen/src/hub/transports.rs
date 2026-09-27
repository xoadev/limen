//! How the MCP server is reached (spec §9): newline-delimited JSON-RPC on stdio, or HTTP.

use super::dir::{Hub, LiveHub};
use super::mcp::McpServer;
use super::{NodeClient, constant_time_eq};
use crate::os::sys;
use limen_core::config::hub::HubConfig;
use limen_core::join::{self, Arrival};
use limen_core::protocol::{ErrorCode, Result, error};
use serde_json::json;
use std::io::{BufRead, Read};
use std::sync::Arc;
use tiny_http::{Header, Method, Request, Response, Server};

const MAX_BODY: u64 = 1024 * 1024;
const MAX_ARRIVAL: u64 = 16 * 1024;

/// `limen mcp`: newline-delimited JSON-RPC on stdin and stdout. Logs go to stderr: stdout is the protocol.
pub fn stdio(client: Arc<dyn NodeClient>) {
    let server = McpServer::new(
        client,
        Some(Box::new(|m| sys::out(&format!("{m}\n")))),
        Box::new(|m| sys::err(&format!("limen: {m}\n"))),
    );
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(answer) = server.handle(&line) {
            sys::out(&format!("{answer}\n"));
        }
    }
}

/// `limen serve`: MCP Streamable HTTP on `POST /mcp`, JSON answers and no SSE. A bearer token is required and
/// `Origin`, when a browser sends one, must be listed, against DNS rebinding.
///
/// And `/join/<code>` (spec §10.1), where a node with an invitation fetches the hub's key and then reports its own.
/// No token there: the one-time code is the authorisation, and all it allows is adding the node it names.
pub fn http(hub: Arc<Hub>, live: Arc<LiveHub>, listen: &str, token: String) -> Result<()> {
    let config = live.config()?;
    let server = Arc::new(McpServer::new(live.clone(), None, Box::new(|m| sys::err(&format!("limen: {m}\n")))));
    let http =
        Server::http(listen).map_err(|e| error(ErrorCode::Unavailable, format!("cannot listen on {listen}: {e}")))?;
    let nodes = live.nodes().map(|n| n.len()).unwrap_or(0);
    let fingerprint = join::fingerprint(&hub.public_key()?)?;
    sys::err(&format!("limen: serving MCP on http://{listen}/mcp; {nodes} node(s); hub key {fingerprint}\n"));
    sys::err(
        "limen: `limen connect` prints the line for an MCP client, `limen invite <name>` the one for a new node\n",
    );
    let shared = Arc::new(Shared { hub, live, server, config, token });
    for request in http.incoming_requests() {
        let shared = shared.clone();
        // A thread per request: a tool call waits on ssh, and a slow node must not hold the others.
        std::thread::spawn(move || shared.answer(request));
    }
    Ok(())
}

struct Shared {
    hub: Arc<Hub>,
    live: Arc<LiveHub>,
    server: Arc<McpServer>,
    config: HubConfig,
    token: String,
}

fn header<'a>(request: &'a Request, name: &'static str) -> Option<&'a str> {
    request.headers().iter().find(|h| h.field.equiv(name)).map(|h| h.value.as_str())
}

fn json_response(body: String, status: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_status_code(status)
        .with_header(Header::from_bytes("Content-Type", "application/json").expect("a valid header"))
}

fn status(code: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string("").with_status_code(code)
}

fn join_error(message: &str, code: u16) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(json!({"error": message}).to_string(), code)
}

impl Shared {
    fn answer(&self, mut request: Request) {
        let path = request.url().split('?').next().unwrap_or("").to_string();
        let method = request.method().clone();
        let response = match (&method, path.as_str()) {
            (Method::Post, "/mcp") => self.mcp(&mut request),
            // No SSE stream and no sessions to end: every call is a request and its answer.
            (Method::Get | Method::Delete, "/mcp") => status(405),
            (Method::Get, p) if p.starts_with("/join/") => match self.hub.invitation(&p["/join/".len()..]) {
                Some(invitation) => {
                    json_response(serde_json::to_string(&invitation).expect("an invitation serializes"), 200)
                }
                None => join_error("this invitation does not exist, was used, or expired", 404),
            },
            (Method::Post, p) if p.starts_with("/join/") => {
                let code = p["/join/".len()..].to_string();
                self.arrive(&mut request, &code)
            }
            _ => status(404),
        };
        request.respond(response).ok();
    }

    fn mcp(&self, request: &mut Request) -> Response<std::io::Cursor<Vec<u8>>> {
        if let Some(refused) = self.refused(request) {
            return refused;
        }
        let body = match bounded(request, MAX_BODY) {
            Ok(b) => b,
            Err(code) => return status(code),
        };
        match self.server.handle(&body) {
            None => status(202),
            Some(answer) => json_response(answer, 200),
        }
    }

    /// Why this request may not use the MCP endpoint, if it may not: a foreign `Origin` (403) or no bearer token (401).
    fn refused(&self, request: &Request) -> Option<Response<std::io::Cursor<Vec<u8>>>> {
        if let Some(origin) = header(request, "Origin") {
            if !self.config.origins.iter().any(|o| o == origin) {
                return Some(status(403));
            }
        }
        let given = header(request, "Authorization")
            .and_then(|h| h.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("bearer ")).map(|_| h[7..].trim()))
            .unwrap_or("");
        if !constant_time_eq(given, &self.token) {
            return Some(
                status(401).with_header(Header::from_bytes("WWW-Authenticate", "Bearer").expect("a valid header")),
            );
        }
        None
    }

    fn arrive(&self, request: &mut Request, code: &str) -> Response<std::io::Cursor<Vec<u8>>> {
        let body = match bounded(request, MAX_ARRIVAL) {
            Ok(b) => b,
            Err(code) => return status(code),
        };
        let Ok(arrival) = serde_json::from_str::<Arrival>(&body) else {
            return join_error("malformed request", 400);
        };
        let from = request.remote_addr().map(|a| a.ip().to_string()).unwrap_or_default();
        match self.hub.arrive(code, &arrival, &from, self.live.as_ref()) {
            Ok(welcome) => {
                sys::err(&format!(
                    "limen: node {} joined from {}: {}\n",
                    welcome.name, welcome.address, welcome.detail
                ));
                json_response(serde_json::to_string(&welcome).expect("a welcome serializes"), 200)
            }
            Err(e) => join_error(&e.message, if e.code == ErrorCode::NotFound { 404 } else { 400 }),
        }
    }
}

/// A body of at most [max] bytes, said by `Content-Length` and only by it: with `Transfer-Encoding` the length header
/// is not what is read, and a chunked body could grow without end. Otherwise the status to answer.
fn bounded(request: &mut Request, max: u64) -> std::result::Result<String, u16> {
    let length = header(request, "Content-Length").and_then(|l| l.trim().parse::<u64>().ok());
    let length = match length {
        _ if header(request, "Transfer-Encoding").is_some() => return Err(411),
        None => return Err(411),
        Some(l) if l > max => return Err(413),
        Some(l) => l,
    };
    let mut body = String::new();
    request.as_reader().take(length).read_to_string(&mut body).map_err(|_| 400u16)?;
    Ok(body)
}
