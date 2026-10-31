//! MQTT for Home Assistant (spec §9.1): MQTT 3.1.1 packets at QoS 0, the hub's topics, the discovery payloads, and
//! what a node's answer becomes on them. Pure: the hub's `hub::mqtt` does the network.

use crate::config::hub::Approval;
use crate::config::mqtt::{Entity, Mqtt};
use crate::params;
use crate::protocol::NodeResponse;
use crate::scripts::Catalog;
use serde_json::{Map, Value, json};
use std::io::{self, Read};

/// The most a sensor's state or a button's result may hold.
pub const MAX_PAYLOAD: usize = 16 * 1024;
/// The largest packet the hub takes from the broker: what it reads back is its own, and small.
pub const MAX_PACKET: usize = 64 * 1024;
pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";
/// How Home Assistant turns a button's result into its sensor's state: the exit code, or the error's code.
const RESULT_TEMPLATE: &str = "{{ value_json.data.exit if value_json.ok else value_json.error.code }}";

pub const PINGREQ: [u8; 2] = [0xC0, 0];

/// What the broker sends the hub.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    ConnAck {
        code: u8,
    },
    Publish {
        topic: String,
        payload: Vec<u8>,
        retained: bool,
    },
    /// A PUBLISH over [MAX_PACKET], read past: its topic, to empty it, if the topic itself fit.
    Oversized {
        topic: Option<String>,
        retained: bool,
    },
    /// Which filters the broker granted, in the order they were asked: 0x80 is a refusal.
    SubAck {
        codes: Vec<u8>,
    },
    /// PINGRESP and anything else the hub only needs to know arrived.
    Other,
}

/// What CONNECT carries: who the hub is, and the will that says it is gone.
pub struct Connect<'a> {
    pub client_id: &'a str,
    pub user: Option<&'a str>,
    pub password: Option<&'a str>,
    pub will_topic: &'a str,
    pub keep_alive_seconds: u16,
}

pub fn connect(connect: &Connect) -> Vec<u8> {
    // Clean session; a retained will at QoS 0.
    let mut flags = 0x02 | 0x04 | 0x20;
    let mut body = Vec::new();
    put_bytes(&mut body, b"MQTT");
    body.push(4);
    let flags_at = body.len();
    body.push(0);
    body.extend(connect.keep_alive_seconds.to_be_bytes());
    put_bytes(&mut body, connect.client_id.as_bytes());
    put_bytes(&mut body, connect.will_topic.as_bytes());
    put_bytes(&mut body, OFFLINE.as_bytes());
    if let Some(user) = connect.user {
        flags |= 0x80;
        put_bytes(&mut body, user.as_bytes());
        if let Some(password) = connect.password {
            flags |= 0x40;
            put_bytes(&mut body, password.as_bytes());
        }
    }
    body[flags_at] = flags;
    packet(0x10, &body)
}

pub fn publish(topic: &str, payload: &[u8], retain: bool) -> Vec<u8> {
    let mut body = Vec::new();
    put_bytes(&mut body, topic.as_bytes());
    body.extend(payload);
    packet(0x30 | u8::from(retain), &body)
}

pub fn subscribe(packet_id: u16, filters: &[String]) -> Vec<u8> {
    let mut body = packet_id.to_be_bytes().to_vec();
    for filter in filters {
        put_bytes(&mut body, filter.as_bytes());
        body.push(0);
    }
    packet(0x82, &body)
}

/// What a CONNACK's refusal means, and what to do about it.
pub fn connack_refusal(code: u8) -> String {
    match code {
        1 => "the broker doesn't speak MQTT 3.1.1".into(),
        2 => "the broker refused the client id limen-<prefix>".into(),
        3 => "the broker is unavailable".into(),
        4 => "wrong user or password: mqtt.user, and LIMEN_MQTT_PASSWORD or mqtt.password_file".into(),
        5 => "the broker doesn't authorise this user".into(),
        other => format!("the broker refused the connection (code {other})"),
    }
}

/// The next packet from [reader]. One over [MAX_PACKET] is read past, never kept: a retained one would otherwise come
/// back on every connection and end each of them.
pub fn read_packet(reader: &mut impl Read) -> io::Result<Incoming> {
    let mut first = [0u8; 1];
    reader.read_exact(&mut first)?;
    let mut length = 0usize;
    for shift in [0, 7, 14, 21] {
        let mut byte = [0u8; 1];
        reader.read_exact(&mut byte)?;
        length |= usize::from(byte[0] & 0x7F) << shift;
        if byte[0] & 0x80 == 0 {
            break;
        }
    }
    if length > MAX_PACKET {
        return oversized(reader, first[0], length);
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    parse(first[0], &body)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "a malformed packet from the broker"))
}

fn oversized(reader: &mut impl Read, first: u8, length: usize) -> io::Result<Incoming> {
    let mut topic_length = [0u8; 2];
    reader.read_exact(&mut topic_length)?;
    let topic_length = usize::from(u16::from_be_bytes(topic_length));
    let mut topic = vec![0u8; topic_length.min(length - 2)];
    reader.read_exact(&mut topic)?;
    let rest = (length - 2 - topic.len()) as u64;
    if io::copy(&mut reader.take(rest), &mut io::sink())? != rest {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    let topic = (first >> 4 == 3).then(|| String::from_utf8(topic).ok()).flatten();
    Ok(Incoming::Oversized { topic, retained: first & 0x01 == 1 })
}

fn parse(first: u8, body: &[u8]) -> Option<Incoming> {
    match first >> 4 {
        2 => Some(Incoming::ConnAck { code: *body.get(1)? }),
        3 => {
            let topic_length = usize::from(u16::from_be_bytes([*body.first()?, *body.get(1)?]));
            let topic = String::from_utf8(body.get(2..2 + topic_length)?.to_vec()).ok()?;
            // At QoS 1 or 2 a packet id comes first; the hub subscribes at 0, so the broker never sends one.
            let id_length = if first & 0x06 == 0 { 0 } else { 2 };
            let payload = body.get(2 + topic_length + id_length..)?.to_vec();
            Some(Incoming::Publish { topic, payload, retained: first & 0x01 == 1 })
        }
        9 => Some(Incoming::SubAck { codes: body.get(2..)?.to_vec() }),
        _ => Some(Incoming::Other),
    }
}

fn put_bytes(body: &mut Vec<u8>, bytes: &[u8]) {
    let length = u16::try_from(bytes.len()).expect("limen's strings are short");
    body.extend(length.to_be_bytes());
    body.extend(bytes);
}

fn packet(first: u8, body: &[u8]) -> Vec<u8> {
    let mut packet = vec![first];
    let mut length = body.len();
    loop {
        let byte = (length % 128) as u8;
        length /= 128;
        packet.push(if length > 0 { byte | 0x80 } else { byte });
        if length == 0 {
            break;
        }
    }
    packet.extend(body);
    packet
}

/// The hub's topics, under its prefix and Home Assistant's discovery prefix.
pub struct Topics {
    pub prefix: String,
    pub discovery: Option<String>,
}

impl Topics {
    pub fn of(mqtt: &Mqtt) -> Self {
        Topics { prefix: mqtt.prefix.clone(), discovery: mqtt.discovery.clone() }
    }

    pub fn hub_status(&self) -> String {
        format!("{}/status", self.prefix)
    }

    pub fn node_status(&self, node: &str) -> String {
        format!("{}/{node}/status", self.prefix)
    }

    /// Whether the node's catalog accepts [entity]: `offline` while it refuses it, and Home Assistant keeps it.
    pub fn entity_status(&self, entity: &Entity) -> String {
        self.of_entity(entity, "status")
    }

    pub fn state(&self, entity: &Entity) -> String {
        self.of_entity(entity, "state")
    }

    pub fn press(&self, entity: &Entity) -> String {
        self.of_entity(entity, "press")
    }

    pub fn result(&self, entity: &Entity) -> String {
        self.of_entity(entity, "result")
    }

    fn of_entity(&self, entity: &Entity, what: &str) -> String {
        format!("{}/{}/{}/{what}", self.prefix, entity.node, entity.id)
    }

    /// Where Home Assistant finds [entity] as a [component] (`sensor` or `button`); nowhere without discovery.
    pub fn config(&self, component: &str, entity: &Entity) -> Option<String> {
        let discovery = self.discovery.as_ref()?;
        Some(format!("{discovery}/{component}/{}/{}-{}/config", self.prefix, entity.node, entity.id))
    }

    /// What the hub subscribes to: presses, and the retained messages it left before.
    pub fn filters(&self) -> Vec<String> {
        let mut filters = vec![format!("{}/#", self.prefix)];
        filters.extend(self.discovery.as_ref().map(|discovery| format!("{discovery}/+/{}/+/config", self.prefix)));
        filters
    }

    /// The node and id of a press topic.
    pub fn pressed<'a>(&self, topic: &'a str) -> Option<(&'a str, &'a str)> {
        let rest = topic.strip_prefix(&self.prefix)?.strip_prefix('/')?.strip_suffix("/press")?;
        rest.split_once('/').filter(|(_, id)| !id.contains('/'))
    }

    /// Every discovery payload of [entity] —a button has a sensor for its result too— with where it goes.
    pub fn discovery_payloads(&self, entity: &Entity, button: bool) -> Vec<(String, Value)> {
        let unique_id = format!("{}-{}-{}", self.prefix, entity.node, entity.id);
        let mut payloads = Vec::new();
        if button {
            let mut config = self.base(entity, &unique_id, &entity.id);
            config.insert("command_topic".into(), json!(self.press(entity)));
            config.extend(entity.discovery.clone());
            payloads.extend(self.config("button", entity).map(|topic| (topic, Value::Object(config))));
            let mut result = self.base(entity, &format!("{unique_id}-result"), &format!("{} result", entity.id));
            result.insert("state_topic".into(), json!(self.result(entity)));
            result.insert("json_attributes_topic".into(), json!(self.result(entity)));
            result.insert("value_template".into(), json!(RESULT_TEMPLATE));
            result.insert("entity_category".into(), json!("diagnostic"));
            payloads.extend(self.config("sensor", entity).map(|topic| (topic, Value::Object(result))));
        } else {
            let mut config = self.base(entity, &unique_id, &entity.id);
            config.insert("state_topic".into(), json!(self.state(entity)));
            config.extend(entity.discovery.clone());
            payloads.extend(self.config("sensor", entity).map(|topic| (topic, Value::Object(config))));
        }
        payloads
    }

    /// What every entity's discovery says: its id, its name, its node as a device, and when it is available.
    fn base(&self, entity: &Entity, unique_id: &str, name: &str) -> Map<String, Value> {
        json!({
            "unique_id": unique_id,
            "name": name,
            "device": {"identifiers": [format!("{}-{}", self.prefix, entity.node)], "name": entity.node},
            "availability": [
                {"topic": self.hub_status()},
                {"topic": self.node_status(&entity.node)},
                {"topic": self.entity_status(entity)},
            ],
            "availability_mode": "all",
        })
        .as_object()
        .cloned()
        .expect("a JSON object literal")
    }
}

/// Why [entity] is refused on its node (spec §9.1), if it is: decided on the node's [catalog], unknown until it
/// answers. [nodes] are those in `[nodes]`.
pub fn refusal(
    entity: &Entity,
    button: bool,
    nodes: &[String],
    catalog: Option<&Catalog>,
    approval: &Approval,
) -> Option<String> {
    if !nodes.contains(&entity.node) {
        return Some(format!("no node {} in [nodes]", entity.node));
    }
    let spec = catalog?.scripts.iter().find(|spec| spec.name == entity.script);
    let Some(spec) = spec else { return Some(format!("{} offers no script {}", entity.node, entity.script)) };
    if !button && !spec.read_only {
        return Some(format!("{} is not read_only on {}: a sensor only reads", entity.script, entity.node));
    }
    if approval.needed(&entity.script, spec.read_only) {
        return Some(format!(
            "[approval] asks a person before {} runs, and over MQTT there is nobody to ask",
            spec.name
        ));
    }
    params::validate(&spec.params, &entity.args).err().map(|refused| format!("args: {}", refused.message))
}

/// A sensor's state out of its script's run: its trimmed stdout, if it ran, exited 0 and was not cut; else why not.
pub fn sensor_state(response: &NodeResponse) -> Result<String, String> {
    if let Some(failure) = &response.error {
        if failure.message == "unknown argument 'read_only'" {
            return Err("the node's limen predates sensors, which it checks read-only: update limen there".into());
        }
        return Err(failure.summary());
    }
    let data = response.data.as_ref().ok_or("no answer")?;
    if response.truncated {
        return Err("the node cut its output at one of its limits".into());
    }
    match data.get("exit").and_then(Value::as_i64) {
        Some(0) => {}
        exit => return Err(format!("exit {}", exit.map_or("unknown".into(), |exit| exit.to_string()))),
    }
    let state = data.get("stdout").and_then(Value::as_str).unwrap_or_default().trim();
    if state.len() > MAX_PAYLOAD {
        return Err(format!("{} bytes of output, over {MAX_PAYLOAD}", state.len()));
    }
    Ok(state.to_string())
}

/// A button's result: the node's answer, without its output when it would pass [MAX_PAYLOAD].
pub fn result_payload(response: &NodeResponse) -> String {
    let whole = serde_json::to_string(response).expect("an answer serializes");
    if whole.len() <= MAX_PAYLOAD {
        return whole;
    }
    let mut cut = response.clone();
    if let Some(Value::Object(data)) = cut.data.as_mut() {
        data.shift_remove("stdout");
        data.shift_remove("stderr");
    }
    cut.truncated = true;
    serde_json::to_string(&cut).expect("an answer serializes")
}

/// Whether the hub reached the node: anything but ssh's failure to connect. A timeout is not one: the node's own, of
/// a slow script, carries the same code as the hub's, and a node that is reached and slow is not down.
pub fn answered(response: &NodeResponse) -> bool {
    !response
        .error
        .as_ref()
        .is_some_and(|failure| ["unreachable", "host_key_mismatch"].contains(&failure.code.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::hub::{ApprovalScripts, HubConfig};
    use crate::params::{Param, ParamType};
    use crate::protocol::{ErrorCode, error};
    use crate::scripts::ScriptSpec;

    fn mqtt() -> Mqtt {
        HubConfig::parse(
            r#"[mqtt]
url = "mqtt://10.0.0.1"
[[mqtt.sensor]]
node = "nas-2"
id = "disk_used"
script = "disk_used_percent"
args = { path = "/srv" }
discovery = { unit_of_measurement = "%", name = "Disk" }
[[mqtt.button]]
node = "nas-2"
id = "restart"
script = "restart_container"
args = { name = "immich" }
"#,
        )
        .unwrap()
        .mqtt
        .unwrap()
    }

    #[test]
    fn packets_as_the_standard_writes_them() {
        assert_eq!(publish("a/b", b"on", true), [0x31, 7, 0, 3, b'a', b'/', b'b', b'o', b'n']);
        assert_eq!(subscribe(1, &["x/#".into()]), [0x82, 8, 0, 1, 0, 3, b'x', b'/', b'#', 0]);
        let connect = connect(&Connect {
            client_id: "limen-home",
            user: Some("limen"),
            password: Some("secret"),
            will_topic: "home/status",
            keep_alive_seconds: 60,
        });
        assert_eq!(&connect[..2], [0x10, 59]);
        assert_eq!(&connect[2..10], [0, 4, b'M', b'Q', b'T', b'T', 4, 0xE6], "user, password, retained will, clean");
        assert!(connect.ends_with(b"\0\x06secret"));
        // A remaining length past 127 takes a second byte.
        assert_eq!(&publish("t", &[0; 200], false)[..3], [0x30, 203, 1]);
    }

    #[test]
    fn packets_from_the_broker() {
        let read = |bytes: &[u8]| read_packet(&mut &bytes[..]);
        assert_eq!(read(&[0x20, 2, 0, 5]).unwrap(), Incoming::ConnAck { code: 5 });
        assert_eq!(
            read(&[0x31, 6, 0, 1, b't', b'o', b'n', b'!']).unwrap(),
            Incoming::Publish { topic: "t".into(), payload: b"on!".to_vec(), retained: true }
        );
        assert_eq!(
            read(&[0x32, 5, 0, 1, b't', 0, 9]).unwrap(),
            Incoming::Publish { topic: "t".into(), payload: vec![], retained: false }
        );
        assert_eq!(read(&[0xD0, 0]).unwrap(), Incoming::Other);
        let mut huge = vec![0x31, 0x86, 0x80, 0x04, 0, 1, b't'];
        huge.extend(vec![b'x'; 65536 + 3]);
        huge.extend([0xD0, 0]);
        let mut stream = &huge[..];
        assert_eq!(read_packet(&mut stream).unwrap(), Incoming::Oversized { topic: Some("t".into()), retained: true });
        assert_eq!(read_packet(&mut stream).unwrap(), Incoming::Other, "read past, and the next packet follows");
        assert!(read(&[0x30, 0xFF, 0xFF, 0x04, 0, 1]).is_err(), "cut short");
        assert_eq!(read(&[0x90, 4, 0, 1, 0, 0x80]).unwrap(), Incoming::SubAck { codes: vec![0, 0x80] });
        assert!(read(&[0x30, 3, 0, 9, b't']).is_err(), "a topic longer than its packet");
        assert!(read(&[0x30, 5, 0]).is_err(), "cut short");
    }

    #[test]
    fn topics_and_discovery() {
        let mqtt = mqtt();
        let topics = Topics::of(&mqtt);
        let (sensor, button) = (&mqtt.sensors[0], &mqtt.buttons[0]);
        assert_eq!(topics.state(sensor), "limen/nas-2/disk_used/state");
        assert_eq!(topics.filters(), ["limen/#", "homeassistant/+/limen/+/config"]);
        assert_eq!(topics.pressed("limen/nas-2/restart/press"), Some(("nas-2", "restart")));
        assert_eq!(topics.pressed("limen/nas-2/restart/state"), None);
        assert_eq!(topics.pressed("limen/a/b/c/press"), None);
        let [(topic, config)] = topics.discovery_payloads(sensor, false).try_into().unwrap();
        assert_eq!(topic, "homeassistant/sensor/limen/nas-2-disk_used/config");
        assert_eq!(config["state_topic"], "limen/nas-2/disk_used/state");
        assert_eq!((config["name"].as_str(), config["unit_of_measurement"].as_str()), (Some("Disk"), Some("%")));
        assert_eq!(config["device"]["identifiers"][0], "limen-nas-2");
        assert_eq!(config["availability"][1]["topic"], "limen/nas-2/status");
        assert_eq!(config["availability"][2]["topic"], "limen/nas-2/disk_used/status");
        let [(button_topic, pressing), (result_topic, result)] =
            topics.discovery_payloads(button, true).try_into().unwrap();
        assert_eq!(button_topic, "homeassistant/button/limen/nas-2-restart/config");
        assert_eq!(pressing["command_topic"], "limen/nas-2/restart/press");
        assert_eq!(result_topic, "homeassistant/sensor/limen/nas-2-restart/config");
        assert_eq!(
            (result["state_topic"].as_str(), result["unique_id"].as_str()),
            (Some("limen/nas-2/restart/result"), Some("limen-nas-2-restart-result"))
        );
        let plain = Topics { prefix: "limen".into(), discovery: None };
        assert!(plain.discovery_payloads(sensor, false).is_empty());
        assert_eq!(plain.filters(), ["limen/#"]);
    }

    fn catalog(read_only: bool) -> Catalog {
        let spec = |name: &str| ScriptSpec {
            name: name.into(),
            description: String::new(),
            timeout_seconds: 30,
            params: vec![Param::new("path", ParamType::String, "where").pattern("/[a-z]*")],
            read_only,
        };
        Catalog { scripts: vec![spec("disk_used_percent"), spec("restart_container")], problems: vec![] }
    }

    #[test]
    fn refused_on_the_nodes_word() {
        let mqtt = mqtt();
        let (sensor, button) = (&mqtt.sensors[0], &mqtt.buttons[0]);
        let nodes = ["nas-2".to_string()];
        let none = Approval::default();
        let refused = |entity, is_button, catalog: Option<&Catalog>, approval: &Approval| {
            refusal(entity, is_button, &nodes, catalog, approval)
        };
        assert_eq!(refused(sensor, false, None, &none), None, "unknown until the node answers");
        assert_eq!(refusal(sensor, false, &[], None, &none).unwrap(), "no node nas-2 in [nodes]");
        assert_eq!(refused(sensor, false, Some(&catalog(true)), &none), None);
        assert!(refused(sensor, false, Some(&catalog(false)), &none).unwrap().contains("is not read_only"));
        let other = Catalog::default();
        assert_eq!(refused(sensor, false, Some(&other), &none).unwrap(), "nas-2 offers no script disk_used_percent");
        assert!(refused(button, true, Some(&catalog(false)), &none).unwrap().starts_with("args: "), "name undeclared");
        let changes = Approval { scripts: ApprovalScripts::Changes, ..Approval::default() };
        let mut no_args = button.clone();
        no_args.args.clear();
        assert!(refused(&no_args, true, Some(&catalog(false)), &changes).unwrap().contains("nobody to ask"));
        assert_eq!(refused(&no_args, true, Some(&catalog(false)), &none), None);
    }

    #[test]
    fn states_results_and_who_answered() {
        let ran = |exit: i64, stdout: &str| {
            NodeResponse::success(json!({"script": "s", "exit": exit, "stdout": stdout, "stderr": ""}), false)
        };
        assert_eq!(sensor_state(&ran(0, " 42\n")).unwrap(), "42");
        assert_eq!(sensor_state(&ran(1, "42")).unwrap_err(), "exit 1");
        assert!(sensor_state(&ran(0, &"x".repeat(MAX_PAYLOAD + 1))).is_err());
        let down = NodeResponse::failure(&error(ErrorCode::Unreachable, "nas: no route"));
        assert_eq!(sensor_state(&down).unwrap_err(), "unreachable: nas: no route");
        let old = NodeResponse::failure(&error(ErrorCode::BadRequest, "unknown argument 'read_only'"));
        assert!(sensor_state(&old).unwrap_err().ends_with("update limen there"));
        assert!(!answered(&down));
        assert!(answered(&NodeResponse::failure(&error(ErrorCode::NotFound, "no script"))));
        assert!(answered(&NodeResponse::failure(&error(ErrorCode::Timeout, "slow did not finish in 1s"))));
        let cut = NodeResponse { truncated: true, ..ran(0, "42") };
        assert!(sensor_state(&cut).is_err(), "a state cut short is not the state");
        assert_eq!(result_payload(&ran(0, "done")), serde_json::to_string(&ran(0, "done")).unwrap());
        let long: Value = serde_json::from_str(&result_payload(&ran(3, &"x".repeat(MAX_PAYLOAD)))).unwrap();
        assert_eq!((long["data"]["exit"].as_i64(), long["truncated"].as_bool()), (Some(3), Some(true)));
        assert!(long["data"].get("stdout").is_none());
    }
}
