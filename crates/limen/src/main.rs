//! The `limen` binary (spec §10): the hub (`mcp`, `serve`, `call`, …) and the node side (`gate`, `join`, `install`,
//! `apply`, …) in one file. Exit codes: 0 ok, 1 error, 2 usage; `check` exits with Nagios' codes.

#![forbid(unsafe_code)]

mod hub;
mod node;
mod os;

use clap::{Parser, Subcommand};
use hub::dir::{Hub, LiveHub};
use hub::{NodeClient, transports};
use limen_core::config::hub::{self as hub_config, is};
use limen_core::config::node::PATH as NODE_CONFIG;
use limen_core::durations;
use limen_core::join::{self, JoinUrl};
use limen_core::params::{self, Param, ParamType};
use limen_core::protocol::{LimenError, NodeRequest, pretty};
use limen_core::requests::{self, Role};
use limen_core::scripts::{Catalog, ScriptKind};
use limen_core::version::{BUILD_DATE, BUILD_NUMBER, VERSION};
use node::installer::{Installer, RepoOptions};
use node::joiner::Joiner;
use node::{Node, deploy, gate, lint, read};
use os::sys;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "limen",
    about = "Read-only access to Linux machines for MCP clients. The hub runs `mcp` or `serve`; each node runs `gate` \
             as an SSH forced command, set up by `join` (or `install`).",
    disable_version_flag = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create the hub: its key, limen.toml and, with --serve, the token of HTTP clients
    Init {
        /// Also the token of HTTP clients
        #[arg(long)]
        serve: bool,
        /// Hub directory (default: $LIMEN_HOME or ~/.limen)
        #[arg(long)]
        home: Option<String>,
    },
    /// MCP server over stdio (the hub)
    Mcp {
        /// Hub directory with limen.toml (default: $LIMEN_HOME or ~/.limen)
        #[arg(long)]
        home: Option<String>,
    },
    /// MCP server over HTTP (the hub), and where nodes join; creates the hub on first start
    Serve {
        /// Hub directory (default: $LIMEN_HOME or ~/.limen)
        #[arg(long)]
        home: Option<String>,
        /// host:port, over [http].listen and LIMEN_LISTEN
        #[arg(long)]
        listen: Option<String>,
    },
    /// Print the command that connects Claude Code to this hub
    Connect {
        /// Hub directory
        #[arg(long)]
        home: Option<String>,
        /// The hub's HTTP address, over [http].public_url and LIMEN_PUBLIC_URL
        #[arg(long)]
        url: Option<String>,
    },
    /// A one-time line that joins a machine to this hub; paste it on the machine as root
    Invite {
        /// The machine's name on the hub
        name: String,
        /// Hub directory
        #[arg(long)]
        home: Option<String>,
        /// How long the invitation lasts
        #[arg(long, default_value = "1h")]
        ttl: String,
    },
    /// Add a machine to this hub by hand: its name, address and host key
    Trust {
        name: String,
        address: String,
        host_key: String,
        /// root for OpenWrt
        #[arg(long, default_value = hub_config::READ_USER)]
        user: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        /// Hub directory
        #[arg(long)]
        home: Option<String>,
    },
    /// Remove a machine from this hub (uninstall on the machine is separate)
    Forget {
        name: String,
        /// Hub directory
        #[arg(long)]
        home: Option<String>,
    },
    /// One request to a node over SSH, printed as JSON. Checks as check_<name>; deploy requests (sync, apply, action)
    /// stream their output and take the deploy role's --user and --identity
    Call {
        node: String,
        request: String,
        /// key=value, repeatable
        #[arg(long = "arg")]
        args: Vec<String>,
        /// Hub directory with limen.toml
        #[arg(long)]
        home: Option<String>,
        /// SSH user for deploy requests (default: limen-deploy)
        #[arg(long)]
        user: Option<String>,
        /// SSH key for deploy requests (default: the hub's)
        #[arg(long)]
        identity: Option<String>,
    },
    /// Join this machine to a hub (as root): with the line of `limen invite`, or with --hub-key and --name when the
    /// hub is not reachable over HTTP
    Join {
        /// The join line of `limen invite`
        line: Option<String>,
        /// The hub's public key, when there is no join line
        #[arg(long)]
        hub_key: Option<String>,
        /// This machine's name on the hub, with --hub-key
        #[arg(long)]
        name: Option<String>,
        /// Public key for the deploy role
        #[arg(long)]
        deploy_key: Option<String>,
        /// Addresses or CIDRs the keys may connect from (not OpenWrt)
        #[arg(long)]
        from: Option<String>,
        /// Git repository this machine follows
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, default_value = "main")]
        branch: String,
        /// This machine's folder in --repo (default: nodes/<its name on the hub>)
        #[arg(long)]
        path: Option<String>,
        /// Where the hub reaches this machine (default: where the join request comes from)
        #[arg(long)]
        address: Option<String>,
        #[arg(long, default_value_t = 22)]
        ssh_port: u16,
    },
    /// The SSH forced command on a node: one JSON request on stdin, the answer on stdout
    Gate {
        #[arg(long, value_parser = ["read", "deploy"])]
        role: String,
        /// Node configuration
        #[arg(long, default_value = NODE_CONFIG)]
        config: String,
    },
    /// Set this node up: binary, users, authorized_keys, sudoers, /etc/limen (as root)
    Install {
        /// Public key of the hub (read role)
        #[arg(long)]
        read_key: String,
        /// Public key of CI or a person (deploy role); without it, no deploy role
        #[arg(long)]
        deploy_key: Option<String>,
        /// Addresses or CIDRs the keys may connect from, e.g. 100.64.0.0/10
        #[arg(long)]
        from: Option<String>,
        /// Git repository with this node's scripts, stacks and node.toml (https:// asks for a token)
        #[arg(long)]
        repo: Option<String>,
        /// Branch of --repo
        #[arg(long, default_value = "main")]
        branch: String,
        /// This node's folder in --repo (default: nodes/<hostname>)
        #[arg(long)]
        path: Option<String>,
        /// Say what would change and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Undo install (as root)
    Uninstall {
        /// Also remove /etc/limen and /var/log/limen
        #[arg(long)]
        purge: bool,
        /// Say what would change and change nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// Replace the token that reads this node's repository (as root)
    Token,
    /// Bring this node's copy of its repository to the remote branch
    Sync {
        #[arg(long, default_value = NODE_CONFIG)]
        config: String,
    },
    /// Sync the repository, run the setup scripts in order and bring up the stacks, on this node
    Apply {
        /// Start at the script with this number prefix
        #[arg(long)]
        from: Option<String>,
        /// Use the checkout as it is
        #[arg(long)]
        no_sync: bool,
        /// List what would run
        #[arg(long)]
        dry_run: bool,
        #[arg(long, default_value = NODE_CONFIG)]
        config: String,
    },
    /// Run one action script, on this node
    Action {
        name: String,
        /// key=value, repeatable
        #[arg(long = "arg")]
        args: Vec<String>,
        #[arg(long, default_value = NODE_CONFIG)]
        config: String,
    },
    /// Run one check script, on this node; exits 0, 1 or 2 by its status, and 3 when it gave none
    Check {
        name: String,
        /// key=value, repeatable
        #[arg(long = "arg")]
        args: Vec<String>,
        #[arg(long, default_value = NODE_CONFIG)]
        config: String,
    },
    /// Check script names, headers and permissions without running anything
    Lint {
        #[arg(long, default_value = NODE_CONFIG)]
        config: String,
    },
    /// Print the version
    Version,
}

/// Why a command stopped: its usage (exit 2), limen's own error (1), or a message (1).
enum Stop {
    Usage(String),
    Limen(LimenError),
    Message(String),
}

impl From<LimenError> for Stop {
    fn from(e: LimenError) -> Self {
        Stop::Limen(e)
    }
}

type Exit = Result<i32, Stop>;

/// `limen 0.3.1 · build 42 · 2026-09-26T08:00:00Z`, or `limen dev` for a local build.
fn version_line() -> String {
    let mut parts = vec![format!("limen {VERSION}")];
    if !BUILD_NUMBER.is_empty() {
        parts.push(format!("build {BUILD_NUMBER}"));
    }
    if !BUILD_DATE.is_empty() {
        parts.push(BUILD_DATE.into());
    }
    parts.join(" · ")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if matches!(args.get(1).map(String::as_str), Some("--version" | "-V")) {
        sys::out(&format!("{}\n", version_line()));
        return;
    }
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            e.print().ok();
            std::process::exit(e.exit_code());
        }
    };
    let code = match run(cli.command) {
        Ok(code) => code,
        Err(Stop::Usage(m)) => {
            sys::err(&format!("limen: {m}\n"));
            2
        }
        Err(Stop::Limen(e)) => {
            sys::err(&format!("limen: {}: {}\n", e.code.wire(), e.message));
            1
        }
        Err(Stop::Message(m)) => {
            sys::err(&format!("limen: {m}\n"));
            1
        }
    };
    std::process::exit(code);
}

fn run(command: Command) -> Exit {
    match command {
        Command::Init { serve, home } => {
            let hub = Hub::at(home.as_deref());
            let created = hub.init(serve)?;
            for c in &created {
                sys::out(&format!("created {c}\n"));
            }
            if created.is_empty() {
                sys::out(&format!("{} was already a hub\n", hub.home));
            }
            sys::out(&format!("hub key: {}\n", join::fingerprint(&hub.public_key()?)?));
            sys::out("Next: `limen invite <name>` for each machine, and `limen connect` for the MCP client.\n");
            Ok(0)
        }
        Command::Mcp { home } => {
            let hub = Arc::new(Hub::at(home.as_deref()));
            hub.config()?;
            transports::stdio(Arc::new(LiveHub::new(hub)));
            Ok(0)
        }
        Command::Serve { home, listen } => {
            let hub = Arc::new(Hub::at(home.as_deref()));
            for c in hub.init(true)? {
                sys::err(&format!("limen: created {c}\n"));
            }
            let token = hub.token()?;
            let live = Arc::new(LiveHub::new(hub.clone()));
            let address = listen
                .or_else(|| sys::env("LIMEN_LISTEN").filter(|l| !l.trim().is_empty()))
                .map_or_else(|| live.config().map(|c| c.listen), Ok)?;
            transports::http(hub, live, &address, token)?;
            Ok(0)
        }
        Command::Connect { home, url } => {
            let hub = Hub::at(home.as_deref());
            let base = url.or_else(|| hub.config().ok().and_then(|c| c.public_url));
            match (base, hub.token().ok()) {
                (Some(base), Some(token)) => {
                    sys::out(&format!(
                        "claude mcp add --transport http limen {base}/mcp --header \"Authorization: Bearer {token}\"\n"
                    ));
                }
                _ => {
                    let option = if hub.home == Hub::home(None).trim_end_matches('/') {
                        String::new()
                    } else {
                        format!(" --home {}", hub.home)
                    };
                    sys::out(&format!("claude mcp add limen -- limen mcp{option}\n"));
                }
            }
            Ok(0)
        }
        Command::Invite { name, home, ttl } => invite(&name, home.as_deref(), &ttl),
        Command::Trust { name, address, host_key, user, port, home } => {
            Hub::at(home.as_deref()).trust(&name, &address, &host_key, &user, port)?;
            sys::out(&format!("{name} added; try it with `limen call {name} hello`\n"));
            Ok(0)
        }
        Command::Forget { name, home } => {
            if !Hub::at(home.as_deref()).remove(&name)? {
                return Err(Stop::Usage(format!("no node named '{name}'")));
            }
            sys::out(&format!("{name} removed from the hub\n"));
            Ok(0)
        }
        Command::Call { node, request, args, home, user, identity } => {
            call(&node, &request, &args, home.as_deref(), user.as_deref(), identity.as_deref())
        }
        Command::Join { line, hub_key, name, deploy_key, from, repo, branch, path, address, ssh_port } => {
            let joiner = Joiner { installer: Installer::new(false) };
            let result = match (line, hub_key) {
                (Some(line), _) => {
                    let url = JoinUrl::parse(&line)?;
                    // The folder defaults to the name the invitation gives, known only once the hub answers.
                    let repo_for = repo.as_ref().map(|url| {
                        let (branch, path) = (branch.clone(), path.clone());
                        move |invited: &str| RepoOptions {
                            url: url.clone(),
                            branch: branch.clone(),
                            path: path.clone().unwrap_or(format!("nodes/{invited}")),
                        }
                    });
                    joiner.join(
                        &url,
                        deploy_key.as_deref(),
                        from.as_deref(),
                        repo_for.as_ref().map(|f| f as &dyn Fn(&str) -> RepoOptions),
                        address.as_deref(),
                        ssh_port,
                    )
                }
                (None, Some(key)) => {
                    let name = name.ok_or(Stop::Usage("--hub-key needs --name".into()))?;
                    let repo =
                        repo.map(|url| RepoOptions { url, branch, path: path.unwrap_or(format!("nodes/{name}")) });
                    joiner.with_key(&key, &name, deploy_key.as_deref(), from.as_deref(), repo.as_ref(), ssh_port)
                }
                (None, None) => {
                    return Err(Stop::Usage("give the join line of `limen invite`, or --hub-key and --name".into()));
                }
            };
            result.map_err(|e| Stop::Message(format!("join: {e}")))
        }
        Command::Gate { role, config } => Ok(gate::run(Role::parse(&role).expect("clap checked it"), &config)),
        Command::Install { read_key, deploy_key, from, repo, branch, path, dry_run } => {
            let repo = repo.map(|url| RepoOptions {
                url,
                branch,
                path: path.unwrap_or(format!("nodes/{}", sys::hostname().to_lowercase())),
            });
            Installer::new(dry_run)
                .install(&read_key, deploy_key.as_deref(), from.as_deref(), repo.as_ref(), true)
                .map_err(|e| Stop::Message(format!("install: {e}")))
        }
        Command::Uninstall { purge, dry_run } => {
            Installer::new(dry_run).uninstall(purge).map_err(|e| Stop::Message(format!("uninstall: {e}")))
        }
        Command::Token => Installer::new(false).token().map_err(|e| Stop::Message(format!("token: {e}"))),
        Command::Sync { config } => Ok(i32::from(!deploy::sync(&Node::load(&config)?))),
        Command::Apply { from, no_sync, dry_run, config } => {
            if from.as_deref().is_some_and(|f| !is("^[0-9]{1,4}$", f)) {
                return Err(Stop::Usage("--from takes the number prefix, e.g. 20".into()));
            }
            sys::chdir_root();
            Ok(i32::from(!deploy::apply(&Node::load(&config)?, from.as_deref(), dry_run, !no_sync)))
        }
        Command::Action { name, args, config } => {
            sys::chdir_root();
            let node = Node::load(&config)?;
            let (_, spec) = node::scripts::find(&node, ScriptKind::Action, &name)?;
            Ok(i32::from(!deploy::action(&node, &name, &parse_args(&args, &spec.params)?)?))
        }
        Command::Check { name, args, config } => check(&name, &args, &config),
        Command::Lint { config } => Ok(lint::run(&Node::load(&config)?)),
        Command::Version => {
            sys::out(&format!("{}\n", version_line()));
            Ok(0)
        }
    }
}

fn invite(name: &str, home: Option<&str>, ttl: &str) -> Exit {
    let hub = Hub::at(home);
    if !is(hub_config::NODE_NAME, name) {
        return Err(Stop::Usage(format!("a node name matches {}", hub_config::NODE_NAME)));
    }
    let script = "https://raw.githubusercontent.com/xoadev/limen/main/install.sh";
    let Some(public_url) = hub.config()?.public_url else {
        // No HTTP hub to call back: the line carries the key, and the machine prints what to trust here.
        let key = join::without_comment(&hub.public_key()?);
        sys::out(&format!(
            "On {name}, as root:\n  curl -fsSL {script} | sudo sh -s -- --hub-key '{key}' --name {name}\n"
        ));
        sys::out(&format!("OpenWrt:\n  wget -qO- {script} | sh -s -- --hub-key '{key}' --name {name}\n"));
        sys::out("It ends printing a `limen trust` line to run here.\n");
        return Ok(0);
    };
    let ttl_value = durations::parse(ttl).ok_or(Stop::Usage("--ttl takes a duration like 30m or 2h".into()))?;
    let issued = hub.invite(name, ttl_value)?;
    let line = JoinUrl {
        base: public_url,
        code: issued.code,
        fingerprint: join::fingerprint(&hub.public_key()?)?,
        secret: issued.secret,
    };
    sys::out(&format!("On {name}, as root:\n  curl -fsSL {script} | sudo sh -s -- --join '{line}'\n"));
    sys::out(&format!("OpenWrt:\n  wget -qO- {script} | sh -s -- --join '{line}'\n"));
    sys::out(&format!("Valid once, for {ttl}.\n"));
    Ok(0)
}

fn call(
    node: &str,
    request: &str,
    pairs: &[String],
    home: Option<&str>,
    user: Option<&str>,
    identity: Option<&str>,
) -> Exit {
    let live = LiveHub::new(Arc::new(Hub::at(home)));
    if live.config()?.node(node).is_none() {
        return Err(Stop::Usage(format!("no node named '{node}'")));
    }
    // A script's arguments are typed as its header says, from the node's catalog: `--arg tag=20` stays a string.
    let catalog = || -> Option<Catalog> {
        let hello = live.call(node, "hello", &Map::new(), None);
        serde_json::from_value(hello.data?.get("catalog")?.clone()).ok()
    };
    let (name, body) = if let Some(check) = request.strip_prefix("check_") {
        let params = catalog()
            .and_then(|c| c.checks.into_iter().find(|s| s.name == check))
            .map(|s| s.params)
            .unwrap_or_default();
        ("check", json_object(json!({"name": check, "args": parse_args(pairs, &params)?})))
    } else if request == "action" {
        let action = pairs
            .iter()
            .find_map(|p| p.strip_prefix("name="))
            .ok_or(Stop::Usage("action needs --arg name=<action>".into()))?;
        let params = catalog()
            .and_then(|c| c.actions.into_iter().find(|s| s.name == action))
            .map(|s| s.params)
            .unwrap_or_default();
        let own: Vec<String> = pairs.iter().filter(|p| !p.starts_with("name=")).cloned().collect();
        ("action", json_object(json!({"name": action, "args": parse_args(&own, &params)?})))
    } else {
        let def = requests::find(request).ok_or(Stop::Usage(format!("unknown request '{request}'")))?;
        (def.name, parse_args(pairs, &def.params)?)
    };
    let role = requests::find(name).map(|d| d.role);
    if role == Some(Role::Deploy) {
        let line = format!(
            "{}\n",
            serde_json::to_string(&NodeRequest { v: 1, request: name.into(), args: body })
                .expect("a request serializes")
        );
        let mut stream = |fd: i32, bytes: &[u8]| {
            if fd == 1 { sys::out_bytes(bytes) } else { sys::err(&String::from_utf8_lossy(bytes)) }
        };
        let r = live.ssh()?.stream(
            node,
            user.unwrap_or("limen-deploy"),
            identity,
            &line,
            Duration::from_secs(6 * 3600),
            &mut stream,
        )?;
        if r.exit_code == 255 {
            sys::err(&format!("limen: cannot reach {node}\n"));
        }
        return Ok(r.exit_code);
    }
    if user.is_some() || identity.is_some() {
        return Err(Stop::Usage("--user and --identity are only for deploy requests".into()));
    }
    let response = live.call(node, name, &body, None);
    sys::out(&format!("{}\n", pretty(&response)));
    Ok(i32::from(!response.ok))
}

fn check(name: &str, pairs: &[String], config: &str) -> Exit {
    sys::chdir_root();
    let node = Node::load(config)?;
    let answer = (|| {
        let (_, spec) = node::scripts::find(&node, ScriptKind::Check, name)?;
        let args = parse_args(pairs, &spec.params).map_err(|e| match e {
            Stop::Usage(m) | Stop::Message(m) => limen_core::protocol::bad_request(m),
            Stop::Limen(e) => e,
        })?;
        let body = json_object(json!({"name": name, "args": args}));
        read::check(&node, &params::validate(&requests::named("check").params, &body)?)
    })();
    match answer {
        Ok(a) => {
            sys::out(&format!("{}\n", pretty(&a.data)));
            let status = a.data.get("status").and_then(Value::as_str);
            Ok(["ok", "warn", "fail"].iter().position(|s| Some(*s) == status).map_or(3, |p| p as i32))
        }
        Err(e) => {
            // Not found, a timeout, bad arguments: no answer from the check is Nagios' UNKNOWN, not a warning.
            sys::err(&format!("limen: check: {}: {}\n", e.code.wire(), e.message));
            Ok(3)
        }
    }
}

fn json_object(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

/// `--arg key=value`, typed by [params] where it names one; otherwise a number, a boolean or a string.
fn parse_args(pairs: &[String], params: &[Param]) -> Result<Map<String, Value>, Stop> {
    let mut out = Map::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=').filter(|(k, _)| !k.is_empty()) else {
            return Err(Stop::Usage(format!("--arg takes key=value, not '{pair}'")));
        };
        let kind = params.iter().find(|p| p.name == key).map(|p| p.kind);
        let parsed = match kind {
            Some(ParamType::Int) => {
                json!(value.parse::<i64>().map_err(|_| Stop::Usage(format!("{key} must be an integer")))?)
            }
            Some(ParamType::Bool) => {
                json!(value.parse::<bool>().map_err(|_| Stop::Usage(format!("{key} must be true or false")))?)
            }
            None if value.parse::<i64>().is_ok() => json!(value.parse::<i64>().unwrap_or_default()),
            None if value == "true" || value == "false" => json!(value == "true"),
            _ => json!(value),
        };
        out.insert(key.into(), parsed);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_types_by_the_schema() {
        let params = [Param::new("name", ParamType::String, ""), Param::new("lines", ParamType::Int, "")];
        let pairs: Vec<String> = ["name=123", "lines=5", "all=true"].map(String::from).to_vec();
        let Ok(args) = parse_args(&pairs, &params) else { panic!() };
        assert_eq!(Value::Object(args), json!({"name": "123", "lines": 5, "all": true}));
    }

    #[test]
    fn the_cli_is_well_formed() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
