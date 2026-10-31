//! How the MCP server is reached (spec §9): newline-delimited JSON-RPC on stdio, or HTTP.

use super::dir::{Hub, LiveHub};
use super::mcp::{self, McpServer};
use super::mqtt::{Bridge, Broker, Declared, Entities};
use super::server::{self, Handler, Head, Limits, Response};
use super::{NodeClient, constant_time_eq};
use crate::os::sys;
use limen_core::config::hub::HubConfig;
use limen_core::join::{self, Arrival};
use limen_core::protocol::{ErrorCode, Result, error};
use serde_json::json;
use std::io::BufRead;
use std::net::TcpListener;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_MCP_BODY: usize = 1024 * 1024;
/// Event streams open at once (`GET /mcp`), each holding one of the server's connections for as long as it lasts.
const MAX_STREAMS: usize = 8;
/// How often the nodes are asked again while a client listens: a catalog that changed is announced within this.
const RECHECK_EVERY: Duration = Duration::from_secs(60);
const MAX_ARRIVAL_BODY: usize = 16 * 1024;
const JOIN_PREFIX: &str = "/join/";

/// `limen mcp`: newline-delimited JSON-RPC on stdin and stdout. Logs go to stderr: stdout is the protocol.
///
/// Each `tools/call` runs in a thread of its own: one that waits for a person's approval holds up no other, and the
/// client's answer to that question arrives on this same stdin. When stdin ends, the calls under way finish first.
pub fn stdio(client: Arc<dyn NodeClient>) {
    let server = Arc::new(McpServer::new(client, Some(Box::new(sys::say)), Box::new(sys::log)));
    // The client listens for as long as the process lives.
    recheck(server.clone(), || true);
    let mut calls = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        if mcp::is_tool_call(&line) {
            let server = server.clone();
            calls.push(std::thread::spawn(move || answer(&server, &line)));
            calls.retain(|call| !call.is_finished());
        } else {
            answer(&server, &line);
        }
    }
    // Nobody is left to answer a question: the calls waiting for one end now, not running.
    server.no_more_answers();
    for call in calls {
        call.join().ok();
    }
}

fn answer(server: &McpServer, line: &str) {
    if let Some(answer) = server.handle(line) {
        sys::say(&answer);
    }
}

/// `limen serve`: MCP Streamable HTTP. `POST /mcp` answers in JSON; `GET /mcp` opens a stream of server-sent events,
/// where `notifications/tools/list_changed` reaches every client listening. No sessions. A bearer token is required
/// and `Origin`, when a browser sends one, must be listed, against DNS rebinding.
///
/// And `/join/<code>` (spec §10.1), where a node with an invitation fetches the hub's key and then reports its own.
/// No token there: the one-time code is the authorisation, and all it allows is adding the node it names.
pub fn http(hub: Arc<Hub>, live: Arc<LiveHub>, listen: &str, token: String) -> Result<()> {
    let config = live.config()?;
    let listeners = Arc::new(Listeners::default());
    let announce = listeners.clone();
    let mut server =
        McpServer::new(live.clone(), Some(Box::new(move |message| announce.send(message))), Box::new(sys::log))
            .announcing_only();
    if let Some(mqtt) = config.mqtt.as_ref().filter(|mqtt| mqtt.enabled) {
        let bridge =
            Bridge::start(live.clone(), entities(live.clone()), mqtt, Broker::of(mqtt, &hub.home)?, Box::new(sys::log));
        server = server.with_problems(Box::new(move || bridge.refusals()));
    }
    let listener = TcpListener::bind(listen)
        .map_err(|cause| error(ErrorCode::Unavailable, format!("cannot listen on {listen}: {cause}")))?;
    let node_count = live.nodes().map_or(0, |nodes| nodes.len());
    let fingerprint = join::fingerprint(&hub.public_key()?)?;
    sys::log(&format!("serving MCP on http://{listen}/mcp; {node_count} node(s); hub key {fingerprint}"));
    sys::log("`limen connect` prints the line for an MCP client, `limen invite <name>` the one for a new node");
    let server = Arc::new(server);
    let listening = listeners.clone();
    recheck(server.clone(), move || listening.any());
    let routes: Arc<dyn Handler> = Arc::new(Routes { hub, live, server, listeners, config, token });
    let stopped = server::serve(&listener, &routes, Limits::default());
    // A server that stops is a hub that is down: exit with an error, so a restart policy brings it back.
    Err(error(ErrorCode::Unavailable, format!("the HTTP server stopped: {stopped}")))
}

/// The sensors and buttons as `limen.toml` says now. `[mqtt]` taken out or disabled is read once, as its other keys
/// are: until `serve` restarts, the sensors and buttons stay as they were.
fn entities(live: Arc<LiveHub>) -> Entities {
    Box::new(move || {
        let config = live.config()?;
        let mqtt = config.mqtt.as_ref().filter(|mqtt| mqtt.enabled).ok_or_else(|| {
            error(ErrorCode::Unavailable, "[mqtt] is gone or disabled; it takes effect when serve restarts")
        })?;
        Ok(Declared {
            entities: mqtt.entities().map(|(entity, button)| (entity.clone(), button)).collect(),
            nodes: config.nodes.iter().map(|node| node.name.clone()).collect(),
            approval: config.approval.clone(),
        })
    })
}

/// Asks the nodes again every [RECHECK_EVERY] while [listening] says a client would hear of a change.
fn recheck(server: Arc<McpServer>, listening: impl Fn() -> bool + Send + 'static) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(RECHECK_EVERY);
            if listening() {
                server.recheck();
            }
        }
    });
}

/// The clients with an event stream open, each as the sender of its stream.
#[derive(Default)]
struct Listeners(Mutex<Vec<Sender<String>>>);

impl Listeners {
    /// A new stream, unless [MAX_STREAMS] are open; one whose connection ended frees its place now.
    fn open(&self) -> Option<Receiver<String>> {
        let mut senders = self.senders();
        senders.retain(|sender| sender.send(String::new()).is_ok());
        if senders.len() >= MAX_STREAMS {
            return None;
        }
        let (sender, events) = mpsc::channel();
        senders.push(sender);
        Some(events)
    }

    /// [message] to every stream; one whose client left is dropped.
    fn send(&self, message: &str) {
        self.senders().retain(|sender| sender.send(message.to_string()).is_ok());
    }

    /// Whether a stream is still open: an empty message, which a stream doesn't send, finds the ones whose client left.
    fn any(&self) -> bool {
        let mut senders = self.senders();
        senders.retain(|sender| sender.send(String::new()).is_ok());
        !senders.is_empty()
    }

    fn senders(&self) -> std::sync::MutexGuard<'_, Vec<Sender<String>>> {
        self.0.lock().expect("nothing panics holding the event streams")
    }
}

struct Routes {
    hub: Arc<Hub>,
    live: Arc<LiveHub>,
    server: Arc<McpServer>,
    listeners: Arc<Listeners>,
    config: HubConfig,
    token: String,
}

impl Handler for Routes {
    fn admit(&self, head: &Head) -> std::result::Result<usize, Response> {
        match (head.method.as_str(), head.path.as_str()) {
            // Refused before a byte of the body is read.
            ("POST", "/mcp") => self.refused(head).map_or(Ok(MAX_MCP_BODY), Err),
            ("GET", "/mcp") => self.refused(head).map_or(Ok(0), Err),
            // No sessions to end.
            ("DELETE", "/mcp") => Err(Response::status(405)),
            ("GET", path) if path.starts_with(JOIN_PREFIX) => Ok(0),
            ("POST", path) if path.starts_with(JOIN_PREFIX) => Ok(MAX_ARRIVAL_BODY),
            _ => Err(Response::status(404)),
        }
    }

    /// Only what [Routes::admit] let through gets here: `POST` and `GET` of `/mcp`, and `GET` or `POST` of a join.
    fn answer(&self, head: &Head, body: &str) -> Response {
        let code = head.path.strip_prefix(JOIN_PREFIX).unwrap_or_default();
        match head.method.as_str() {
            "POST" if head.path == "/mcp" => self.mcp(body),
            "GET" if head.path == "/mcp" => {
                self.listeners.open().map_or_else(|| Response::status(503), Response::events)
            }
            "GET" => self.invitation(code),
            _ => self.arrive(code, body, &head.peer.to_string()),
        }
    }
}

impl Routes {
    /// Why this request may not use the MCP endpoint, if it may not: a foreign `Origin` (403) or no bearer token (401).
    fn refused(&self, head: &Head) -> Option<Response> {
        if head.header("Origin").is_some_and(|origin| !self.config.origins.iter().any(|allowed| allowed == origin)) {
            return Some(Response::status(403));
        }
        if !constant_time_eq(bearer_token(head).unwrap_or(""), &self.token) {
            return Some(Response::status(401).with_header("WWW-Authenticate", "Bearer"));
        }
        None
    }

    fn mcp(&self, body: &str) -> Response {
        match self.server.handle(body) {
            None => Response::status(202),
            Some(answer) => Response::json(200, answer),
        }
    }

    fn invitation(&self, code: &str) -> Response {
        match self.hub.invitation(code) {
            Some(invitation) => {
                Response::json(200, serde_json::to_string(&invitation).expect("an invitation serializes"))
            }
            None => join_error(404, "this invitation does not exist, was used, or expired"),
        }
    }

    fn arrive(&self, code: &str, body: &str, from: &str) -> Response {
        let Ok(arrival) = serde_json::from_str::<Arrival>(body) else {
            return join_error(400, "malformed request");
        };
        match self.hub.arrive(code, &arrival, from, self.live.as_ref()) {
            Ok(welcome) => {
                sys::log(&format!(
                    "node {} joined from {}: {}",
                    welcome.name,
                    welcome.address,
                    welcome.detail.escape_debug()
                ));
                Response::json(200, serde_json::to_string(&welcome).expect("a welcome serializes"))
            }
            Err(refusal) => join_error(if refusal.code == ErrorCode::NotFound { 404 } else { 400 }, &refusal.message),
        }
    }
}

/// The token of `Authorization: Bearer <token>`, the scheme in any case.
fn bearer_token(head: &Head) -> Option<&str> {
    const SCHEME: &str = "bearer ";
    let (scheme, token) = head.header("Authorization")?.split_at_checked(SCHEME.len())?;
    scheme.eq_ignore_ascii_case(SCHEME).then(|| token.trim())
}

fn join_error(status: u16, message: &str) -> Response {
    Response::json(status, json!({"error": message}).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_are_capped_and_a_closed_one_frees_its_place() {
        let listeners = Listeners::default();
        let mut open: Vec<_> = (0..MAX_STREAMS).map(|_| listeners.open().unwrap()).collect();
        assert!(listeners.open().is_none());
        listeners.send("announced");
        assert_eq!(open[0].try_iter().find(|message| !message.is_empty()).unwrap(), "announced");
        open.pop();
        let last = listeners.open();
        assert!(last.is_some());
        open.clear();
        assert!(listeners.any(), "the stream opened last is still open");
        drop(last);
        assert!(!listeners.any());
    }
}
