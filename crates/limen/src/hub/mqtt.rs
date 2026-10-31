//! `limen serve` and Home Assistant (spec §9.1): sensors and buttons through an MQTT broker. Nodes don't know: a
//! sensor's read and a button's press are runs of their scripts over SSH, as an MCP client's are.
//!
//! One thread keeps the connection to the broker, and another schedules each node's rounds: its catalog every minute,
//! then the sensors that are due, one at a time, so a node's sensors take one of its SSH slots at most.
//!
//! Every message goes out through one queue, put there while the state it tells is held: the broker gets them in the
//! order the state changed, and nothing waits on the network holding that state.

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
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::mpsc::{self, SyncSender, TrySendError};
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
/// Messages waiting for a broker that doesn't take them: past this, the connection is dropped and made again.
const MAX_QUEUED: usize = 4096;
/// How often the log says what a broker's clients sent the hub for nothing: they can send it all day.
const QUIET_FOR: Duration = Duration::from_secs(60);

/// What `limen.toml` declares now, read once a tick: the sensors and buttons, each with whether it is a button, the
/// nodes in `[nodes]`, and `[approval]`.
pub struct Declared {
    pub entities: Vec<(Entity, bool)>,
    pub nodes: Vec<String>,
    pub approval: Approval,
}

pub type Entities = Box<dyn Fn() -> Result<Declared> + Send + Sync>;

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
        let unusable = |why: String| error(ErrorCode::BadRequest, format!("{why} ([mqtt] in {home}/limen.toml)"));
        let password = match (sys::env_setting("LIMEN_MQTT_PASSWORD"), &mqtt.password_file) {
            (Some(password), _) => Some(password),
            (None, Some(file)) => {
                let path = if file.starts_with('/') { file.clone() } else { format!("{home}/{file}") };
                let text = fs::read_following(&path)
                    .ok_or_else(|| unusable(format!("cannot read the broker's password at {path}")))?;
                Some(text.trim().to_string())
            }
            (None, None) => None,
        };
        if password.is_some() && mqtt.user.is_none() {
            return Err(unusable("LIMEN_MQTT_PASSWORD is set and mqtt.user isn't: a password goes with a user".into()));
        }
        if password.as_ref().is_some_and(|password| password.len() > usize::from(u16::MAX)) {
            return Err(unusable("the broker's password is over 65535 bytes, more than MQTT carries".into()));
        }
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
    catalog_every: Duration,
    state: Mutex<State>,
    /// Where messages for the broker wait while there is a connection.
    outbox: Mutex<Option<SyncSender<Vec<u8>>>>,
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
    /// Until when the log says nothing more of what was sent for nothing, and how much was.
    quiet_until: Option<Instant>,
    unsaid: u64,
}

/// `sensor nas/disk_used`, as the log and `nodes` name it.
fn label(entity: &Entity, button: bool) -> String {
    format!("{} {}/{}", if button { "button" } else { "sensor" }, entity.node, entity.id)
}

impl State {
    /// The sensors and buttons of nodes in `[nodes]`: declared to the broker, refused or not. A refusal on a node's
    /// word makes one unavailable, not gone: Home Assistant keeps its name, area and dashboards.
    fn declared_entities(&self) -> impl Iterator<Item = &(Entity, bool)> {
        self.entities.iter().filter(|(entity, _)| self.nodes.contains(&entity.node))
    }

    /// The script [entity] runs, once its node's catalog accepts it.
    fn spec(&self, entity: &Entity, button: bool) -> Option<ScriptSpec> {
        if self.refused.contains_key(&label(entity, button)) {
            return None;
        }
        let (_, catalog) = self.catalogs.get(&entity.node)?;
        catalog.as_ref()?.scripts.iter().find(|spec| spec.name == entity.script).cloned()
    }

    /// [entity]'s own status: `online` once its node's catalog accepts it, `offline` while it refuses it, and none
    /// before its node answers.
    fn entity_status(&self, entity: &Entity, button: bool) -> Option<&'static str> {
        if self.refused.contains_key(&label(entity, button)) {
            return Some(mqtt::OFFLINE);
        }
        self.spec(entity, button).map(|_| mqtt::ONLINE)
    }

    /// Every topic the hub publishes to now, with the discovery payloads. An entity whose node hasn't accepted it keeps
    /// its topics —what the broker retains of it stays— and gets its payload once the node's catalog accepts it.
    fn declaration(&self, topics: &Topics) -> BTreeMap<String, String> {
        let mut declared = BTreeMap::from([(topics.hub_status(), String::new())]);
        for (entity, button) in self.declared_entities() {
            declared.insert(topics.node_status(&entity.node), String::new());
            declared.insert(topics.entity_status(entity), String::new());
            declared.insert(if *button { topics.result(entity) } else { topics.state(entity) }, String::new());
            let accepted = self.spec(entity, *button).is_some();
            for (topic, payload) in topics.discovery_payloads(entity, *button) {
                declared.insert(topic, if accepted { payload.to_string() } else { String::new() });
            }
        }
        declared
    }

    /// Every entity's own status, as [State::entity_status] says.
    fn entity_statuses(&self, topics: &Topics) -> BTreeMap<String, &'static str> {
        self.declared_entities()
            .filter_map(|(entity, button)| Some((topics.entity_status(entity), self.entity_status(entity, *button)?)))
            .collect()
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
        let bridge = Arc::new(Bridge::new(client, entities, mqtt, log, CATALOG_EVERY));
        bridge.clone().launch(broker);
        bridge
    }

    fn new(client: Arc<dyn NodeClient>, entities: Entities, mqtt: &Mqtt, log: Sink, catalog_every: Duration) -> Self {
        Bridge {
            client,
            entities,
            topics: Topics::of(mqtt),
            log,
            catalog_every,
            state: Mutex::new(State::default()),
            outbox: Mutex::new(None),
        }
    }

    fn launch(self: Arc<Self>, broker: Broker) {
        self.refresh();
        let scheduler = self.clone();
        std::thread::spawn(move || scheduler.schedule());
        std::thread::spawn(move || self.connect_forever(&broker));
    }

    /// The sensors and buttons refused, and why: for `nodes`.
    pub fn refusals(&self) -> Vec<String> {
        self.state().refused.iter().map(|(label, why)| format!("{label}: {why}")).collect()
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().expect("nothing panics holding the bridge's state")
    }

    /// Queues [packet] for the broker, if there is a connection: what isn't sent now is sent again on the next. A
    /// broker that doesn't keep up loses its connection, and gets everything again on the next.
    fn send(&self, packet: Vec<u8>) {
        let mut outbox = self.outbox.lock().expect("nothing panics holding the outbox");
        if let Some(queue) = outbox.as_ref()
            && let Err(TrySendError::Full(_)) = queue.try_send(packet)
        {
            *outbox = None;
        }
    }

    /// Publishes [payload] retained.
    fn publish(&self, topic: &str, payload: &[u8]) {
        self.send(mqtt::publish(topic, payload, true));
    }

    /// Says [what] in the log, unless it was said less than [QUIET_FOR] ago: a broker's clients send what they like.
    fn say_now_and_then(&self, state: &mut State, what: &str) {
        let now = Instant::now();
        if state.quiet_until.is_some_and(|until| now < until) {
            state.unsaid += 1;
            return;
        }
        let unsaid = std::mem::take(&mut state.unsaid);
        let more = if unsaid > 0 { format!(" ({unsaid} more like it unsaid)") } else { String::new() };
        (self.log)(&format!("mqtt: {what}{more}"));
        state.quiet_until = Some(now + QUIET_FOR);
    }

    /// Reads `limen.toml` again and decides what is refused; a file that doesn't read leaves things as they were.
    fn refresh(&self) {
        let Ok(declared) = (self.entities)() else { return };
        let mut state = self.state();
        state.entities = declared.entities;
        state.nodes = declared.nodes;
        state.approval = declared.approval;
        self.judge(&mut state);
    }

    /// Decides what is refused on what [state] knows, in the same hold of its lock as what changed it, and tells the
    /// broker each entity whose status changed.
    fn judge(&self, state: &mut State) {
        let before = state.entity_statuses(&self.topics);
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
        for (topic, status) in state.entity_statuses(&self.topics) {
            if before.get(&topic) != Some(&status) {
                self.publish(&topic, status.as_bytes());
            }
        }
    }

    /// Starts a round on every node in `[nodes]` with a sensor or a button and none under way, every [TICK]: refused
    /// ones too, since a node may come to offer what it refused.
    fn schedule(self: Arc<Self>) {
        loop {
            self.refresh();
            let idle: BTreeSet<String> = {
                let mut state = self.state();
                let nodes: BTreeSet<String> =
                    state.declared_entities().map(|(entity, _)| entity.node.clone()).collect();
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
        let catalog_due =
            self.state().catalogs.get(node).is_none_or(|(asked, _)| asked.elapsed() >= self.catalog_every);
        if catalog_due {
            let hello = self.client.call(node, "hello", &Map::new(), None);
            let mut state = self.state();
            self.set_online(&mut state, node, mqtt::answered(&hello));
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
            .entities
            .iter()
            .filter(|(entity, button)| !button && entity.node == node)
            .filter_map(|(sensor, _)| Some((sensor.clone(), state.spec(sensor, false)?)))
            .filter(|(sensor, _)| state.next_read.get(&self.topics.state(sensor)).is_none_or(|next| *next <= now))
            .collect();
        for (sensor, _) in &ready {
            state.next_read.insert(self.topics.state(sensor), now + sensor.every);
        }
        ready
    }

    /// Runs [sensor]'s script, as a read the node checks is one, and publishes its state if it changed.
    fn read(&self, sensor: &Entity, spec: &ScriptSpec) {
        let topic = self.topics.state(sensor);
        let response = self.run(sensor, spec, true);
        let mut state = self.state();
        self.set_online(&mut state, &sensor.node, mqtt::answered(&response));
        match mqtt::sensor_state(&response) {
            Ok(value) => {
                state.failing.remove(&topic);
                if state.retained.get(&topic) != Some(&value) {
                    self.publish(&topic, value.as_bytes());
                    state.retained.insert(topic, value);
                }
            }
            Err(why) => {
                if state.failing.insert(topic) {
                    (self.log)(&format!("mqtt: {} failing: {why}", label(sensor, false)));
                }
            }
        }
    }

    /// [entity]'s script on its node, with its fixed arguments, as a tool call would run it; a [read] the node
    /// refuses unless the script's header says `read_only` as it is now.
    fn run(&self, entity: &Entity, spec: &ScriptSpec, read: bool) -> NodeResponse {
        let mut args = Map::new();
        args.insert("script".into(), json!(entity.script));
        args.insert("args".into(), Value::Object(entity.args.clone()));
        if read {
            args.insert("read_only".into(), json!(true));
        }
        let timeout = Duration::from_secs(spec.timeout_seconds).saturating_add(mcp::SCRIPT_MARGIN);
        self.client.call(&entity.node, "run", &args, Some(timeout))
    }

    /// Whether [node] answers: said to the broker and the log when it changes.
    fn set_online(&self, state: &mut State, node: &str, online: bool) {
        if state.online.insert(node.to_string(), online) == Some(online) {
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
            self.say_now_and_then(
                &mut state,
                &format!("a press of {node}/{id}, which is no button ready to run, ignored"),
            );
            return;
        };
        let pressing = self.topics.press(&button);
        if !state.busy.insert(pressing.clone()) {
            self.say_now_and_then(&mut state, &format!("a press of {} while it runs, ignored", label(&button, true)));
            return;
        }
        // A press published retained reaches the hub as any other, and would press again on the next connection. Once
        // per run: a flood of presses fills no queue.
        self.publish(&pressing, b"");
        drop(state);
        let bridge = self.clone();
        std::thread::spawn(move || {
            bridge.run_button(&button, &spec);
            bridge.state().busy.remove(&pressing);
        });
    }

    fn run_button(&self, button: &Entity, spec: &ScriptSpec) {
        let started = Instant::now();
        let response = self.run(button, spec, false);
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
        let mut state = self.state();
        self.set_online(&mut state, &button.node, mqtt::answered(&response));
        self.publish(&topic, payload.as_bytes());
        state.retained.insert(topic, payload);
    }

    /// A packet from the broker: a press, a retained message this hub no longer declares, which it empties, or the
    /// broker's answer to a subscription.
    fn received(self: &Arc<Self>, packet: Incoming, subscribed: &[String]) {
        match packet {
            Incoming::Publish { topic, payload, retained: false } => {
                // Empty: what clears a topic, the hub's own clearing of a press included.
                if let Some((node, id)) = self.topics.pressed(&topic).filter(|_| !payload.is_empty()) {
                    self.press(node, id);
                }
            }
            Incoming::Publish { topic, payload, retained: true } => {
                let state = self.state();
                if !payload.is_empty() && state.declared.as_ref().is_some_and(|declared| !declared.contains_key(&topic))
                {
                    self.publish(&topic, b"");
                }
            }
            Incoming::Oversized { topic: Some(topic), retained: true } => {
                // Its own are small: someone else's, under its topics. Emptied, and, on one of the hub's own topics,
                // everything told again on the next tick. On any other, once: a broker that lets the hub read it but
                // not write it would hand it over again on every subscription.
                let mut state = self.state();
                self.publish(&topic, b"");
                if state.declared.as_ref().is_some_and(|declared| declared.contains_key(&topic)) {
                    state.declared = None;
                }
                self.say_now_and_then(&mut state, &format!("a message of over 64 KiB retained on {topic}, emptied"));
            }
            Incoming::SubAck { codes } => {
                for (filter, _) in subscribed.iter().zip(codes).filter(|(_, code)| *code == 0x80) {
                    (self.log)(&format!(
                        "mqtt: the broker refused the subscription to {filter}: no press arrives and nothing stale is \
                         emptied there; give the user access to it"
                    ));
                }
            }
            Incoming::Oversized { .. } | Incoming::ConnAck { .. } | Incoming::Other => {}
        }
    }

    /// Tells the broker everything again —statuses, discovery, states and results— and subscribes, so it hands over
    /// what is retained under the hub's topics, and what is stale there gets emptied.
    fn sync(&self, packet_id: u16) {
        let mut state = self.state();
        let declared = state.declaration(&self.topics);
        state.retained.retain(|topic, _| declared.contains_key(topic));
        self.publish(&self.topics.hub_status(), mqtt::ONLINE.as_bytes());
        for (node, online) in &state.online {
            let topic = self.topics.node_status(node);
            if declared.contains_key(&topic) {
                self.publish(&topic, if *online { mqtt::ONLINE } else { mqtt::OFFLINE }.as_bytes());
            }
        }
        for (topic, status) in state.entity_statuses(&self.topics) {
            self.publish(&topic, status.as_bytes());
        }
        for (topic, payload) in declared.iter().chain(&state.retained).filter(|(_, payload)| !payload.is_empty()) {
            self.publish(topic, payload.as_bytes());
        }
        state.declared = Some(declared);
        self.send(mqtt::subscribe(packet_id, &self.topics.filters()));
    }

    /// Whether what the hub declares changed since the broker was last told.
    fn changed(&self) -> bool {
        let state = self.state();
        state.declared.as_ref() != Some(&state.declaration(&self.topics))
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
        let (Ok(mut writer), Ok(mut reader)) = (stream.try_clone(), stream.try_clone()) else {
            return "cannot share the connection between threads".into();
        };
        (self.log)(&format!("mqtt: connected to {}", broker.address));
        let (queue, queued) = mpsc::sync_channel::<Vec<u8>>(MAX_QUEUED);
        *self.outbox.lock().expect("nothing panics holding the outbox") = Some(queue);
        // Ends when the session drops the queue, or the broker stops taking what is sent; either ends the connection.
        std::thread::spawn(move || {
            for packet in queued {
                if writer.write_all(&packet).is_err() {
                    break;
                }
            }
            writer.shutdown(Shutdown::Both).ok();
        });
        let heard = Arc::new(Mutex::new(Instant::now()));
        let subscribed = self.topics.filters();
        let listener = {
            let (bridge, heard) = (self.clone(), heard.clone());
            std::thread::spawn(move || {
                loop {
                    match mqtt::read_packet(&mut reader) {
                        Ok(packet) => {
                            *heard.lock().expect("nothing panics holding a time") = Instant::now();
                            bridge.received(packet, &subscribed);
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
                self.send(mqtt::PINGREQ.to_vec());
                pinged = Instant::now();
            }
            if self.changed() {
                packet_id = packet_id.checked_add(1).unwrap_or(1);
                self.sync(packet_id);
            }
        };
        *self.outbox.lock().expect("nothing panics holding the outbox") = None;
        stream.shutdown(Shutdown::Both).ok();
        let lost = listener.join().unwrap_or_else(|_| "the broker's reader stopped".into());
        ended.unwrap_or(lost)
    }
}

/// A connection to [broker] that it accepted, or why there is none.
fn connected(broker: &Broker) -> std::result::Result<TcpStream, String> {
    // Parsed, never resolved: `to_socket_addrs` would ask libc for a name.
    let address: SocketAddr = broker.address.parse().map_err(|_| format!("{} is not an address", broker.address))?;
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
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// A node that offers `disk` (read-only) and, once installed, `restart`; it answers their runs and counts the
    /// restarts.
    struct Nas {
        installed: AtomicBool,
        restarts: AtomicUsize,
    }

    impl Nas {
        fn new(installed: bool) -> Arc<Self> {
            Arc::new(Nas { installed: AtomicBool::new(installed), restarts: AtomicUsize::new(0) })
        }
    }

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
                let mut scripts = vec![spec("disk", true)];
                if self.installed.load(Ordering::SeqCst) {
                    scripts.push(spec("restart", false));
                }
                return NodeResponse::success(json!({"catalog": Catalog { scripts, problems: vec![] }}), false);
            }
            if args["script"] == "disk" && args.get("read_only") != Some(&json!(true)) {
                return NodeResponse::failure(&error(ErrorCode::Denied, "a sensor's read must ask for read_only"));
            }
            let stdout = if args["script"] == "disk" { "42\n" } else { "restarted" };
            if args["script"] == "restart" {
                self.restarts.fetch_add(1, Ordering::SeqCst);
            }
            NodeResponse::success(json!({"script": args["script"], "exit": 0, "stdout": stdout, "stderr": ""}), false)
        }

        fn approval(&self) -> Result<Approval> {
            Ok(Approval::default())
        }
    }

    const ENTITIES: &str = r#"[mqtt]
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
"#;

    /// A bridge for [client] and the [declared] sensors and buttons, with its broker at [address], asking for catalogs
    /// every [catalog_every].
    fn bridge(client: Arc<Nas>, declared: &str, address: String, catalog_every: Duration) -> Arc<Bridge> {
        let mqtt = HubConfig::parse(declared).unwrap().mqtt.unwrap();
        let entities: Vec<(Entity, bool)> = mqtt.entities().map(|(entity, button)| (entity.clone(), button)).collect();
        let broker = Broker {
            address,
            client_id: "limen-limen".into(),
            will_topic: "limen/status".into(),
            user: None,
            password: None,
        };
        let entities = Box::new(move || {
            Ok(Declared { entities: entities.clone(), nodes: vec!["nas".into()], approval: Approval::default() })
        });
        let bridge = Arc::new(Bridge::new(client, entities, &mqtt, Box::new(|_| {}), catalog_every));
        bridge.clone().launch(broker);
        bridge
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
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let nas = Nas::new(true);
        let bridge = bridge(nas.clone(), ENTITIES, listener.local_addr().unwrap().to_string(), CATALOG_EVERY);
        let (mut hub, _) = listener.accept().unwrap();
        hub.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        let (connect, _) = next_packet(&mut hub);
        assert_eq!(connect, 0x10);
        hub.write_all(&[0x20, 2, 0, 0]).unwrap();
        // Every message to each topic, in order.
        let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let last =
            |seen: &BTreeMap<String, Vec<String>>, topic: &str| seen.get(topic).and_then(|all| all.last()).cloned();
        let mut pressed = false;
        // The hub pings and resends on its own, so a read never times out: a broken promise must end the test.
        let started = Instant::now();
        while !(last(&seen, "limen/nas/restart/result").is_some_and(|result| result.contains("restarted"))
            && ["homeassistant/sensor/limen/nas-disk/config", "limen/old/gone/state"]
                .iter()
                .all(|topic| seen.contains_key(*topic))
            && seen.get("limen/nas/disk/status").is_some_and(|all| {
                // Emptied, then told again.
                all.iter().any(String::is_empty) && all.last().is_some_and(|status| status == "online")
            }))
        {
            assert!(started.elapsed() < Duration::from_secs(30), "not everything arrived: {seen:?}");
            let (first, body) = next_packet(&mut hub);
            match first >> 4 {
                // Subscribed: the broker hands over what it retains. One topic nobody declares any more, one the hub
                // does, a press left retained, and something too large to keep.
                8 => {
                    for (topic, payload) in [
                        ("limen/old/gone/state", b"7".to_vec()),
                        ("limen/nas/disk/state", b"41".to_vec()),
                        ("limen/nas/restart/press", b"PRESS".to_vec()),
                        ("limen/nas/disk/status", vec![b'x'; 70_000]),
                    ] {
                        hub.write_all(&mqtt::publish(topic, &payload, true)).unwrap();
                    }
                }
                3 => {
                    let (topic, payload, _) = published(first, &body);
                    seen.entry(topic).or_default().push(payload);
                }
                _ => {}
            }
            if !pressed && seen.contains_key("limen/nas/disk/state") {
                hub.write_all(&mqtt::publish("limen/nas/restart/press", b"PRESS", false)).unwrap();
                pressed = true;
            }
        }
        assert_eq!(last(&seen, "limen/status").unwrap(), "online");
        assert_eq!(last(&seen, "limen/nas/status").unwrap(), "online");
        assert!(seen["limen/nas/disk/state"].iter().all(|state| state == "42"), "a declared topic is not emptied");
        assert_eq!(last(&seen, "limen/old/gone/state").unwrap(), "", "emptied");
        assert_eq!(last(&seen, "limen/nas/restart/press").unwrap(), "", "the retained press emptied");
        assert_eq!(nas.restarts.load(Ordering::SeqCst), 1, "a retained press doesn't run, the live one does");
        assert!(seen.contains_key("homeassistant/button/limen/nas-restart/config"));
        assert!(
            !seen.contains_key("homeassistant/sensor/limen/nas-sneaky/config"),
            "nothing of a sensor its node refuses is published"
        );
        assert_eq!(last(&seen, "limen/nas/sneaky/status").unwrap(), "offline", "refused: unavailable");
        assert_eq!(bridge.refusals(), ["sensor nas/sneaky: restart is not read_only on nas: a sensor only reads"]);
    }

    #[test]
    fn what_a_node_refuses_stays_declared_and_what_is_taken_out_goes() {
        let mqtt = HubConfig::parse(ENTITIES).unwrap().mqtt.unwrap();
        let topics = Topics::of(&mqtt);
        let mut state = State {
            entities: mqtt.entities().map(|(entity, button)| (entity.clone(), button)).collect(),
            nodes: vec!["nas".into()],
            ..State::default()
        };
        state.refused.insert("sensor nas/sneaky".into(), "not read_only".into());
        let declared = state.declaration(&topics);
        // Declared without a payload: the hub neither publishes it nor empties what the broker keeps of it, so Home
        // Assistant keeps the entity, unavailable, for when the node offers it again.
        assert_eq!(declared.get("homeassistant/sensor/limen/nas-sneaky/config").map(String::as_str), Some(""));
        assert!(declared.contains_key("limen/nas/sneaky/status"));
        state.nodes.clear();
        assert!(!state.declaration(&topics).keys().any(|topic| topic.contains("nas")), "forgotten: emptied");
    }

    #[test]
    fn a_node_that_comes_to_offer_a_script_is_asked_again() {
        // The node's only entity refused: no catalog offers `restart` yet. Nothing listens on port 1.
        let only_the_button = "[mqtt]\nurl = \"mqtt://127.0.0.1\"\n[[mqtt.button]]\nnode = \"nas\"\nid = \"restart\"\n\
                               script = \"restart\"\n";
        let nas = Nas::new(false);
        let bridge = bridge(nas.clone(), only_the_button, "127.0.0.1:1".into(), Duration::from_millis(500));
        let wait_for = |refused: bool| {
            let started = Instant::now();
            while bridge.refusals().is_empty() == refused && started.elapsed() < Duration::from_secs(10) {
                std::thread::sleep(Duration::from_millis(100));
            }
        };
        wait_for(true);
        assert_eq!(bridge.refusals(), ["button nas/restart: nas offers no script restart"]);
        nas.installed.store(true, Ordering::SeqCst);
        wait_for(false);
        assert!(bridge.refusals().is_empty(), "{:?}", bridge.refusals());
    }
}
