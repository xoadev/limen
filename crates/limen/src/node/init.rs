//! The node's init system (spec §11): systemd, OpenWrt's procd, or none. Requests ask it for services, failures and
//! logs, and never need to know which one it is.

use super::procd::Procd;
use super::systemd::Systemd;
use super::{Answer, Node, system};
use crate::os::fs;
use limen_core::protocol::{ErrorCode, LimenError, Result, error};
use serde_json::{Map, Value};

type Args = Map<String, Value>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Init {
    Systemd,
    Procd,
    None,
}

impl Init {
    pub fn wire(self) -> &'static str {
        match self {
            Init::Systemd => "systemd",
            Init::Procd => "procd",
            Init::None => "none",
        }
    }

    /// What answers for this init system.
    pub fn system(self) -> &'static dyn InitSystem {
        match self {
            Init::Systemd => &Systemd,
            Init::Procd => &Procd,
            Init::None => &NoInit,
        }
    }
}

/// What differs between init systems: how services are listed, told about, found failed, and logged.
pub trait InitSystem: Sync {
    /// `services`: the units or services [args] asks for.
    fn units(&self, node: &Node, args: &Args) -> Result<Value>;
    /// `service`: one service by [name], and its last [lines] of log.
    fn service(&self, node: &Node, name: &str, lines: usize) -> Result<Value>;
    /// Services that should run and don't.
    fn failed(&self, node: &Node) -> Result<Vec<String>>;
    /// The last [lines] of the system's log, or of the service `filter.source` names.
    fn log(&self, node: &Node, lines: usize, filter: &LogFilter) -> Result<Answer>;
}

/// A node with neither systemd nor procd: services and logs are unavailable, said so.
struct NoInit;

impl InitSystem for NoInit {
    fn units(&self, _node: &Node, _args: &Args) -> Result<Value> {
        Err(no_init())
    }

    fn service(&self, _node: &Node, _name: &str, _lines: usize) -> Result<Value> {
        Err(no_init())
    }

    fn failed(&self, _node: &Node) -> Result<Vec<String>> {
        Err(no_init())
    }

    fn log(&self, _node: &Node, _lines: usize, _filter: &LogFilter) -> Result<Answer> {
        Err(error(ErrorCode::Unavailable, "no journal or logread on this node"))
    }
}

/// Which init system this node runs.
pub fn detect() -> Init {
    if fs::exists("/run/systemd/system") {
        Init::Systemd
    } else if system::openwrt() {
        Init::Procd
    } else {
        Init::None
    }
}

/// The answer to a request about services on a node with neither init system.
fn no_init() -> LimenError {
    error(ErrorCode::Unavailable, "no systemd or procd on this node")
}

/// What narrows a log: where a line comes from, how urgent it is, when it was written and what it says.
#[derive(Default, Clone, Copy)]
pub struct LogFilter<'a> {
    pub source: Option<&'a str>,
    pub priority: Option<&'a str>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub grep: Option<&'a str>,
}

impl LogFilter<'_> {
    pub fn narrows(&self) -> bool {
        self.source.is_some()
            || self.priority.is_some()
            || self.since.is_some()
            || self.until.is_some()
            || self.grep.is_some()
    }

    /// Whether a line written [at] falls between `since` and `until`; one whose time is unknown doesn't.
    pub fn spans(&self, at: Option<i64>) -> bool {
        self.since.is_none_or(|since| at.is_some_and(|at| at >= since))
            && self.until.is_none_or(|until| at.is_some_and(|at| at <= until))
    }
}
