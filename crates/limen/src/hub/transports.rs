//! How the MCP server is reached (spec §9): newline-delimited JSON-RPC on stdio, or HTTP.

use super::dir::{Hub, LiveHub};
use super::mcp::McpServer;
use super::server::{self, Handler, Head, Limits, Response};
use super::{NodeClient, constant_time_eq};
use crate::os::sys;
use limen_core::config::hub::HubConfig;
use limen_core::join::{self, Arrival};
use limen_core::protocol::{ErrorCode, Result, error};
use serde_json::json;
use std::io::BufRead;
use std::net::TcpListener;
use std::sync::Arc;

const MAX_BODY: usize = 1024 * 1024;
const MAX_ARRIVAL: usize = 16 * 1024;

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
    let server = McpServer::new(live.clone(), None, Box::new(|m| sys::err(&format!("limen: {m}\n"))));
    let listener = TcpListener::bind(listen)
        .map_err(|e| error(ErrorCode::Unavailable, format!("cannot listen on {listen}: {e}")))?;
    let nodes = live.nodes().map(|n| n.len()).unwrap_or(0);
    let fingerprint = join::fingerprint(&hub.public_key()?)?;
    sys::err(&format!("limen: serving MCP on http://{listen}/mcp; {nodes} node(s); hub key {fingerprint}\n"));
    sys::err(
        "limen: `limen connect` prints the line for an MCP client, `limen invite <name>` the one for a new node\n",
    );
    let routes = Arc::new(Routes { hub, live, server, config, token });
    let stopped = server::serve(listener, routes, Limits::default());
    // A server that stops is a hub that is down: exit with an error, so a restart policy brings it back.
    Err(error(ErrorCode::Unavailable, format!("the HTTP server stopped: {stopped}")))
}

struct Routes {
    hub: Arc<Hub>,
    live: Arc<LiveHub>,
    server: McpServer,
    config: HubConfig,
    token: String,
}

fn join_error(message: &str, status: u16) -> Response {
    Response::json(status, json!({"error": message}).to_string())
}

impl Handler for Routes {
    fn admit(&self, head: &Head) -> std::result::Result<usize, Response> {
        match (head.method.as_str(), head.path.as_str()) {
            // Refused before a byte of the body is read.
            ("POST", "/mcp") => self.refused(head).map_or(Ok(MAX_BODY), Err),
            // No SSE stream and no sessions to end: every call is a request and its answer.
            ("GET" | "DELETE", "/mcp") => Err(Response::status(405)),
            ("GET", p) if p.starts_with("/join/") => Ok(0),
            ("POST", p) if p.starts_with("/join/") => Ok(MAX_ARRIVAL),
            _ => Err(Response::status(404)),
        }
    }

    fn answer(&self, head: &Head, body: &str) -> Response {
        let code = head.path.strip_prefix("/join/").unwrap_or_default();
        match head.method.as_str() {
            "POST" if head.path == "/mcp" => match self.server.handle(body) {
                None => Response::status(202),
                Some(answer) => Response::json(200, answer),
            },
            "GET" => match self.hub.invitation(code) {
                Some(invitation) => {
                    Response::json(200, serde_json::to_string(&invitation).expect("an invitation serializes"))
                }
                None => join_error("this invitation does not exist, was used, or expired", 404),
            },
            _ => self.arrive(code, body, head.peer.to_string()),
        }
    }
}

impl Routes {
    /// Why this request may not use the MCP endpoint, if it may not: a foreign `Origin` (403) or no bearer token (401).
    fn refused(&self, head: &Head) -> Option<Response> {
        if head.header("Origin").is_some_and(|origin| !self.config.origins.iter().any(|o| o == origin)) {
            return Some(Response::status(403));
        }
        let given = head
            .header("Authorization")
            .and_then(|h| h.get(..7).filter(|scheme| scheme.eq_ignore_ascii_case("bearer ")).map(|_| h[7..].trim()))
            .unwrap_or("");
        if !constant_time_eq(given, &self.token) {
            return Some(Response::status(401).with_header("WWW-Authenticate", "Bearer"));
        }
        None
    }

    fn arrive(&self, code: &str, body: &str, from: String) -> Response {
        let Ok(arrival) = serde_json::from_str::<Arrival>(body) else {
            return join_error("malformed request", 400);
        };
        match self.hub.arrive(code, &arrival, &from, self.live.as_ref()) {
            Ok(welcome) => {
                sys::err(&format!(
                    "limen: node {} joined from {}: {}\n",
                    welcome.name,
                    welcome.address,
                    welcome.detail.escape_debug()
                ));
                Response::json(200, serde_json::to_string(&welcome).expect("a welcome serializes"))
            }
            Err(e) => join_error(&e.message, if e.code == ErrorCode::NotFound { 404 } else { 400 }),
        }
    }
}
