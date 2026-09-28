//! The node's copy of its repository (spec §6.1). `sync` makes it what the remote branch has —local edits and
//! untracked files discarded, ignored ones such as a stack's `.env` kept— because the repository is the source of
//! truth, not the machine.
//!
//! The token never goes in a URL or an argument, where `ps` and logs would show it: git gets it as an HTTP header
//! through `GIT_CONFIG_*` in its environment.

use super::Node;
use crate::os::{fs, proc};
use base64::Engine;
use limen_core::config::node::RepoConfig;
use limen_core::protocol::{ErrorCode, Result, error};
use limen_core::time::iso;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;
use std::time::Duration;

const CHECKOUT_TIMEOUT: Duration = Duration::from_secs(600);
const REMOTE_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Commit {
    pub hash: String,
    pub date: String,
    pub subject: String,
}

/// A commit as people read it: its first 12 characters.
pub fn short_hash(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

/// Brings the checkout to the remote branch: the commit before (if any) and after.
pub fn sync(node: &Node, repo: &RepoConfig) -> Result<(Option<String>, String)> {
    let before = deployed(repo).map(|commit| commit.hash);
    let token = token(repo);
    let after = checkout_remote_branch(&Git { repo, token: token.as_deref() });
    record(node, repo, before.as_deref(), &after);
    after.map(|after| (before, after))
}

pub fn deployed(repo: &RepoConfig) -> Option<Commit> {
    if !is_checkout(repo) {
        return None;
    }
    let log = Git { repo, token: None }.in_checkout(&["log", "-1", "--format=%H%n%cI%n%s"]).ok()?.out();
    let mut lines = log.lines();
    Some(Commit { hash: lines.next()?.into(), date: lines.next()?.into(), subject: lines.next()?.into() })
}

/// The commit the remote branch points at, asked with `ls-remote`: nothing on the node changes.
pub fn remote(repo: &RepoConfig) -> Result<String> {
    let token = token(repo);
    let heads = ls_remote(&Git { repo, token: token.as_deref() })?.out();
    heads
        .lines()
        .find(|line| !line.trim().is_empty())
        .and_then(|line| line.split('\t').next().map(String::from))
        .ok_or_else(|| error(ErrorCode::NotFound, format!("the remote has no branch {}", repo.branch)))
}

pub enum Access {
    Readable,
    /// The server wants credentials, or these ones are not enough: a token can fix it.
    NeedsToken,
    /// Anything else —network, TLS, a wrong URL—: a token would not help, and asking for one would mislead.
    Failed(String),
}

/// Whether [token] (or none) can read the repository: what `install` checks before saving it.
pub fn access(repo: &RepoConfig, token: Option<&str>) -> Access {
    match ls_remote(&Git { repo, token }) {
        Ok(_) => Access::Readable,
        Err(failure) if asks_for_credentials(&failure.message) => Access::NeedsToken,
        Err(failure) => Access::Failed(failure.message),
    }
}

pub fn last_sync(repo: &RepoConfig) -> Option<Value> {
    fs::read_text(&repo.sync_record())
        .and_then(|text| serde_json::from_str::<Map<String, Value>>(&text).ok())
        .map(Value::Object)
}

pub fn token(repo: &RepoConfig) -> Option<String> {
    fs::read_text(&repo.token_file).map(|text| text.trim().to_string()).filter(|token| !token.is_empty())
}

/// A clone when repo.dir holds none yet, an update otherwise; then the commit it is at.
fn checkout_remote_branch(git: &Git) -> Result<String> {
    if is_checkout(git.repo) {
        update_checkout(git)?;
    } else {
        clone_checkout(git)?;
    }
    deployed(git.repo).map(|commit| commit.hash).ok_or_else(|| error(ErrorCode::Internal, "git: no commit after sync"))
}

fn clone_checkout(git: &Git) -> Result<()> {
    let repo = git.repo;
    let parent = fs::parent(&repo.dir);
    fs::mkdirs(parent, 0o755).map_err(|message| error(ErrorCode::Internal, message))?;
    // Only an empty directory makes way for the clone: anything else at repo.dir is not limen's to remove.
    if fs::exists(&repo.dir) {
        std::fs::remove_dir(&repo.dir).map_err(|_| {
            error(
                ErrorCode::Internal,
                format!("{} is there and is not a checkout; move it away or set another repo.dir", repo.dir),
            )
        })?;
    }
    git.run(
        None,
        &["clone", "--quiet", "--depth", "1", "--single-branch", "--branch", &repo.branch, "--", &repo.url, &repo.dir],
        CHECKOUT_TIMEOUT,
    )?;
    Ok(())
}

fn update_checkout(git: &Git) -> Result<()> {
    let repo = git.repo;
    // The URL of limen.toml, not the one of the first clone: changing [repo].url moves the node.
    git.in_checkout(&["remote", "set-url", "origin", &repo.url])?;
    git.in_checkout(&["fetch", "--quiet", "--depth", "1", "origin", "--", &branch_ref(repo)])?;
    git.in_checkout(&["reset", "--quiet", "--hard", "FETCH_HEAD"])?;
    // Untracked files go; ignored ones stay: a stack's `.env`, kept out of the repository on purpose.
    git.in_checkout(&["clean", "--quiet", "-ffd"])?;
    Ok(())
}

/// Whether repo.dir is what `sync` makes of it: a git checkout.
pub fn is_checkout(repo: &RepoConfig) -> bool {
    fs::is_directory(&format!("{}/.git", repo.dir))
}

fn ls_remote(git: &Git) -> Result<proc::ProcResult> {
    git.run(None, &["ls-remote", "--", &git.repo.url, &branch_ref(git.repo)], REMOTE_TIMEOUT)
}

fn branch_ref(repo: &RepoConfig) -> String {
    format!("refs/heads/{}", repo.branch)
}

/// What git says when credentials are missing or refused, prompts being off. GitHub answers "not found" for a private
/// repository it won't show.
fn asks_for_credentials(message: &str) -> bool {
    static CREDENTIALS_WANTED: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new("could not read Username|Authentication failed|terminal prompts disabled|Repository not found|returned error: 40[134]")
            .expect("a valid pattern")
    });
    CREDENTIALS_WANTED.is_match(message)
}

/// What `state` reports as `last_sync`.
fn record(node: &Node, repo: &RepoConfig, before: Option<&str>, after: &Result<String>) {
    let mut entry = json!({
        "time": iso(node.now()),
        "result": if after.is_ok() { "ok" } else { "failed" },
        "from": before,
        "to": after.as_ref().ok(),
    });
    if let Err(failure) = after {
        entry["error"] = json!(failure.message);
    }
    fs::write_atomic(&repo.sync_record(), entry.to_string().as_bytes(), 0o644).ok();
}

/// git on one repository, with its token —if any— in the environment only.
struct Git<'a> {
    repo: &'a RepoConfig,
    token: Option<&'a str>,
}

impl Git<'_> {
    fn in_checkout(&self, args: &[&str]) -> Result<proc::ProcResult> {
        self.run(Some(&self.repo.dir), args, CHECKOUT_TIMEOUT)
    }

    fn run(&self, dir: Option<&str>, args: &[&str], timeout: Duration) -> Result<proc::ProcResult> {
        let git_path =
            proc::which("git").ok_or_else(|| error(ErrorCode::Unavailable, "git is not installed on this node"))?;
        if self.token.is_some() {
            require_token_support(&git_path)?;
        }
        let mut argv = vec![git_path];
        if let Some(dir) = dir {
            argv.extend(["-C".to_string(), dir.to_string()]);
        }
        argv.extend(args.iter().map(ToString::to_string));
        let result = proc::run(&argv, proc::Run { env: self.environment(), timeout, ..Default::default() })
            .map_err(|message| error(ErrorCode::Internal, message))?;
        let command = args[0];
        if result.timed_out {
            return Err(error(ErrorCode::Timeout, format!("git {command} did not finish in {}s", timeout.as_secs())));
        }
        if result.exit_code != 0 {
            return Err(error(ErrorCode::Unavailable, format!("git {command}: {}", self.failure_message(&result))));
        }
        Ok(result)
    }

    /// git's last word on stderr, without the token.
    fn failure_message(&self, result: &proc::ProcResult) -> String {
        let stderr = result.err();
        let last_line = stderr
            .trim()
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .map_or_else(|| format!("exit {}", result.exit_code), String::from);
        match self.token {
            Some(token) if !token.is_empty() => last_line.replace(token, "[redacted]"),
            _ => last_line,
        }
    }

    fn environment(&self) -> Vec<String> {
        // No prompt ever, no system or user configuration: what git does is what limen asks.
        let mut env = proc::root_env();
        env.extend(["GIT_TERMINAL_PROMPT=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null"].map(String::from));
        let (Some(token), Some(after_scheme)) = (self.token, self.repo.url.strip_prefix("https://")) else {
            return env;
        };
        let credentials = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
        let host = after_scheme.split('/').next().unwrap_or(after_scheme);
        env.extend([
            "GIT_CONFIG_COUNT=1".to_string(),
            format!("GIT_CONFIG_KEY_0=http.https://{host}/.extraheader"),
            format!("GIT_CONFIG_VALUE_0=Authorization: Basic {credentials}"),
        ]);
        env
    }
}

/// Whether this git sends the token the way limen gives it (`GIT_CONFIG_COUNT`, `GIT_CONFIG_GLOBAL`): 2.32 or later.
/// An older one ignores it quietly, and a readable repository looks like a refused token.
fn require_token_support(git_path: &str) -> Result<()> {
    let version = proc::run(&[git_path.to_string(), "--version".into()], proc::Run::default())
        .map_err(|message| error(ErrorCode::Unavailable, message))?
        .out();
    // `git version 2.39.2`: major and minor of the third word.
    let numbers: Vec<u32> = version
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .split('.')
        .take(2)
        .map(|number| number.parse().unwrap_or(0))
        .collect();
    if matches!(numbers[..], [major, minor] if (major, minor) >= (2, 32)) {
        return Ok(());
    }
    Err(error(ErrorCode::Unavailable, format!("{} can't send a token: limen needs git 2.32 or later", version.trim())))
}
