//! `[mqtt]` of the hub's `limen.toml` (spec §7.2, §9.1): the broker, and the sensors and buttons Home Assistant gets.

use super::hub::is;
use super::{ConfigResult, fail};
use crate::durations;
use crate::scripts;
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::time::Duration;

/// The hub's prefix: without `-`, so `<prefix>-<node>-<id>` reads one way only across hubs.
pub const PREFIX: &str = "^[a-z0-9][a-z0-9_]{0,31}$";
/// Home Assistant's discovery prefix.
pub const LEVEL: &str = "^[a-z0-9][a-z0-9_-]{0,31}$";
/// A sensor's or button's `id`. Without `-`, so `<node>-<id>` in a discovery topic reads one way only.
pub const ENTITY_ID: &str = "^[a-z0-9][a-z0-9_]{0,31}$";
const URL: &str = r"^mqtt://(\d{1,3}(\.\d{1,3}){3}|\[[0-9a-fA-F:]+\])(:\d{1,5})?$";
const DEFAULT_PORT: u16 = 1883;
const EVERY: Duration = Duration::from_secs(60);
const EVERY_RANGE: RangeInclusive<Duration> = Duration::from_secs(10)..=Duration::from_secs(86_400);
/// What a button's `discovery` may set: Home Assistant refuses a button's whole payload with a sensor's keys in it.
const BUTTON_KEYS: [&str; 6] =
    ["name", "icon", "entity_picture", "device_class", "entity_category", "enabled_by_default"];
/// The longest user and password MQTT carries: a length of two bytes.
const MAX_CREDENTIAL: usize = u16::MAX as usize;
/// What a sensor's `discovery` may set: how an entity looks, never a topic, an id or its device.
const SENSOR_KEYS: [&str; 12] = [
    "name",
    "icon",
    "entity_picture",
    "device_class",
    "state_class",
    "unit_of_measurement",
    "suggested_display_precision",
    "value_template",
    "entity_category",
    "enabled_by_default",
    "expire_after",
    "force_update",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mqtt {
    pub enabled: bool,
    /// The broker as `host:port`.
    pub address: String,
    pub user: Option<String>,
    /// Relative to the hub's directory unless absolute.
    pub password_file: Option<String>,
    pub prefix: String,
    /// Home Assistant's discovery prefix; none for plain topics only.
    pub discovery: Option<String>,
    pub sensors: Vec<Entity>,
    pub buttons: Vec<Entity>,
}

/// A sensor or a button: one script of one node, with fixed arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entity {
    pub node: String,
    pub id: String,
    pub script: String,
    pub args: Map<String, Value>,
    /// How often a sensor reads; zero for a button.
    pub every: Duration,
    pub discovery: Map<String, Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MqttFile {
    enabled: Option<bool>,
    url: Option<String>,
    user: Option<String>,
    password_file: Option<String>,
    prefix: Option<String>,
    discovery: Option<String>,
    #[serde(default)]
    sensor: Vec<EntityFile>,
    #[serde(default)]
    button: Vec<EntityFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntityFile {
    node: String,
    id: String,
    script: String,
    #[serde(default)]
    args: Map<String, Value>,
    every: Option<String>,
    #[serde(default)]
    discovery: Map<String, Value>,
}

impl Mqtt {
    /// The sensors, then the buttons, each with whether it is a button.
    pub fn entities(&self) -> impl Iterator<Item = (&Entity, bool)> {
        self.sensors.iter().map(|sensor| (sensor, false)).chain(self.buttons.iter().map(|button| (button, true)))
    }

    pub(super) fn parse(file: MqttFile) -> ConfigResult<Mqtt> {
        let url = file.url.unwrap_or_default();
        if !is(URL, &url) {
            return fail("mqtt.url", "expected mqtt://<address>[:<port>], an address and not a name");
        }
        let address = url.trim_start_matches("mqtt://");
        let has_port = address.rsplit_once(':').is_some_and(|(_, port)| !port.ends_with(']'));
        let address = if has_port { address.to_string() } else { format!("{address}:{DEFAULT_PORT}") };
        if address.parse::<std::net::SocketAddr>().is_err() {
            return fail("mqtt.url", format!("{address} is not an address and a port"));
        }
        let prefix = file.prefix.unwrap_or_else(|| "limen".into());
        if !is(PREFIX, &prefix) {
            return fail("mqtt.prefix", format!("must match {PREFIX}"));
        }
        let discovery = match file.discovery {
            None => Some("homeassistant".to_string()),
            Some(discovery) if discovery.is_empty() => None,
            Some(discovery) if is(LEVEL, &discovery) => Some(discovery),
            Some(_) => return fail("mqtt.discovery", format!("\"\" or a prefix matching {LEVEL}")),
        };
        match file.user.as_deref() {
            Some(user) if user.is_empty() || user.len() > MAX_CREDENTIAL => {
                return fail("mqtt.user", "leave it out for a broker without users");
            }
            None if file.password_file.is_some() => {
                return fail("mqtt.password_file", "a password goes with a user: set mqtt.user");
            }
            _ => {}
        }
        let sensors = file.sensor.into_iter().map(|sensor| entity("mqtt.sensor", sensor, false));
        let buttons = file.button.into_iter().map(|button| entity("mqtt.button", button, true));
        let mqtt = Mqtt {
            enabled: file.enabled.unwrap_or(false),
            address,
            user: file.user,
            password_file: file.password_file,
            prefix,
            discovery,
            sensors: sensors.collect::<ConfigResult<_>>()?,
            buttons: buttons.collect::<ConfigResult<_>>()?,
        };
        let mut seen = BTreeSet::new();
        for (entity, _) in mqtt.entities() {
            if !seen.insert((&entity.node, &entity.id)) {
                return fail("mqtt", format!("{}/{}: an id is used once within its node", entity.node, entity.id));
            }
        }
        Ok(mqtt)
    }
}

fn entity(key: &str, file: EntityFile, button: bool) -> ConfigResult<Entity> {
    let key = format!("{key} {}/{}", file.node, file.id);
    if !is(super::hub::NODE_NAME, &file.node) {
        return fail(&key, "node is not a node's name");
    }
    if !is(ENTITY_ID, &file.id) {
        return fail(&key, format!("id must match {ENTITY_ID}"));
    }
    if !scripts::is_script_name(&file.script) {
        return fail(&key, "script is not a script's name");
    }
    let keys: &[&str] = if button { &BUTTON_KEYS } else { &SENSOR_KEYS };
    if let Some(bad) = file.discovery.iter().find_map(|(name, value)| {
        let scalar = matches!(value, Value::String(_) | Value::Number(_) | Value::Bool(_));
        (!keys.contains(&name.as_str()) || !scalar).then_some(name)
    }) {
        return fail(&key, format!("discovery.{bad}: one of {} and a string, number or boolean", keys.join(", ")));
    }
    let every = match (button, file.every) {
        (true, Some(_)) => return fail(&key, "a button has no every: it runs when pressed"),
        (true, None) => Duration::ZERO,
        (false, None) => EVERY,
        (false, Some(text)) => match durations::parse(&text) {
            Some(every) if EVERY_RANGE.contains(&every) => every,
            _ => return fail(&key, "every: 10s to 24h"),
        },
    };
    Ok(Entity { node: file.node, id: file.id, script: file.script, args: file.args, every, discovery: file.discovery })
}

#[cfg(test)]
mod tests {
    use crate::config::hub::HubConfig;
    use serde_json::json;
    use std::time::Duration;

    const BROKER: &str = "[mqtt]\nurl = \"mqtt://192.168.1.229\"\n";

    fn refused(text: &str) -> String {
        HubConfig::parse(&format!("{BROKER}{text}")).unwrap_err().0
    }

    #[test]
    fn off_and_absent_unless_configured() {
        assert_eq!(HubConfig::parse("").unwrap().mqtt, None);
        let mqtt = HubConfig::parse(BROKER).unwrap().mqtt.unwrap();
        assert!(!mqtt.enabled, "a broker written down is not yet a broker used");
        assert_eq!(mqtt.address, "192.168.1.229:1883");
        assert_eq!((mqtt.prefix.as_str(), mqtt.discovery.as_deref()), ("limen", Some("homeassistant")));
    }

    #[test]
    fn sensors_and_buttons_as_declared() {
        let config = HubConfig::parse(
            r#"[mqtt]
enabled = true
url = "mqtt://[fd00::2]:1884"
prefix = "home"
discovery = ""

[[mqtt.sensor]]
node = "nas"
id = "disk_used"
script = "disk_used_percent"
args = { path = "/srv" }
every = "30s"
discovery = { unit_of_measurement = "%", suggested_display_precision = 0 }

[[mqtt.button]]
node = "nas"
id = "restart_immich"
script = "restart_container"
args = { name = "immich_server" }
"#,
        )
        .unwrap();
        let mqtt = config.mqtt.unwrap();
        assert!(mqtt.enabled);
        assert_eq!(mqtt.address, "[fd00::2]:1884");
        assert_eq!(mqtt.discovery, None);
        let sensor = &mqtt.sensors[0];
        assert_eq!(sensor.every, Duration::from_secs(30));
        assert_eq!(sensor.args["path"], json!("/srv"));
        assert_eq!(sensor.discovery["unit_of_measurement"], json!("%"));
        assert_eq!(mqtt.buttons[0].every, Duration::ZERO);
        assert_eq!(
            mqtt.entities().map(|(entity, button)| (entity.id.as_str(), button)).collect::<Vec<_>>(),
            [("disk_used", false), ("restart_immich", true)]
        );
    }

    #[test]
    fn what_the_file_says_alone_is_checked_as_it_is_read() {
        assert!(HubConfig::parse("[mqtt]\nurl = \"mqtt://broker.lan\"").unwrap_err().0.starts_with("mqtt.url: "));
        assert!(HubConfig::parse("[mqtt]\nurl = \"mqtts://10.0.0.1\"").is_err(), "no TLS in this version");
        for not_an_address in ["mqtt://999.1.1.1", "mqtt://10.0.0.1:99999"] {
            let error = HubConfig::parse(&format!("[mqtt]\nurl = \"{not_an_address}\"")).unwrap_err();
            assert!(error.0.ends_with("is not an address and a port"), "{error}");
        }
        assert!(refused("prefix = \"a/b\"").starts_with("mqtt.prefix: "));
        assert!(refused("prefix = \"a-b\"").starts_with("mqtt.prefix: "), "a-b + c and a + b-c would meet");
        assert!(refused("password_file = \"p\"").starts_with("mqtt.password_file: "));
        assert!(refused("discovery = \"#\"").starts_with("mqtt.discovery: "));
        assert!(refused("colour = 1").contains("unknown field"));
        let sensor =
            |line: &str| refused(&format!("[[mqtt.sensor]]\nnode = \"nas\"\nid = \"disk\"\nscript = \"df\"\n{line}"));
        assert_eq!(sensor("every = \"5s\""), "mqtt.sensor nas/disk: every: 10s to 24h");
        assert!(
            sensor("discovery = { state_topic = \"elsewhere\" }")
                .starts_with("mqtt.sensor nas/disk: discovery.state_topic: ")
        );
        assert!(sensor("discovery = { name = [\"a\"] }").starts_with("mqtt.sensor nas/disk: discovery.name: "));
        assert!(sensor("grep = \"x\"").contains("unknown field"));
        let button = "[[mqtt.button]]\nnode = \"nas\"\nscript = \"df\"\n";
        assert_eq!(
            refused(&format!("{button}id = \"a-b\"")),
            "mqtt.button nas/a-b: id must match ^[a-z0-9][a-z0-9_]{0,31}$"
        );
        assert_eq!(
            refused(&format!("{button}id = \"go\"\nevery = \"1m\"")),
            "mqtt.button nas/go: a button has no every: it runs when pressed"
        );
        assert_eq!(
            refused(&format!("{button}id = \"go\"\n{button}id = \"go\"")),
            "mqtt: nas/go: an id is used once within its node"
        );
        // Home Assistant drops a button's whole payload with a sensor's key in it.
        assert!(
            refused(&format!("{button}id = \"go\"\ndiscovery = {{ unit_of_measurement = \"%\" }}"))
                .starts_with("mqtt.button nas/go: discovery.unit_of_measurement: one of name, icon,")
        );
        assert!(
            HubConfig::parse(&format!("{BROKER}{button}id = \"go\"\ndiscovery = {{ icon = \"mdi:restart\" }}")).is_ok()
        );
    }

    #[test]
    fn a_node_left_out_of_nodes_doesnt_break_the_file() {
        // `limen forget` takes a node out of [nodes]; its sensors are refused as the hub runs, not here.
        let config =
            HubConfig::parse(&format!("{BROKER}[[mqtt.sensor]]\nnode = \"gone\"\nid = \"disk\"\nscript = \"df\"\n"));
        assert_eq!(config.unwrap().mqtt.unwrap().sensors[0].node, "gone");
    }
}
