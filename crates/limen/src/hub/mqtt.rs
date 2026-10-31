//! `limen serve` and Home Assistant (spec §9.1): sensors and buttons through an MQTT broker. Nodes don't know: a
//! sensor's read and a button's press are runs of their scripts over SSH, as an MCP client's are.
//!
//! One thread keeps the connection to the broker, and another schedules each node's rounds: its catalog every minute,
//! then the sensors that are due, one at a time, so a node's sensors take one of its SSH slots at most.

use super::NodeClient;
use super::mcp::{self, Sink};
use crate::os::{fs, sys};
use limen_core::config::hub::Approval;
use limen_core::config::mqtt::{Entity, Mqtt};
use limen_core::mqtt::{self, Connect, Incoming, Topics};
use limen_core::protocol::{ErrorCode, NodeResponse, Result, error};
use limen_core::scripts::{Catalog, ScriptSpec};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

const KEEP_ALIVE_SECONDS: u16 = 60;
const PING_EVERY: Duration = Duration::from_secs(30);
/// Pings are answered: a broker silent this long is gone.
const SILENCE: Duration = Duration::from_secs(90);
const TICK: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const CATALOG_EVERY: Duration = Duration::from_secs(60);

/// The sensors and buttons as `limen.toml` says now, each with whether it is a button.
pub type Entities = Box<dyn Fn() -> Result<Vec<(Entity, bool)>> + Send + Sync>;

/// How the hub reaches its broker: read once, when `serve` starts.
pub struct Broker {
    address: String,
    client_id: String,
    /// The hub's status: the will the broker publishes when the connection drops.
    will_topic: String,
    user: Option<String>,
    password: Option<String>,
}

impl Broker {
    /// `[mqtt]`, with the password from `LIMEN_MQTT_PASSWORD` or `password_file` under the hub's directory [home].
    pub fn of(mqtt: &Mqtt, home: &str) -> Result<Broker> {
        let password = match (sys::env_setting("LIMEN_MQTT_PASSWORD"), &mqtt.password_file) {
            (Some(password), _) => Some(password),
            (None, Some(file)) => {
                let path = if file.starts_with('/') { file.clone() } else { format!("{home}/{file}") };
                let text = fs::read_following(&path).ok_or_else(|| {
                    error(
                        ErrorCode::Unavailable,
                        format!(
                            "cannot read the broker's password at {path} (mqtt.password_file in {home}/limen.toml)"
                        ),
                    )
                })?;
                Some(text.trim().to_string())
            }
            (None, None) => None,
        };
        Ok(Broker {
            address: mqtt.address.clone(),
            client_id: format!("limen-{}", mqtt.prefix),
            will_topic: Topics::of(mqtt).hub_status(),
            user: mqtt.user.clone(),
            password,
        })
    }
}

pub struct Bridge {
    client: Arc<dyn NodeClient>,
    entities: Entities,
    topics: Topics,
    log: Sink,
    state: Mutex<State>,
    /// The connection to the broker, while there is one.
    writer: Mutex<Option<TcpStream>>,
}

#[derive(Default)]
struct State {
    /// The sensors and buttons last read, and the nodes in `[nodes]`.
    entities: Vec<(Entity, bool)>,
    nodes: Vec<String>,
    approval: Approval,
    /// Each node's catalog, and when it was asked for.
    catalogs: HashMap<String, (Instant, Option<Catalog>)>,
    online: HashMap<String, bool>,
    /// The last state of each sensor and result of each button, by topic: what every connection sends again.
    retained: BTreeMap<String, String>,
    failing: HashSet<String>,
    next_read: HashMap<String, Instant>,
    /// Nodes in the middle of a round, and buttons, by their press topic, in the middle of a run.
    busy: HashSet<String>,
    /// Why each refused sensor or button is, by its label.
    refused: BTreeMap<String, String>,
    /// What the broker was last told: each topic declared, with its discovery payload if it is one.
    declared: Option<BTreeMap<String, String>>,
}

/// `sensor nas/disk_used`, as the log and `nodes` name it.
fn label(entity: &Entity, button: bool) -> String {
    format!("{} {}/{}", if button { "button" } else { "sensor" }, entity.node, entity.id)
}

impl State {
    fn kept(&self) -> impl Iterator<Item = &(Entity, bool)> {
        self.entities.iter().filter(|(entity, button)| !self.refused.contains_key(&label(entity, *button)))
    }

    /// The script [entity] runs, once its node's catalog says it may.
    fn spec(&self, entity: &Entity, button: bool) -> Option<ScriptSpec> {
        if self.refused.contains_key(&label(entity, button)) {
            return None;
        }
        let (_, catalog) = self.catalogs.get(&entity.node)?;
        catalog.as_ref()?.scripts.iter().find(|spec| spec.name == entity.script).cloned()
    }

    /// Every topic the hub publishes to now, with the discovery payloads. An entity whose node hasn't answered yet keeps
    /// its topics —what the broker retains of it stays— and gets its payload once the node's catalog accepts it.
    fn declaration(&self, topics: &Topics) -> BTreeMap<String, String> {
        let mut declared = BTreeMap::from([(topics.hub_status(), String::new())]);
        for (entity, button) in self.kept() {
            declared.insert(topics.node_status(&entity.node), String::new());
            declared.insert(if *button { topics.result(entity) } else { topics.state(entity) }, String::new());
            let accepted = self.spec(entity, *button).is_some();
            for (topic, payload) in topics.discovery_payloads(entity, *button) {
                declared.insert(topic, if accepted { payload.to_string() } else { String::new() });
            }
        }
        declared
    }
}

impl Bridge {
    /// Starts the bridge's threads: it connects, and reconnects, to [broker] for as long as `serve` runs.
    pub fn start(
        client: Arc<dyn NodeClient>,
        entities: Entities,
        mqtt: &Mqtt,
        broker: Broker,
        log: Sink,
    ) -> Arc<Bridge> {
        let bridge = Arc::new(Bridge {
            client,
            entities,
            topics: Topics::of(mqtt),
            log,
            state: Mutex::new(State::default()),
            writer: Mutex::new(None),
        });
        bridge.refresh();
        let scheduler = bridge.clone();
        std::thread::spawn(move || scheduler.schedule());
        let connector = bridge.clone();
        std::thread::spawn(move || connector.connect_forever(&broker));
        bridge
    }

    /// The sensors and buttons refused, and why: for `nodes`.
    pub fn refusals(&self) -> Vec<String> {
        self.state().refused.iter().map(|(label, why)| format!("{label}: {why}")).collect()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("nothing panics holding the bridge's state")
    }

    fn writer(&self) -> MutexGuard<'_, Option<TcpStream>> {
        self.writer.lock().expect("nothing panics holding the broker's connection")
    }

    /// Reads `limen.toml` again and decides what is refused; a file that doesn't read leaves things as they were.
    fn refresh(&self) {
        let (Ok(entities), Ok(nodes), Ok(approval)) = ((self.entities)(), self.client.nodes(), self.client.approval())
        else {
            return;
        };
        let mut state = self.state();
        state.entities = entities;
        state.nodes = nodes;
        state.approval = approval;
        self.judge(&mut state);
    }

    /// Decides what is refused on what [state] knows, in the same hold of its lock as what changed it: no one sees a
    /// catalog without what it refuses.
    fn judge(&self, state: &mut State) {
        let mut refused = BTreeMap::new();
        for (entity, button) in &state.entities {
            let catalog = state.catalogs.get(&entity.node).and_then(|(_, catalog)| catalog.as_ref());
            if let Some(why) = mqtt::refusal(entity, *button, &state.nodes, catalog, &state.approval) {
                let label = label(entity, *button);
                if state.refused.get(&label) != Some(&why) {
                    (self.log)(&format!("mqtt: refused {label}: {why}"));
                }
                refused.insert(label, why);
            }
        }
        state.refused = refused;
    }

    /// Starts a round on every node with a sensor or a button and none under way, every [TICK].
    fn schedule(self: Arc<Self>) {
        loop {
            self.refresh();
            let idle: BTreeSet<String> = {
                let mut state = self.state();
                let nodes: BTreeSet<String> = state.kept().map(|(entity, _)| entity.node.clone()).collect();
                nodes.into_iter().filter(|node| state.busy.insert(node.clone())).collect()
            };
            for node in idle {
                let bridge = self.clone();
                std::thread::spawn(move || {
                    bridge.round(&node);
                    bridge.state().busy.remove(&node);
                });
            }
            std::thread::sleep(TICK);
        }
    }

    /// One node's turn: its catalog when it is due, then each sensor that is, one at a time.
    fn round(&self, node: &str) {
        let catalog_due = self.state().catalogs.get(node).is_none_or(|(asked, _)| asked.elapsed() >= CATALOG_EVERY);
        if catalog_due {
            let hello = self.client.call(node, "hello", &Map::new(), None);
            self.set_online(node, mqtt::answered(&hello));
            let mut state = self.state();
            // A node that doesn't answer keeps the catalog it had.
            let catalog = mcp::catalog_of(&hello).or_else(|| state.catalogs.get(node)?.1.clone());
            state.catalogs.insert(node.to_string(), (Instant::now(), catalog));
            self.judge(&mut state);
        }
        for (sensor, spec) in self.due(node) {
            self.read(&sensor, &spec);
        }
    }

    /// The sensors of [node] whose turn has come, each given its next.
    fn due(&self, node: &str) -> Vec<(Entity, ScriptSpec)> {
        let now = Instant::now();
        let mut state = self.state();
        let ready: Vec<(Entity, ScriptSpec)> = state
            .kept()
            .filter(|(entity, button)| !button && entity.node == node)
            .filter_map(|(sensor, _)| Some((sensor.clone(), state.spec(sensor, false)?)))
            .filter(|(sensor, _)| state.next_read.get(&self.topics.state(sensor)).is_none_or(|next| *next <= now))
            .collect();
        for (sensor, _) in &ready {
            state.next_read.insert(self.topics.state(sensor), now + sensor.every);
        }
        ready
    }

    /// Runs [sensor]'s script and publishes its state if it changed.
    fn read(&self, sensor: &Entity, spec: &ScriptSpec) {
        let topic = self.topics.state(sensor);
        let response = self.run(sensor, spec);
        self.set_online(&sensor.node, mqtt::answered(&response));
        let mut state = self.state();
        match mqtt::sensor_state(&response) {
            Ok(value) => {
                state.failing.remove(&topic);
                if state.retained.get(&topic) != Some(&value) {
                    state.retained.insert(topic.clone(), value.clone());
                    drop(state);
                    self.publish(&topic, value.as_bytes());
                }
            }
            Err(why) => {
                if state.failing.insert(topic) {
                    (self.log)(&format!("mqtt: {} failing: {why}", label(sensor, false)));
                }
            }
        }
    }

    /// [entity]'s script on its node, with its fixed arguments, as a tool call would run it.
    fn run(&self, entity: &Entity, spec: &ScriptSpec) -> NodeResponse {
        let mut args = Map::new();
        args.insert("script".into(), json!(entity.script));
        args.insert("args".into(), Value::Object(entity.args.clone()));
        let timeout = Duration::from_secs(spec.timeout_seconds).saturating_add(mcp::SCRIPT_MARGIN);
        self.client.call(&entity.node, "run", &args, Some(timeout))
    }

    /// Whether [node] answers: said to the broker and the log when it changes.
    fn set_online(&self, node: &str, online: bool) {
        if self.state().online.insert(node.to_string(), online) == Some(online) {
            return;
        }
        let word = if online { mqtt::ONLINE } else { mqtt::OFFLINE };
        (self.log)(&format!("mqtt: node {node} {word}"));
        self.publish(&self.topics.node_status(node), word.as_bytes());
    }

    /// A press of [node]'s button [id]: its script runs, once at a time, and its result is published.
    fn press(self: &Arc<Self>, node: &str, id: &str) {
        let mut state = self.state();
        let ready = state
            .entities
            .iter()
            .find(|(entity, button)| *button && entity.node == node && entity.id == id)
            .and_then(|(button, _)| Some((button.clone(), state.spec(button, true)?)));
        let Some((button, spec)) = ready else {
            (self.log)(&format!("mqtt: a press of {node}/{id}, which is no button ready to run, ignored"));
            return;
        };
        let pressing = self.topics.press(&button);
        // A press published retained reaches the hub as any other, and would press again on the next connection.
        self.publish(&pressing, b"");
        if !state.busy.insert(pressing.clone()) {
            (self.log)(&format!("mqtt: a press of {} while it runs, ignored", label(&button, true)));
            return;
        }
        drop(state);
        let bridge = self.clone();
        std::thread::spawn(move || {
            bridge.run_button(&button, &spec);
            bridge.state().busy.remove(&pressing);
        });
    }

    fn run_button(&self, button: &Entity, spec: &ScriptSpec) {
        let started = Instant::now();
        let response = self.run(button, spec);
        self.set_online(&button.node, mqtt::answered(&response));
        let result = response.error.as_ref().map_or("ok", |failure| failure.code.as_str());
        (self.log)(&format!(
            "tool={} node={} button={} result={result} {}ms",
            button.script,
            button.node,
            button.id,
            started.elapsed().as_millis()
        ));
        let topic = self.topics.result(button);
        let payload = mqtt::result_payload(&response);
        self.state().retained.insert(topic.clone(), payload.clone());
        self.publish(&topic, payload.as_bytes());
    }

    /// A message from the broker: a press, or a retained message this hub no longer declares, which it empties.
    fn received(self: &Arc<Self>, topic: &str, payload: &[u8], retained: bool) {
        if !retained {
            // Empty: what clears a topic, the hub's own clearing of a press included.
            if let Some((node, id)) = self.topics.pressed(topic).filter(|_| !payload.is_empty()) {
                self.press(node, id);
            }
            return;
        }
        let stale = self.state().declared.as_ref().is_some_and(|declared| !declared.contains_key(topic));
        if stale && !payload.is_empty() {
            self.publish(topic, b"");
        }
    }

    /// Tells the broker everything again —statuses, discovery, states and results— and subscribes, so it hands over
    /// what is retained under the hub's topics, and what is stale there gets emptied.
    fn sync(&self, packet_id: u16) {
        let mut state = self.state();
        let declared = state.declaration(&self.topics);
        state.retained.retain(|topic, _| declared.contains_key(topic));
        let mut messages = vec![(self.topics.hub_status(), mqtt::ONLINE.to_string())];
        for (node, online) in &state.online {
            let topic = self.topics.node_status(node);
            if declared.contains_key(&topic) {
                messages.push((topic, if *online { mqtt::ONLINE } else { mqtt::OFFLINE }.to_string()));
            }
        }
        messages.extend(
            declared
                .iter()
                .filter(|(_, payload)| !payload.is_empty())
                .map(|(topic, payload)| (topic.clone(), payload.clone())),
        );
        messages.extend(state.retained.clone());
        state.declared = Some(declared);
        drop(state);
        for (topic, payload) in messages {
            self.publish(&topic, payload.as_bytes());
        }
        self.write(&mqtt::subscribe(packet_id, &self.topics.filters()));
    }

    /// Whether what the hub declares changed since the broker was last told.
    fn changed(&self) -> bool {
        let state = self.state();
        state.declared.as_ref() != Some(&state.declaration(&self.topics))
    }

    /// Publishes [payload] retained, if there is a connection: what isn't sent now is sent on the next.
    fn publish(&self, topic: &str, payload: &[u8]) {
        self.write(&mqtt::publish(topic, payload, true));
    }

    fn write(&self, packet: &[u8]) {
        if let Some(stream) = self.writer().as_mut()
            && stream.write_all(packet).is_err()
        {
            // The session sees the reader end, and connects again.
            stream.shutdown(Shutdown::Both).ok();
        }
    }

    fn connect_forever(self: Arc<Self>, broker: &Broker) {
        let mut backoff = TICK;
        loop {
            let started = Instant::now();
            let ended = self.session(broker);
            if started.elapsed() > MAX_BACKOFF {
                backoff = TICK;
            }
            (self.log)(&format!("mqtt: {ended}; connecting to {} again in {}s", broker.address, backoff.as_secs()));
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// One connection to the broker, until it ends; why it ended.
    fn session(self: &Arc<Self>, broker: &Broker) -> String {
        let stream = match connected(broker) {
            Ok(stream) => stream,
            Err(why) => return why,
        };
        let (Ok(writer), Ok(mut reader)) = (stream.try_clone(), stream.try_clone()) else {
            return "cannot share the connection between threads".into();
        };
        (self.log)(&format!("mqtt: connected to {}", broker.address));
        *self.writer() = Some(writer);
        let heard = Arc::new(Mutex::new(Instant::now()));
        let listener = {
            let (bridge, heard) = (self.clone(), heard.clone());
            std::thread::spawn(move || {
                loop {
                    match mqtt::read_packet(&mut reader) {
                        Ok(packet) => {
                            *heard.lock().expect("nothing panics holding a time") = Instant::now();
                            if let Incoming::Publish { topic, payload, retained } = packet {
                                bridge.received(&topic, &payload, retained);
                            }
                        }
                        Err(cause) => return format!("lost the broker: {cause}"),
                    }
                }
            })
        };
        let mut packet_id = 1;
        self.sync(packet_id);
        let mut pinged = Instant::now();
        let ended = loop {
            std::thread::sleep(TICK);
            if listener.is_finished() {
                break None;
            }
            if heard.lock().expect("nothing panics holding a time").elapsed() > SILENCE {
                break Some(format!("no word from the broker in {}s", SILENCE.as_secs()));
            }
            if pinged.elapsed() >= PING_EVERY {
                self.write(&mqtt::PINGREQ);
                pinged = Instant::now();
            }
            if self.changed() {
                packet_id = packet_id.checked_add(1).unwrap_or(1);
                self.sync(packet_id);
            }
        };
        *self.writer() = None;
        stream.shutdown(Shutdown::Both).ok();
        let lost = listener.join().unwrap_or_else(|_| "the broker's reader stopped".into());
        ended.unwrap_or(lost)
    }
}

/// A connection to [broker] that it accepted, or why there is none.
fn connected(broker: &Broker) -> std::result::Result<TcpStream, String> {
    let address = broker.address.to_socket_addrs().ok().and_then(|mut addresses| addresses.next());
    let address = address.ok_or_else(|| format!("{} is not an address", broker.address))?;
    let mut stream = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
        .map_err(|cause| format!("cannot reach the broker at {}: {cause}", broker.address))?;
    let timeouts = [stream.set_read_timeout(Some(CONNECT_TIMEOUT)), stream.set_write_timeout(Some(CONNECT_TIMEOUT))];
    if timeouts.iter().any(std::result::Result::is_err) {
        return Err("cannot set the connection's timeouts".into());
    }
    let connect = mqtt::connect(&Connect {
        client_id: &broker.client_id,
        user: broker.user.as_deref(),
        password: broker.password.as_deref(),
        will_topic: &broker.will_topic,
        keep_alive_seconds: KEEP_ALIVE_SECONDS,
    });
    stream.write_all(&connect).map_err(|cause| format!("cannot send CONNECT: {cause}"))?;
    match mqtt::read_packet(&mut stream) {
        Ok(Incoming::ConnAck { code: 0 }) => {}
        Ok(Incoming::ConnAck { code }) => return Err(mqtt::connack_refusal(code)),
        Ok(_) => return Err("the broker answered CONNECT with something else".into()),
        Err(cause) => return Err(format!("no CONNACK from the broker: {cause}")),
    }
    stream.set_read_timeout(None).map_err(|cause| format!("cannot wait on the broker: {cause}"))?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use limen_core::config::hub::HubConfig;
    use limen_core::params::{Param, ParamType};
    use std::io::Read;
    use std::net::TcpListener;

    /// A node that offers `disk` (read-only) and `restart`, and answers their runs.
    struct Nas;

    impl NodeClient for Nas {
        fn nodes(&self) -> Result<Vec<String>> {
            Ok(vec!["nas".into()])
        }

        fn call(&self, _: &str, request: &str, args: &Map<String, Value>, _: Option<Duration>) -> NodeResponse {
            let spec = |name: &str, read_only: bool| ScriptSpec {
                name: name.into(),
                description: String::new(),
                timeout_seconds: 10,
                params: vec![Param::new("path", ParamType::String, "where")],
                read_only,
            };
            if request == "hello" {
                let catalog = Catalog { scripts: vec![spec("disk", true), spec("restart", false)], problems: vec![] };
                return NodeResponse::success(json!({"catalog": catalog}), false);
            }
            let stdout = if args["script"] == "disk" { "42\n" } else { "restarted" };
            NodeResponse::success(json!({"script": args["script"], "exit": 0, "stdout": stdout, "stderr": ""}), false)
        }

        fn approval(&self) -> Result<Approval> {
            Ok(Approval::default())
        }
    }

    /// A packet the hub sent: its first byte and its body.
    fn next_packet(stream: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut first = [0u8; 2];
        stream.read_exact(&mut first[..1]).unwrap();
        let (mut length, mut shift) = (0usize, 0);
        loop {
            stream.read_exact(&mut first[1..]).unwrap();
            length |= usize::from(first[1] & 0x7F) << shift;
            shift += 7;
            if first[1] & 0x80 == 0 {
                break;
            }
        }
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).unwrap();
        (first[0], body)
    }

    /// A PUBLISH's topic, payload and retain flag.
    fn published(first: u8, body: &[u8]) -> (String, String, bool) {
        let topic_length = usize::from(u16::from_be_bytes([body[0], body[1]]));
        let topic = String::from_utf8(body[2..2 + topic_length].to_vec()).unwrap();
        (topic, String::from_utf8(body[2 + topic_length..].to_vec()).unwrap(), first & 1 == 1)
    }

    #[test]
    fn sensors_buttons_and_what_is_stale_through_a_broker() {
        let config = HubConfig::parse(
            r#"[mqtt]
url = "mqtt://127.0.0.1"
[[mqtt.sensor]]
node = "nas"
id = "disk"
script = "disk"
args = { path = "/srv" }
[[mqtt.sensor]]
node = "nas"
id = "sneaky"
script = "restart"
[[mqtt.button]]
node = "nas"
id = "restart"
script = "restart"
"#,
        )
        .unwrap();
        let mqtt = config.mqtt.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let broker = Broker {
            address: listener.local_addr().unwrap().to_string(),
            client_id: "limen-limen".into(),
            will_topic: "limen/status".into(),
            user: None,
            password: None,
        };
        let entities: Vec<(Entity, bool)> = mqtt.entities().map(|(entity, button)| (entity.clone(), button)).collect();
        let bridge =
            Bridge::start(Arc::new(Nas), Box::new(move || Ok(entities.clone())), &mqtt, broker, Box::new(|_| {}));
        let (mut hub, _) = listener.accept().unwrap();
        hub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let (connect, _) = next_packet(&mut hub);
        assert_eq!(connect, 0x10);
        hub.write_all(&[0x20, 2, 0, 0]).unwrap();
        let mut seen: BTreeMap<String, (String, bool)> = BTreeMap::new();
        let mut pressed = false;
        let expected = ["homeassistant/sensor/limen/nas-disk/config", "limen/old/gone/state"];
        while !(seen.get("limen/nas/restart/result").is_some_and(|(result, _)| result.contains("restarted"))
            && expected.iter().all(|topic| seen.contains_key(*topic)))
        {
            let (first, body) = next_packet(&mut hub);
            match first >> 4 {
                // Subscribed: the broker hands over what it retains, one of which nobody declares any more.
                8 => hub.write_all(&mqtt::publish("limen/old/gone/state", b"7", true)).unwrap(),
                3 => {
                    let (topic, payload, retained) = published(first, &body);
                    seen.insert(topic, (payload, retained));
                }
                _ => {}
            }
            if !pressed && seen.contains_key("limen/nas/disk/state") {
                hub.write_all(&mqtt::publish("limen/nas/restart/press", b"PRESS", false)).unwrap();
                pressed = true;
            }
        }
        assert_eq!(seen["limen/status"], ("online".into(), true));
        assert_eq!(seen["limen/nas/status"], ("online".into(), true));
        assert_eq!(seen["limen/nas/disk/state"], ("42".into(), true));
        assert_eq!(seen["limen/old/gone/state"], (String::new(), true), "emptied");
        assert!(seen.contains_key("homeassistant/sensor/limen/nas-disk/config"));
        assert!(seen.contains_key("homeassistant/button/limen/nas-restart/config"));
        assert!(
            !seen.keys().any(|topic| topic.contains("sneaky")),
            "a sensor on a script that changes things: {seen:?}"
        );
        assert_eq!(bridge.refusals(), ["sensor nas/sneaky: restart is not read_only on nas: a sensor only reads"]);
    }
}
