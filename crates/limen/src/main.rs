//! The `limen` binary (spec §10): the hub (`mcp`, `serve`, `call`, …) and the node side (`gate`, `join`, `install`,
//! `apply`, …) in one file. Exit codes: 0 ok, 1 error, 2 usage; `check` exits with Nagios' codes.

mod hub;
mod node;
mod os;

use clap::{Args, Parser, Subcommand};
use hub::dir::{Hub, LiveHub};
use hub::{NodeClient, transports};
use limen_core::config::hub::{self as hub_config, is};
use limen_core::config::node::PATH as NODE_CONFIG;
use limen_core::durations;
use limen_core::join::{self, JoinUrl};
use limen_core::params::{self, Param, ParamType};
use limen_core::protocol::{LimenError, NodeRequest, bad_request, pretty};
use limen_core::requests::{self, Role};
use limen_core::scripts::{Catalog, ScriptKind};
use limen_core::version::{BUILD_DATE, BUILD_NUMBER, VERSION};
use node::installer::{self, Installer, RepoChoice, Setup};
use node::joiner::Joiner;
use node::{Answer, Node, deploy, gate, lint, read};
use os::sys;
use serde_json::{Map, Value};
use std::sync::Arc;
use std::time::Duration;

/// Nagios' UNKNOWN: a check that gave no status.
const NAGIOS_UNKNOWN: i32 = 3;
/// What the installer is fetched from, in the lines `invite` prints.
const INSTALL_SCRIPT: &str = "https://raw.githubusercontent.com/xoadev/limen/main/install.sh";
/// `ssh` exits with 255 when it fails itself, rather than the command it ran.
const SSH_FAILED: i32 = 255;
/// How long `call` waits for a deploy request.
const DEPLOY_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

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
    Join(JoinArgs),
    /// The SSH forced command on a node: one JSON request on stdin, the answer on stdout
    Gate {
        #[arg(long, value_parser = ["read", "deploy"])]
        role: String,
        /// Node configuration
        #[arg(long, default_value = NODE_CONFIG)]
        config: String,
    },
    /// Set this node up: binary, users, authorized_keys, sudoers, /etc/limen (as root)
    Install(InstallArgs),
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

#[derive(Args)]
struct JoinArgs {
    /// The join line of `limen invite`
    line: Option<String>,
    /// The hub's public key, when there is no join line
    #[arg(long)]
    hub_key: Option<String>,
    /// This machine's name on the hub, with --hub-key
    #[arg(long)]
    name: Option<String>,
    #[command(flatten)]
    setup: SetupArgs,
    /// Where the hub reaches this machine (default: where the join request comes from)
    #[arg(long)]
    address: Option<String>,
    /// This machine's SSH port, as the hub reaches it
    #[arg(long, default_value_t = 22)]
    ssh_port: u16,
}

#[derive(Args)]
struct InstallArgs {
    /// Public key of the hub (read role); without it there is no hub yet, and a join adds one later
    #[arg(long)]
    read_key: Option<String>,
    #[command(flatten)]
    setup: SetupArgs,
    /// Say what would change and change nothing
    #[arg(long)]
    dry_run: bool,
}

/// What `install` and `join` set up besides the hub's key.
#[derive(Args)]
struct SetupArgs {
    /// Public key of CI or a person (deploy role); without it, no deploy role
    #[arg(long)]
    deploy_key: Option<String>,
    /// Addresses or CIDRs the keys may connect from, e.g. 100.64.0.0/10 (not OpenWrt)
    #[arg(long)]
    from: Option<String>,
    /// Git repository with this machine's scripts, stacks and node.toml (https:// asks for a token)
    #[arg(long)]
    repo: Option<String>,
    /// Branch of --repo
    #[arg(long, default_value = "main")]
    branch: String,
    /// This machine's folder in --repo (default: nodes/<its name>)
    #[arg(long)]
    path: Option<String>,
}

impl SetupArgs {
    fn setup(&self) -> Setup {
        Setup {
            deploy_key: self.deploy_key.clone(),
            from: self.from.clone(),
            repo: self.repo.as_ref().map(|url| RepoChoice {
                url: url.clone(),
                branch: self.branch.clone(),
                path: self.path.clone(),
            }),
        }
    }
}

/// Why a command stopped: its usage (exit 2), limen's own error (1), or a message (1).
enum Stop {
    Usage(String),
    Limen(LimenError),
    Message(String),
}

impl From<LimenError> for Stop {
    fn from(error: LimenError) -> Self {
        Stop::Limen(error)
    }
}

impl Stop {
    /// Says why on stderr, and gives the exit code.
    fn report(self) -> i32 {
        let (text, code) = match self {
            Stop::Usage(message) => (message, 2),
            Stop::Limen(error) => (error.summary(), 1),
            Stop::Message(message) => (message, 1),
        };
        sys::log(&text);
        code
    }

    fn into_limen_error(self) -> LimenError {
        match self {
            Stop::Usage(message) | Stop::Message(message) => bad_request(message),
            Stop::Limen(error) => error,
        }
    }
}

type Exit = Result<i32, Stop>;

/// How `join`, `install`, `uninstall` and `token` stop: their own message, after the command's name.
fn failed(command: &'static str) -> impl FnOnce(String) -> Stop {
    move |message| Stop::Message(format!("{command}: {message}"))
}

fn exit_code(succeeded: bool) -> i32 {
    i32::from(!succeeded)
}

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

fn version() -> Exit {
    sys::say(&version_line());
    Ok(0)
}

fn main() {
    let asks_version = matches!(std::env::args().nth(1).as_deref(), Some("--version" | "-V"));
    let exit = if asks_version { version() } else { run(parse_command()) };
    std::process::exit(exit.unwrap_or_else(Stop::report));
}

/// The command on the command line; on a mistake, clap's message and exit code.
fn parse_command() -> Command {
    match Cli::try_parse() {
        Ok(cli) => cli.command,
        Err(error) => {
            error.print().ok();
            std::process::exit(error.exit_code());
        }
    }
}

fn run(command: Command) -> Exit {
    match command {
        Command::Init { serve, home } => init(serve, home.as_deref()),
        Command::Mcp { home } => mcp(home.as_deref()),
        Command::Serve { home, listen } => serve(home.as_deref(), listen),
        Command::Connect { home, url } => connect(home.as_deref(), url),
        Command::Invite { name, home, ttl } => invite(&name, home.as_deref(), &ttl),
        Command::Trust { name, address, host_key, user, port, home } => {
            trust(&name, &address, &host_key, &user, port, home.as_deref())
        }
        Command::Forget { name, home } => forget(&name, home.as_deref()),
        Command::Call { node, request, args, home, user, identity } => {
            call(&node, &request, &args, home.as_deref(), user.as_deref(), identity.as_deref())
        }
        Command::Join(args) => join_hub(&args),
        Command::Gate { role, config } => Ok(gate::run(Role::parse(&role).expect("clap checked it"), &config)),
        Command::Install(args) => install(&args),
        Command::Uninstall { purge, dry_run } => Installer::new(dry_run).uninstall(purge).map_err(failed("uninstall")),
        Command::Token => Installer::new(false).token().map_err(failed("token")),
        Command::Sync { config } => Ok(exit_code(deploy::sync(&Node::load(&config)?))),
        Command::Apply { from, no_sync, dry_run, config } => apply(from.as_deref(), !no_sync, dry_run, &config),
        Command::Action { name, args, config } => action(&name, &args, &config),
        Command::Check { name, args, config } => check(&name, &args, &config),
        Command::Lint { config } => Ok(lint::run(&Node::load(&config)?)),
        Command::Version => version(),
    }
}

fn init(serve: bool, home: Option<&str>) -> Exit {
    let hub = Hub::at(home);
    let created = hub.init(serve)?;
    for path in &created {
        sys::say(&format!("created {path}"));
    }
    if created.is_empty() {
        sys::say(&format!("{} was already a hub", hub.home));
    }
    sys::say(&format!("hub key: {}", join::fingerprint(&hub.public_key()?)?));
    sys::out("Next: `limen invite <name>` for each machine, and `limen connect` for the MCP client.\n");
    Ok(0)
}

fn mcp(home: Option<&str>) -> Exit {
    let hub = Arc::new(Hub::at(home));
    hub.config()?;
    transports::stdio(Arc::new(LiveHub::new(hub)));
    Ok(0)
}

fn serve(home: Option<&str>, listen: Option<String>) -> Exit {
    let hub = Arc::new(Hub::at(home));
    for path in hub.init(true)? {
        sys::log(&format!("created {path}"));
    }
    let token = hub.token()?;
    let live = Arc::new(LiveHub::new(hub.clone()));
    let address = listen_address(listen, &live)?;
    transports::http(hub, live, &address, token)?;
    Ok(0)
}

/// `--listen`, else `LIMEN_LISTEN` when it says something, else `[http].listen`.
fn listen_address(listen: Option<String>, live: &LiveHub) -> Result<String, LimenError> {
    match listen.or_else(|| sys::env_setting("LIMEN_LISTEN")) {
        Some(address) => Ok(address),
        None => live.config().map(|config| config.listen),
    }
}

/// Prints the command that adds this hub to Claude Code: over HTTP when the hub has an address and a token, else
/// over stdio.
fn connect(home: Option<&str>, url: Option<String>) -> Exit {
    let hub = Hub::at(home);
    let base = url.or_else(|| hub.config().ok().and_then(|config| config.public_url));
    let command = match (base, hub.token().ok()) {
        (Some(base), Some(token)) => {
            format!("claude mcp add --transport http limen {base}/mcp --header \"Authorization: Bearer {token}\"")
        }
        _ => format!("claude mcp add limen -- limen mcp{}", home_option(&hub)),
    };
    sys::say(&command);
    Ok(0)
}

/// ` --home <dir>` when the hub is not where `limen mcp` looks by default.
fn home_option(hub: &Hub) -> String {
    if hub.home == Hub::home(None).trim_end_matches('/') { String::new() } else { format!(" --home {}", hub.home) }
}

fn invite(name: &str, home: Option<&str>, ttl: &str) -> Exit {
    let hub = Hub::at(home);
    if !is(hub_config::NODE_NAME, name) {
        return Err(Stop::Usage(format!("a node name matches {}", hub_config::NODE_NAME)));
    }
    let Some(public_url) = hub.config()?.public_url else {
        // No HTTP hub to call back: the line carries the key, and the machine prints what to trust here.
        let key = join::without_comment(&hub.public_key()?);
        print_install_lines(name, &format!("--hub-key '{key}' --name {name}"));
        sys::out("It ends printing a `limen trust` line to run here.\n");
        return Ok(0);
    };
    let valid_for = durations::parse(ttl).ok_or_else(|| Stop::Usage("--ttl takes a duration like 30m or 2h".into()))?;
    let issued = hub.invite(name, valid_for)?;
    let line = JoinUrl {
        base: public_url,
        code: issued.code,
        fingerprint: join::fingerprint(&hub.public_key()?)?,
        secret: issued.secret,
    };
    print_install_lines(name, &format!("--join '{line}'"));
    sys::say(&format!("Valid once, for {ttl}."));
    Ok(0)
}

/// How to run the installer on [name] with [options]: with curl and sudo, and on OpenWrt.
fn print_install_lines(name: &str, options: &str) {
    sys::say(&format!("On {name}, as root:\n  curl -fsSL {INSTALL_SCRIPT} | sudo sh -s -- {options}"));
    sys::say(&format!("OpenWrt:\n  wget -qO- {INSTALL_SCRIPT} | sh -s -- {options}"));
}

fn trust(name: &str, address: &str, host_key: &str, user: &str, port: u16, home: Option<&str>) -> Exit {
    Hub::at(home).trust(name, address, host_key, user, port)?;
    sys::say(&format!("{name} added; try it with `limen call {name} hello`"));
    Ok(0)
}

fn forget(name: &str, home: Option<&str>) -> Exit {
    if !Hub::at(home).remove(name)? {
        return Err(Stop::Usage(format!("no node named '{name}'")));
    }
    sys::say(&format!("{name} removed from the hub"));
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
    let (name, args) = request_and_args(&live, node, request, pairs)?;
    if requests::find(name).map(|definition| definition.role) == Some(Role::Deploy) {
        let user = user.map_or_else(|| installer::user_of(Role::Deploy), String::from);
        return call_deploy(&live, node, name, args, &user, identity);
    }
    if user.is_some() || identity.is_some() {
        return Err(Stop::Usage("--user and --identity are only for deploy requests".into()));
    }
    let response = live.call(node, name, &args, None);
    sys::say(&pretty(&response));
    Ok(exit_code(response.ok))
}

/// The request `call` sends for [request], and its arguments: `check_<name>` is `check` with that script, and
/// `action` takes its script's name from `--arg name=…`.
fn request_and_args(
    live: &LiveHub,
    node: &str,
    request: &str,
    pairs: &[String],
) -> Result<(&'static str, Map<String, Value>), Stop> {
    if let Some(check) = request.strip_prefix("check_") {
        let params = script_params(live, node, ScriptKind::Check, check);
        return Ok(("check", script_args(check, parse_args(pairs, &params)?)));
    }
    if request == "action" {
        let action = pairs
            .iter()
            .find_map(|pair| pair.strip_prefix("name="))
            .ok_or_else(|| Stop::Usage("action needs --arg name=<action>".into()))?;
        let params = script_params(live, node, ScriptKind::Action, action);
        let own: Vec<String> = pairs.iter().filter(|pair| !pair.starts_with("name=")).cloned().collect();
        return Ok(("action", script_args(action, parse_args(&own, &params)?)));
    }
    let definition = requests::find(request).ok_or_else(|| Stop::Usage(format!("unknown request '{request}'")))?;
    Ok((definition.name, parse_args(pairs, &definition.params)?))
}

/// The parameters [name]'s header declares, from the node's catalog, so a script's arguments are typed as it says:
/// `--arg tag=20` stays a string. Empty when the node doesn't tell.
fn script_params(live: &LiveHub, node: &str, kind: ScriptKind, name: &str) -> Vec<Param> {
    let hello = live.call(node, "hello", &Map::new(), None);
    let catalog: Option<Catalog> =
        hello.data.and_then(|data| serde_json::from_value(data.get("catalog")?.clone()).ok());
    let scripts = catalog.map(|catalog| match kind {
        ScriptKind::Check => catalog.checks,
        ScriptKind::Action => catalog.actions,
        ScriptKind::Setup => catalog.setup,
    });
    scripts
        .and_then(|scripts| scripts.into_iter().find(|script| script.name == name))
        .map(|script| script.params)
        .unwrap_or_default()
}

/// A deploy request streams its scripts' output as it comes, and exits with their result.
fn call_deploy(
    live: &LiveHub,
    node: &str,
    name: &str,
    args: Map<String, Value>,
    user: &str,
    identity: Option<&str>,
) -> Exit {
    let line = NodeRequest::new(name, args).to_line();
    let mut print = |fd: i32, bytes: &[u8]| {
        if fd == 1 { sys::out_bytes(bytes) } else { sys::err(&String::from_utf8_lossy(bytes)) }
    };
    let finished = live.ssh()?.stream(node, user, identity, &line, DEPLOY_TIMEOUT, &mut print)?;
    if finished.exit_code == SSH_FAILED {
        sys::log(&format!("cannot reach {node}"));
    }
    Ok(finished.exit_code)
}

fn join_hub(args: &JoinArgs) -> Exit {
    match (&args.line, &args.hub_key) {
        (Some(line), _) => join_with_line(line, args),
        (None, Some(hub_key)) => join_with_key(hub_key, args),
        (None, None) => Err(Stop::Usage("give the join line of `limen invite`, or --hub-key and --name".into())),
    }
}

/// Joins through the hub's HTTP server, which gives its key and this machine's name.
fn join_with_line(line: &str, args: &JoinArgs) -> Exit {
    let url = JoinUrl::parse(line)?;
    Joiner { installer: Installer::new(false) }
        .join(&url, &args.setup.setup(), args.address.as_deref(), args.ssh_port)
        .map_err(failed("join"))
}

/// Joins with the hub's key given by hand: the machine prints the `limen trust` line to run on the hub.
fn join_with_key(hub_key: &str, args: &JoinArgs) -> Exit {
    let name = args.name.as_deref().ok_or_else(|| Stop::Usage("--hub-key needs --name".into()))?;
    Joiner { installer: Installer::new(false) }
        .with_key(hub_key, name, &args.setup.setup(), args.ssh_port)
        .map_err(failed("join"))
}

fn install(args: &InstallArgs) -> Exit {
    let node = sys::hostname().to_lowercase();
    Installer::new(args.dry_run).install(args.read_key.as_deref(), &args.setup.setup(), &node, true).map_err(failed("install"))
}

fn apply(from: Option<&str>, sync_first: bool, dry_run: bool, config: &str) -> Exit {
    if from.is_some_and(|prefix| !is("^[0-9]{1,4}$", prefix)) {
        return Err(Stop::Usage("--from takes the number prefix, e.g. 20".into()));
    }
    sys::chdir_root();
    Ok(exit_code(deploy::apply(&Node::load(config)?, from, dry_run, sync_first)))
}

fn action(name: &str, pairs: &[String], config: &str) -> Exit {
    sys::chdir_root();
    let node = Node::load(config)?;
    let (_, spec) = node::scripts::find(&node, ScriptKind::Action, name)?;
    Ok(exit_code(deploy::action(&node, name, &parse_args(pairs, &spec.params)?)?))
}

/// Runs a check here and exits with Nagios' code for its status.
fn check(name: &str, pairs: &[String], config: &str) -> Exit {
    sys::chdir_root();
    let node = Node::load(config)?;
    match run_check(&node, name, pairs) {
        Ok(answer) => {
            sys::say(&pretty(&answer.data));
            Ok(nagios_code(answer.data.get("status").and_then(Value::as_str)))
        }
        Err(error) => {
            // Not found, a timeout, bad arguments: no answer from the check is Nagios' UNKNOWN, not a warning.
            sys::log(&format!("check: {}", error.summary()));
            Ok(NAGIOS_UNKNOWN)
        }
    }
}

fn run_check(node: &Node, name: &str, pairs: &[String]) -> Result<Answer, LimenError> {
    let (_, spec) = node::scripts::find(node, ScriptKind::Check, name)?;
    let args = parse_args(pairs, &spec.params).map_err(Stop::into_limen_error)?;
    let body = script_args(name, args);
    read::check(node, &params::validate(&requests::named("check").params, &body)?)
}

fn nagios_code(status: Option<&str>) -> i32 {
    match status {
        Some("ok") => 0,
        Some("warn") => 1,
        Some("fail") => 2,
        _ => NAGIOS_UNKNOWN,
    }
}

/// The arguments of `check` and `action`: the script's name and its own arguments.
fn script_args(name: &str, args: Map<String, Value>) -> Map<String, Value> {
    let mut body = Map::new();
    body.insert("name".into(), name.into());
    body.insert("args".into(), Value::Object(args));
    body
}

/// `--arg key=value`, typed by [params] where it names one; otherwise a number, a boolean or a string.
fn parse_args(pairs: &[String], params: &[Param]) -> Result<Map<String, Value>, Stop> {
    let mut args = Map::new();
    for pair in pairs {
        let Some((key, value)) = pair.split_once('=').filter(|(key, _)| !key.is_empty()) else {
            return Err(Stop::Usage(format!("--arg takes key=value, not '{pair}'")));
        };
        let kind = params.iter().find(|param| param.name == key).map(|param| param.kind);
        args.insert(key.into(), typed_value(key, value, kind)?);
    }
    Ok(args)
}

fn typed_value(key: &str, value: &str, kind: Option<ParamType>) -> Result<Value, Stop> {
    match kind {
        Some(ParamType::Int) => {
            value.parse::<i64>().map(Value::from).map_err(|_| Stop::Usage(format!("{key} must be an integer")))
        }
        Some(ParamType::Bool) => {
            value.parse::<bool>().map(Value::from).map_err(|_| Stop::Usage(format!("{key} must be true or false")))
        }
        Some(_) => Ok(Value::from(value)),
        None => Ok(guessed_value(value)),
    }
}

/// A value no parameter declares: a number, a boolean, or else a string.
fn guessed_value(value: &str) -> Value {
    if let Ok(number) = value.parse::<i64>() {
        Value::from(number)
    } else if let Ok(flag) = value.parse::<bool>() {
        Value::from(flag)
    } else {
        Value::from(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
