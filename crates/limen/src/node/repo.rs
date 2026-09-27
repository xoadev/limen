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
use limen_core::protocol::{ErrorCode, LimenError, Result, error};
use limen_core::time::iso;
use regex::Regex;
use serde_json::{Map, Value, json};
use std::sync::LazyLock;
use std::time::Duration;

pub struct Commit {
    pub hash: String,
    pub date: String,
    pub subject: String,
}

/// Brings the checkout to the remote branch: the commit before (if any) and after.
pub fn sync(node: &Node, repo: &RepoConfig) -> Result<(Option<String>, String)> {
    let before = deployed(repo).map(|c| c.hash);
    let token = token(repo);
    let result = (|| {
        if fs::stat(&format!("{}/.git", repo.dir)).map(|i| i.kind) != Some(fs::FileType::Directory) {
            let parent = repo.dir.rsplit_once('/').map_or("/", |(p, _)| p);
            fs::mkdirs(parent, 0o755).map_err(|e| error(ErrorCode::Internal, e))?;
            if fs::exists(&repo.dir) {
                std::fs::remove_dir_all(&repo.dir)
                    .map_err(|e| error(ErrorCode::Internal, format!("cannot remove {}: {e}", repo.dir)))?;
            }
            git(
                repo,
                token.as_deref(),
                None,
                &[
                    "clone",
                    "--quiet",
                    "--depth",
                    "1",
                    "--single-branch",
                    "--branch",
                    &repo.branch,
                    "--",
                    &repo.url,
                    &repo.dir,
                ],
                LONG,
            )?;
        } else {
            // The URL of limen.toml, not the one of the first clone: changing [repo].url moves the node.
            git(repo, token.as_deref(), Some(&repo.dir), &["remote", "set-url", "origin", &repo.url], LONG)?;
            git(
                repo,
                token.as_deref(),
                Some(&repo.dir),
                &["fetch", "--quiet", "--depth", "1", "origin", "--", &format!("refs/heads/{}", repo.branch)],
                LONG,
            )?;
            git(repo, token.as_deref(), Some(&repo.dir), &["reset", "--quiet", "--hard", "FETCH_HEAD"], LONG)?;
            // Untracked files go; ignored ones stay: a stack's `.env`, kept out of the repository on purpose.
            git(repo, token.as_deref(), Some(&repo.dir), &["clean", "--quiet", "-ffd"], LONG)?;
        }
        deployed(repo).map(|c| c.hash).ok_or_else(|| error(ErrorCode::Internal, "git: no commit after sync"))
    })();
    record(
        node,
        repo,
        before.as_deref(),
        result.as_ref().ok().map(String::as_str),
        result.as_ref().err().map(|e| e.message.as_str()),
    );
    result.map(|after| (before, after))
}

const LONG: Duration = Duration::from_secs(600);

pub fn deployed(repo: &RepoConfig) -> Option<Commit> {
    if fs::stat(&format!("{}/.git", repo.dir)).map(|i| i.kind) != Some(fs::FileType::Directory) {
        return None;
    }
    let r = git(repo, None, Some(&repo.dir), &["log", "-1", "--format=%H%n%cI%n%s"], LONG).ok()?;
    let out = r.out();
    let mut lines = out.lines();
    Some(Commit { hash: lines.next()?.into(), date: lines.next()?.into(), subject: lines.next()?.into() })
}

/// The commit the remote branch points at, asked with `ls-remote`: nothing on the node changes.
pub fn remote(repo: &RepoConfig) -> Result<String> {
    let r = git(
        repo,
        token(repo).as_deref(),
        None,
        &["ls-remote", "--", &repo.url, &format!("refs/heads/{}", repo.branch)],
        Duration::from_secs(30),
    )?;
    r.out()
        .lines()
        .find(|l| !l.trim().is_empty())
        .and_then(|l| l.split('\t').next().map(String::from))
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
    match git(
        repo,
        token,
        None,
        &["ls-remote", "--", &repo.url, &format!("refs/heads/{}", repo.branch)],
        Duration::from_secs(30),
    ) {
        Ok(_) => Access::Readable,
        Err(e) => {
            // What git says when credentials are missing or refused, prompts being off. GitHub answers "not found"
            // for a private repository it won't show.
            static AUTH: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new("could not read Username|Authentication failed|terminal prompts disabled|Repository not found|returned error: 40[134]").unwrap()
            });
            if AUTH.is_match(&e.message) { Access::NeedsToken } else { Access::Failed(e.message) }
        }
    }
}

pub fn last_sync(repo: &RepoConfig) -> Option<Value> {
    fs::read_text(&repo.sync_record())
        .and_then(|t| serde_json::from_str::<Map<String, Value>>(&t).ok())
        .map(Value::Object)
}

pub fn token(repo: &RepoConfig) -> Option<String> {
    fs::read_text(&repo.token_file).map(|t| t.trim().to_string()).filter(|t| !t.is_empty())
}

fn record(node: &Node, repo: &RepoConfig, from: Option<&str>, to: Option<&str>, failure: Option<&str>) {
    let mut entry = json!({
        "time": iso(node.now()),
        "result": if failure.is_none() { "ok" } else { "failed" },
        "from": from,
        "to": to,
    });
    if let Some(f) = failure {
        entry["error"] = json!(f);
    }
    fs::write_atomic(&repo.sync_record(), entry.to_string().as_bytes(), 0o644).ok();
}

fn git(
    repo: &RepoConfig,
    token: Option<&str>,
    dir: Option<&str>,
    args: &[&str],
    timeout: Duration,
) -> Result<proc::ProcResult> {
    let git = proc::which("git").ok_or_else(|| error(ErrorCode::Unavailable, "git is not installed on this node"))?;
    let mut argv = vec![git];
    if let Some(d) = dir {
        argv.extend(["-C".to_string(), d.to_string()]);
    }
    argv.extend(args.iter().map(|a| a.to_string()));
    let r = proc::run(&argv, proc::Run { env: env(repo, token), timeout, ..Default::default() })
        .map_err(|e| error(ErrorCode::Internal, e))?;
    if r.timed_out {
        return Err(error(ErrorCode::Timeout, format!("git {} did not finish in {}s", args[0], timeout.as_secs())));
    }
    if r.exit_code != 0 {
        let err = r.err();
        let message = err
            .trim()
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(String::from)
            .unwrap_or(format!("exit {}", r.exit_code));
        let message = match token {
            Some(t) if !t.is_empty() => message.replace(t, "[redacted]"),
            _ => message,
        };
        return Err(LimenError::new(ErrorCode::Unavailable, format!("git {}: {message}", args[0])));
    }
    Ok(r)
}

fn env(repo: &RepoConfig, token: Option<&str>) -> Vec<String> {
    // No prompt ever, no system or user configuration: what git does is what limen asks.
    let mut env = proc::root_env();
    env.extend(["GIT_TERMINAL_PROMPT=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null"].map(String::from));
    let (Some(token), Some(rest)) = (token, repo.url.strip_prefix("https://")) else { return env };
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("x-access-token:{token}"));
    let host = rest.split('/').next().unwrap_or(rest);
    env.extend([
        "GIT_CONFIG_COUNT=1".to_string(),
        format!("GIT_CONFIG_KEY_0=http.https://{host}/.extraheader"),
        format!("GIT_CONFIG_VALUE_0=Authorization: Basic {basic}"),
    ]);
    env
}
