//! The coding agents running on this machine, read straight from `/proc`.
//!
//! An agent is a `claude`, `omp` or `codex` process whose parent is not itself
//! an agent; everything below it (MCP servers, the shell it runs commands in, a
//! build it started) is counted against it. herdr, when it is running, adds
//! where each agent sits, whether it is busy, its title and its session id: it
//! puts `HERDR_PANE_ID` in the environment of everything it starts, so a pane
//! is matched to its process exactly rather than by guessing from the cwd.

use crate::usage::TokenTotals;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::Mutex,
    thread,
    time::{Duration, UNIX_EPOCH},
};

const HERDR_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Omp,
    Codex,
}

impl Agent {
    fn from_comm(comm: &str) -> Option<Self> {
        match comm {
            "claude" => Some(Self::Claude),
            "omp" => Some(Self::Omp),
            "codex" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Omp => "omp",
            Self::Codex => "codex",
        }
    }
}

/// One process as `/proc` lists it.
#[derive(Clone, Debug, Default)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
    pub cmd: Vec<String>,
}

/// Something running under an agent: one of its children together with
/// everything below that child, merged with siblings that read the same.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub label: String,
    pub kib: u64,
    pub procs: usize,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pane {
    pub workspace: String,
    pub title: Option<String>,
    pub working: bool,
    /// Claude Code's session id, or the path of OMP's session file.
    pub session: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub agent: Agent,
    pub pid: i32,
    pub cwd: String,
    pub pane_id: Option<String>,
    pub pane: Option<Pane>,
    /// Claude Code's session id, or the path of OMP's session file.
    pub session: Option<String>,
    /// When the session's transcript was last written, which is the last time
    /// the agent said or did anything.
    pub active_ms: Option<i64>,
    /// What the last turn sent the model: how full the context is.
    pub context: Option<u64>,
    /// How much the model it is talking to can hold.
    pub context_limit: Option<u64>,
    pub model: Option<String>,
    /// Every token the session has spent so far.
    pub tokens: Option<TokenTotals>,
    /// The agent itself and everything below it.
    pub kib: u64,
    /// The agent process alone first, then what runs under it, largest first.
    pub parts: Vec<Part>,
    pub pids: Vec<i32>,
}

impl Session {
    /// The command that brings this conversation back after it is closed.
    pub fn resume_command(&self) -> Option<String> {
        // Claude Code writes no transcript until the first message, and
        // resuming a session with none fails.
        self.active_ms?;
        let session = self.session.as_deref()?;
        let resume = match self.agent {
            Agent::Claude => format!("claude --resume {session}"),
            Agent::Omp => format!("omp --resume={}", shell_quote(session)),
            Agent::Codex => format!("codex resume {session}"),
        };
        Some(format!("cd {} && {resume}", shell_quote(&self.cwd)))
    }

    pub fn working(&self) -> bool {
        self.pane.as_ref().is_some_and(|pane| pane.working)
    }

    /// Whether it is busy is known only for an agent in a herdr pane; outside
    /// one, a quiet transcript may just be a long build.
    pub fn status_known(&self) -> bool {
        self.pane.is_some()
    }

    /// How long it has sat idle, for a session known to be waiting.
    pub fn idle_ms(&self, now_ms: i64) -> Option<i64> {
        if !self.status_known() || self.working() {
            return None;
        }
        self.quiet_ms(now_ms)
    }

    /// How long since its transcript last changed, whatever it is doing.
    pub fn quiet_ms(&self, now_ms: i64) -> Option<i64> {
        self.active_ms.map(|at| now_ms.saturating_sub(at).max(0))
    }
}

/// An MCP server whose agent is gone: reparented to init or to the user's
/// systemd, it would otherwise run until logout.
#[derive(Clone, Debug, PartialEq)]
pub struct Orphan {
    pub pid: i32,
    pub label: String,
    pub kib: u64,
    /// Each process with the command line it had, so a stop asked for from
    /// an old report cannot reach a process that has taken over its pid.
    pub procs: Vec<(i32, Vec<String>)>,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub sessions: Vec<Session>,
    pub orphans: Vec<Orphan>,
    pub mem_total_kib: u64,
    /// Workspace labels in herdr's own order, so the card lists them the way
    /// herdr's sidebar does.
    pub workspaces: Vec<String>,
    pub herdr: bool,
    pub collected_at_ms: i64,
}

impl Report {
    pub fn total_kib(&self) -> u64 {
        self.sessions.iter().map(|session| session.kib).sum::<u64>()
            + self.orphans.iter().map(|orphan| orphan.kib).sum::<u64>()
    }
}

pub fn collect() -> Report {
    let procs = read_procs();
    let herdr = read_herdr();
    let pane_ids: HashMap<i32, String> = procs
        .iter()
        .filter(|proc| Agent::from_comm(&proc.comm).is_some())
        .filter_map(|proc| Some((proc.pid, herdr_pane_of(proc.pid)?)))
        .collect();
    let (mut sessions, mut orphans) = build(&procs, &pane_ids, read_kib);
    // A user service that happens to speak MCP was started on purpose.
    orphans.retain(|orphan| !own_service(orphan.pid));
    let (panes, workspaces) = herdr.clone().unwrap_or_default();
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|proc| (proc.pid, proc)).collect();
    let mut transcripts: Vec<Option<PathBuf>> = Vec::with_capacity(sessions.len());
    for session in &mut sessions {
        session.pane = session
            .pane_id
            .as_ref()
            .and_then(|id| panes.get(id))
            .cloned();
        let cmd = by_pid
            .get(&session.pid)
            .map(|proc| proc.cmd.as_slice())
            .unwrap_or(&[]);
        session.session = session_of(session.agent, session.pid, cmd, session.pane.as_ref());
        transcripts.push(
            session
                .session
                .as_deref()
                .and_then(|id| transcript(session.agent, id, &session.cwd)),
        );
    }
    // An OMP that names its session nowhere is given the newest transcript
    // in its cwd's folder, but only when it is the one such OMP there: two
    // would both be handed the same file, and one the other's resume command.
    for index in 0..sessions.len() {
        let session = &sessions[index];
        if transcripts[index].is_some() || session.agent != Agent::Omp {
            continue;
        }
        let alike = (0..sessions.len())
            .filter(|other| {
                transcripts[*other].is_none()
                    && sessions[*other].agent == Agent::Omp
                    && sessions[*other].cwd == session.cwd
            })
            .count();
        if alike == 1 {
            transcripts[index] = newest_omp_transcript(&session.cwd);
            sessions[index].session = transcripts[index]
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned());
        }
    }
    let seen: HashSet<PathBuf> = transcripts.iter().flatten().cloned().collect();
    for (session, transcript) in sessions.iter_mut().zip(transcripts) {
        if let Some(path) = transcript {
            session.active_ms = modified_ms(&path);
            if let Some(tallied) = tally(session.agent, &path) {
                session.tokens = Some(tallied.tokens);
                session.context = (tallied.context > 0).then_some(tallied.context);
                session.context_limit = tallied
                    .model
                    .as_deref()
                    .and_then(|model| context_limit(session.agent, model))
                    // A model that took more than its listed window evidently
                    // has the larger one.
                    .map(|limit| match session.context {
                        Some(used) if used > limit => limit.max(1_000_000),
                        _ => limit,
                    });
                session.model = tallied.model;
            }
        }
    }
    // Sessions come and go (an OMP `/new` starts a new file); what was read
    // of the ones that went is no use.
    if let Ok(mut guard) = TALLIES.lock() {
        if let Some(tallies) = guard.as_mut() {
            tallies.retain(|path, _| seen.contains(path));
        }
    }
    Report {
        sessions,
        orphans,
        mem_total_kib: crate::system::memory_total_kib(),
        workspaces,
        herdr: herdr.is_some(),
        collected_at_ms: crate::usage::now_ms(),
    }
}

/// Whether a process runs in a systemd unit of its own rather than in the
/// scope of the terminal or app that started it, the way a reparented MCP
/// server still does.
fn own_service(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .ok()
        .and_then(|text| {
            let path = text.lines().find_map(|line| line.strip_prefix("0::"))?;
            Some(path.trim_end().ends_with(".service"))
        })
        .unwrap_or(false)
}

/// Splits the process table into agents and the orphans they left behind.
/// `kib` is asked only about processes that end up on the card, because
/// reading a process's memory map is the expensive part of a pass.
pub fn build(
    procs: &[Proc],
    pane_ids: &HashMap<i32, String>,
    kib: impl Fn(i32) -> u64,
) -> (Vec<Session>, Vec<Orphan>) {
    let by_pid: HashMap<i32, &Proc> = procs.iter().map(|proc| (proc.pid, proc)).collect();
    let mut children: HashMap<i32, Vec<i32>> = HashMap::new();
    for proc in procs {
        children.entry(proc.ppid).or_default().push(proc.pid);
    }
    let is_agent = |pid: i32| {
        by_pid
            .get(&pid)
            .is_some_and(|proc| Agent::from_comm(&proc.comm).is_some())
    };
    let subtree = |root: i32| {
        let mut out = vec![root];
        let mut index = 0;
        while index < out.len() {
            if let Some(kids) = children.get(&out[index]) {
                out.extend(kids);
            }
            index += 1;
        }
        out
    };
    let mut claimed = HashSet::new();
    let mut sessions = Vec::new();
    for proc in procs {
        let Some(agent) = Agent::from_comm(&proc.comm) else {
            continue;
        };
        // An agent started by another agent is part of that one's tree.
        let mut ancestor = proc.ppid;
        let mut nested = false;
        let mut under_herdr = false;
        while let Some(parent) = by_pid.get(&ancestor) {
            if is_agent(parent.pid) {
                nested = true;
                break;
            }
            under_herdr |= parent.comm == "herdr";
            if parent.ppid == ancestor {
                break;
            }
            ancestor = parent.ppid;
        }
        if nested {
            continue;
        }
        let pids = subtree(proc.pid);
        claimed.extend(pids.iter().copied());
        let own = kib(proc.pid);
        let mut parts = vec![Part {
            label: format!("{} (main)", agent.label()),
            kib: own,
            procs: 1,
        }];
        let mut grouped: Vec<Part> = Vec::new();
        for child in children.get(&proc.pid).into_iter().flatten() {
            let Some(child_proc) = by_pid.get(child) else {
                continue;
            };
            let below = subtree(*child);
            let label = part_label(child_proc, &below, &by_pid);
            let size: u64 = below.iter().map(|pid| kib(*pid)).sum();
            match grouped.iter_mut().find(|part| part.label == label) {
                Some(part) => {
                    part.kib += size;
                    part.procs += below.len();
                }
                None => grouped.push(Part {
                    label,
                    kib: size,
                    procs: below.len(),
                }),
            }
        }
        grouped.sort_by(|a, b| b.kib.cmp(&a.kib).then_with(|| a.label.cmp(&b.label)));
        parts.extend(grouped);
        sessions.push(Session {
            agent,
            pid: proc.pid,
            cwd: proc_cwd(proc.pid),
            // The variable is inherited, so a process that left herdr's tree
            // (double-forked, or started by something running in a pane)
            // still carries it; only one herdr still holds names a pane.
            pane_id: pane_ids.get(&proc.pid).filter(|_| under_herdr).cloned(),
            pane: None,
            session: None,
            active_ms: None,
            context: None,
            context_limit: None,
            model: None,
            tokens: None,
            kib: parts.iter().map(|part| part.kib).sum(),
            parts,
            pids,
        });
    }
    sessions.sort_by(|a, b| b.kib.cmp(&a.kib).then(a.pid.cmp(&b.pid)));

    let reaper = |pid: i32| {
        pid == 1
            || by_pid
                .get(&pid)
                .is_some_and(|proc| proc.comm == "systemd" || proc.comm == "init")
    };
    let mut orphans = Vec::new();
    for proc in procs {
        if claimed.contains(&proc.pid) || !reaper(proc.ppid) || !is_mcp(proc) {
            continue;
        }
        let pids = subtree(proc.pid);
        orphans.push(Orphan {
            pid: proc.pid,
            label: part_label(proc, &pids, &by_pid),
            kib: pids.iter().map(|pid| kib(*pid)).sum(),
            procs: pids
                .iter()
                .filter_map(|pid| Some((*pid, by_pid.get(pid)?.cmd.clone())))
                .collect(),
        });
    }
    orphans.sort_by(|a, b| b.kib.cmp(&a.kib).then(a.pid.cmp(&b.pid)));
    (sessions, orphans)
}

fn is_mcp(proc: &Proc) -> bool {
    let cmd = proc.cmd.join(" ");
    cmd.contains("codegraph")
        || cmd
            .split(|c: char| !c.is_alphanumeric())
            .any(|word| word == "mcp")
}

/// A name for what a child of an agent is. A wrapper process says little
/// (`node`, `bash -c`), so the whole subtree is searched for an MCP server's
/// name before falling back to the command the child runs.
fn part_label(proc: &Proc, below: &[i32], by_pid: &HashMap<i32, &Proc>) -> String {
    let words: Vec<&str> = below
        .iter()
        .filter_map(|pid| by_pid.get(pid))
        .flat_map(|proc| proc.cmd.iter().map(String::as_str))
        .collect();
    if words.iter().any(|word| word.contains("codegraph")) {
        return "codegraph mcp".into();
    }
    if below
        .iter()
        .filter_map(|pid| by_pid.get(pid))
        .any(|proc| is_mcp(proc))
    {
        // `node /x/@scope/server-github/dist/index.js` names itself in the
        // package path; a bare `mcp-server-foo` binary in its own name.
        let named = words
            .iter()
            .flat_map(|word| word.split('/'))
            .find(|part| part.contains("mcp") || part.starts_with("server-"))
            .filter(|part| *part != "mcp" && *part != "--mcp");
        return match named {
            Some(name) => name.trim_end_matches(".js").to_owned(),
            None => format!("{} mcp", command_name(proc)),
        };
    }
    command_name(proc)
}

/// The program a process runs, past an interpreter: `node foo.js` is `foo`.
/// A shell is just `shell`: what an agent's Bash tool hands it is a long
/// generated line that names nothing useful.
fn command_name(proc: &Proc) -> String {
    let base = |word: &str| word.rsplit('/').next().unwrap_or(word).to_owned();
    let first = proc.cmd.first().map(|word| base(word)).unwrap_or_default();
    if matches!(proc.comm.as_str(), "bash" | "sh" | "dash" | "zsh" | "fish") {
        return "shell".into();
    }
    let interpreted = matches!(
        first.as_str(),
        "node" | "bun" | "python" | "python3" | "deno" | "npx" | "uvx"
    );
    if interpreted {
        if let Some(script) = proc.cmd.iter().skip(1).find(|word| !word.starts_with('-')) {
            return script_name(script);
        }
    }
    if first.is_empty() {
        proc.comm.clone()
    } else {
        first
    }
}

/// `/x/node_modules/@scope/server-github/dist/index.js` is `server-github`:
/// an entry point named `index` says nothing, the package it sits in does.
fn script_name(path: &str) -> String {
    const GENERIC: [&str; 8] = [
        "index", "main", "cli", "server", "dist", "build", "lib", "bin",
    ];
    let stem = |part: &str| {
        part.trim_end_matches(".js")
            .trim_end_matches(".mjs")
            .trim_end_matches(".cjs")
            .trim_end_matches(".py")
            .to_owned()
    };
    path.rsplit('/')
        .map(stem)
        .find(|part| !part.is_empty() && !GENERIC.contains(&part.as_str()) && part != "src")
        .unwrap_or_else(|| stem(path.rsplit('/').next().unwrap_or(path)))
}

fn read_procs() -> Vec<Proc> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|pid| {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
            let (comm, ppid) = parse_stat(&stat)?;
            let cmd = read_cmdline(pid).unwrap_or_default();
            Some(Proc {
                pid,
                ppid,
                comm,
                cmd,
            })
        })
        .collect()
}

fn read_cmdline(pid: i32) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|byte| *byte == 0)
            .filter(|word| !word.is_empty())
            .map(|word| String::from_utf8_lossy(word).into_owned())
            .collect(),
    )
}

/// `comm` sits in parentheses and may itself contain spaces or parentheses,
/// so the fields are read from after the last `)`.
fn parse_stat(stat: &str) -> Option<(String, i32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat.get(open + 1..close)?.to_owned();
    let ppid = stat
        .get(close + 2..)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some((comm, ppid))
}

/// Proportional memory, in RAM and in swap: a page shared by several processes
/// is split between them, so adding a tree up never counts a page twice.
fn read_kib(pid: i32) -> u64 {
    fs::read_to_string(format!("/proc/{pid}/smaps_rollup"))
        .map(|text| parse_rollup(&text))
        .unwrap_or(0)
}

fn parse_rollup(text: &str) -> u64 {
    text.lines()
        .filter(|line| line.starts_with("Pss:") || line.starts_with("SwapPss:"))
        .filter_map(|line| line.split_whitespace().nth(1)?.parse::<u64>().ok())
        .sum()
}

fn proc_cwd(pid: i32) -> String {
    fs::read_link(format!("/proc/{pid}/cwd"))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Only the one variable is kept; an agent's environment can carry keys.
fn herdr_pane_of(pid: i32) -> Option<String> {
    let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
    raw.split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(b"HERDR_PANE_ID="))
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .filter(|value| !value.is_empty())
}

type HerdrState = (HashMap<String, Pane>, Vec<String>);

fn read_herdr() -> Option<HerdrState> {
    let panes = herdr_json(&["pane", "list"])?;
    let workspaces = herdr_json(&["workspace", "list"])?;
    Some(parse_herdr(&panes, &workspaces))
}

fn parse_herdr(panes: &Value, workspaces: &Value) -> HerdrState {
    let mut labels = HashMap::new();
    let mut order = Vec::new();
    for workspace in workspaces["result"]["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let (Some(id), Some(label)) = (
            workspace["workspace_id"].as_str(),
            workspace["label"].as_str(),
        ) else {
            continue;
        };
        labels.insert(id.to_owned(), label.to_owned());
        order.push(label.to_owned());
    }
    let mut out = HashMap::new();
    for pane in panes["result"]["panes"].as_array().into_iter().flatten() {
        let Some(id) = pane["pane_id"].as_str() else {
            continue;
        };
        let workspace = pane["workspace_id"].as_str().unwrap_or_default();
        out.insert(
            id.to_owned(),
            Pane {
                workspace: labels
                    .get(workspace)
                    .cloned()
                    .unwrap_or_else(|| workspace.to_owned()),
                title: pane["terminal_title_stripped"]
                    .as_str()
                    .map(clean_title)
                    .filter(|title| !title.is_empty())
                    .map(str::to_owned),
                working: pane["agent_status"].as_str() == Some("working"),
                session: pane["agent_session"]["value"].as_str().map(str::to_owned),
            },
        );
    }
    (out, order)
}

/// OMP puts `π > ` before its title; the row already says it is OMP.
fn clean_title(title: &str) -> &str {
    let title = title.trim();
    title
        .strip_prefix("π >")
        .map(str::trim_start)
        .unwrap_or(title)
}

fn herdr_json(args: &[&str]) -> Option<Value> {
    let output = crate::usage::run_cli("herdr", "herdr", args, HERDR_TIMEOUT).ok()?;
    serde_json::from_slice(&output).ok()
}

fn herdr(args: &[&str]) -> Result<(), String> {
    crate::usage::run_cli("herdr", "herdr", args, HERDR_TIMEOUT).map(|_| ())
}

fn claude_dir() -> Option<PathBuf> {
    env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(env::var_os("HOME")?).join(".claude")))
}

/// Which conversation an agent is in. Claude Code keeps a file per running
/// process naming its session, which works inside herdr or out of it and
/// follows a `/resume` made after launch. OMP is asked of herdr first, which
/// follows a `/new`, then of the `--resume` it was started with.
fn session_of(agent: Agent, pid: i32, cmd: &[String], pane: Option<&Pane>) -> Option<String> {
    let from_pane = || pane.and_then(|pane| pane.session.clone());
    match agent {
        Agent::Claude => claude_dir()
            .and_then(|dir| fs::read(dir.join(format!("sessions/{pid}.json"))).ok())
            .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
            .and_then(|value| value["sessionId"].as_str().map(str::to_owned))
            .or_else(from_pane),
        Agent::Omp => from_pane().or_else(|| resume_arg(cmd)),
        Agent::Codex => from_pane(),
    }
}

fn resume_arg(cmd: &[String]) -> Option<String> {
    cmd.iter().enumerate().find_map(|(index, word)| {
        word.strip_prefix("--resume=")
            .map(str::to_owned)
            .or_else(|| {
                (word == "--resume")
                    .then(|| cmd.get(index + 1).cloned())
                    .flatten()
            })
    })
}

/// An OMP started fresh outside herdr names its session nowhere, but writes
/// it under a folder named for its cwd: `~/Workspace/sysi` is
/// `-Workspace-sysi`. The newest file there is the one it is writing.
fn newest_omp_transcript(cwd: &str) -> Option<PathBuf> {
    let home = PathBuf::from(env::var_os("HOME")?);
    let relative = Path::new(cwd).strip_prefix(&home).ok()?;
    let folder = format!("-{}", relative.to_string_lossy().replace('/', "-"));
    fs::read_dir(home.join(".omp/agent/sessions").join(folder))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .max_by_key(|path| modified_ms(path).unwrap_or(0))
}

fn transcript(agent: Agent, session: &str, cwd: &str) -> Option<PathBuf> {
    match agent {
        Agent::Omp => Some(PathBuf::from(session)).filter(|path| path.is_file()),
        Agent::Claude => {
            let projects = claude_dir()?.join("projects");
            // Claude Code names a project's folder after its cwd, every
            // character but a letter or digit made a dash. A session resumed
            // from elsewhere lives in another folder, so fall back to looking.
            let folder: String = cwd
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let direct = projects.join(folder).join(format!("{session}.jsonl"));
            if direct.is_file() {
                return Some(direct);
            }
            fs::read_dir(projects)
                .ok()?
                .flatten()
                .map(|dir| dir.path().join(format!("{session}.jsonl")))
                .find(|path| path.is_file())
        }
        Agent::Codex => None,
    }
}

/// How far each transcript has been read, and what it added up to. A long
/// session's transcript runs to tens of megabytes, and a pass every few
/// seconds reads only what was appended since the last one.
#[derive(Default)]
struct Tally {
    offset: u64,
    tokens: TokenTotals,
    context: u64,
    last_id: String,
    /// The model the last reply came from, as OMP's catalog names it
    /// (`provider/id`) or as Claude Code records it.
    model: Option<String>,
}

/// What a pass found in a transcript.
pub struct Tallied {
    pub tokens: TokenTotals,
    pub context: u64,
    pub model: Option<String>,
}

static TALLIES: Mutex<Option<HashMap<PathBuf, Tally>>> = Mutex::new(None);

fn tally(agent: Agent, path: &Path) -> Option<Tallied> {
    let mut file = fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let mut guard = TALLIES.lock().ok()?;
    let tally = guard
        .get_or_insert_with(HashMap::new)
        .entry(path.to_owned())
        .or_default();
    // Shorter than what was read: rewritten, so start over.
    if len < tally.offset {
        *tally = Tally::default();
    }
    if len > tally.offset {
        file.seek(SeekFrom::Start(tally.offset)).ok()?;
        // A line at a time: a first read of a long session runs to tens of
        // megabytes, which the allocator would keep long after.
        let mut reader = BufReader::new(file.take(len - tally.offset));
        let mut line = Vec::new();
        loop {
            line.clear();
            let read = reader.read_until(b'\n', &mut line).ok()?;
            // A line still being written is left for the next pass.
            if read == 0 || line.last() != Some(&b'\n') {
                break;
            }
            add_line(agent, tally, &line[..line.len() - 1]);
            tally.offset += read as u64;
        }
    }
    Some(Tallied {
        tokens: tally.tokens,
        context: tally.context,
        model: tally.model.clone(),
    })
}

fn add_line(agent: Agent, tally: &mut Tally, line: &[u8]) {
    let has = |needle: &[u8]| line.windows(needle.len()).any(|window| window == needle);
    // Most lines are tool output; only replies and compactions are parsed.
    let compaction = has(b"\"compact_boundary\"") || has(b"\"type\":\"compaction\"");
    if !compaction && !has(b"\"usage\"") {
        return;
    }
    let Ok(value) = serde_json::from_slice::<Value>(line) else {
        return;
    };
    // A compaction shrinks the context at once; the next reply's usage will
    // say the same, but until it arrives the old figure would be a lie.
    // Claude Code: `compactMetadata.postTokens`; OMP: `tokensAfter`.
    if compaction {
        let after = value
            .pointer("/compactMetadata/postTokens")
            .or_else(|| value.get("tokensAfter"))
            .and_then(Value::as_u64);
        if let Some(after) = after {
            tally.context = after;
        }
        return;
    }
    let Some(usage) = value.pointer("/message/usage") else {
        return;
    };
    let model = match agent {
        Agent::Omp => value
            .pointer("/message/provider")
            .and_then(Value::as_str)
            .zip(value.pointer("/message/model").and_then(Value::as_str))
            .map(|(provider, model)| format!("{provider}/{model}")),
        _ => value
            .pointer("/message/model")
            .and_then(Value::as_str)
            .map(str::to_owned),
    };
    // Claude Code writes `<synthetic>` for replies it made up itself.
    if let Some(model) = model.filter(|model| !model.contains("synthetic")) {
        tally.model = Some(model);
    }
    let count = |key: &str| usage.get(key).and_then(Value::as_u64).unwrap_or(0);
    let turn = match agent {
        Agent::Omp => TokenTotals {
            input: count("input"),
            output: count("output"),
            cache_read: count("cacheRead"),
            cache_write: count("cacheWrite"),
        },
        _ => {
            if value["isSidechain"].as_bool() == Some(true) {
                return;
            }
            // Claude Code writes one line per content block of a reply, each
            // carrying the same usage.
            let id = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !id.is_empty() && id == tally.last_id {
                return;
            }
            tally.last_id = id.to_owned();
            TokenTotals {
                input: count("input_tokens"),
                output: count("output_tokens"),
                cache_read: count("cache_read_input_tokens"),
                cache_write: count("cache_creation_input_tokens"),
            }
        }
    };
    tally.tokens.merge(turn);
    let context = turn.input + turn.cache_read + turn.cache_write;
    if context > 0 {
        tally.context = context;
    }
}

fn context_limit(agent: Agent, model: &str) -> Option<u64> {
    match agent {
        Agent::Claude => Some(claude_context_limit(model)),
        Agent::Omp => omp_context_limit(model),
        Agent::Codex => None,
    }
}

/// Every Claude model from Opus 4.6 and Sonnet 4.6 on holds 1M tokens; the
/// ones before (Haiku 4.5, Opus 4.5, Sonnet 4.5 and older) hold 200K.
fn claude_context_limit(model: &str) -> u64 {
    let model = model.trim_start_matches("claude-");
    let older = [
        "haiku-4",
        "opus-4-5",
        "opus-4-1",
        "opus-4-0",
        "sonnet-4-5",
        "sonnet-4-0",
        "3-",
    ];
    let legacy = older.iter().any(|prefix| model.starts_with(prefix))
        || model == "opus-4"
        || model == "sonnet-4";
    if legacy {
        200_000
    } else {
        1_000_000
    }
}

/// OMP's own catalog says how big each model's window is. Asking it takes a
/// couple of seconds, so it is asked once, and again only when a session
/// names a model the last answer did not have (at most hourly).
fn omp_context_limit(model: &str) -> Option<u64> {
    static CATALOG: Mutex<Option<(i64, HashMap<String, u64>)>> = Mutex::new(None);
    let mut guard = CATALOG.lock().ok()?;
    let stale = match guard.as_ref() {
        None => true,
        Some((at, catalog)) => {
            !catalog.contains_key(model) && crate::usage::now_ms().saturating_sub(*at) > 3_600_000
        }
    };
    if stale {
        let catalog =
            crate::usage::run_cli("OMP", "omp", &["models", "--json"], Duration::from_secs(10))
                .ok()
                .and_then(|raw| serde_json::from_slice::<Value>(&raw).ok())
                .map(|value| parse_omp_catalog(&value))
                .unwrap_or_default();
        *guard = Some((crate::usage::now_ms(), catalog));
    }
    guard.as_ref()?.1.get(model).copied()
}

fn parse_omp_catalog(value: &Value) -> HashMap<String, u64> {
    value["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|model| {
            let window = model["contextWindow"].as_u64()?;
            let name = model["selector"].as_str().map(str::to_owned).or_else(|| {
                Some(format!(
                    "{}/{}",
                    model["provider"].as_str()?,
                    model["id"].as_str()?
                ))
            })?;
            Some((name, window))
        })
        .collect()
}

fn modified_ms(path: &Path) -> Option<i64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as i64)
}

fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:=+@".contains(c))
    {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', r"'\''"))
}

pub fn alive(pid: i32) -> bool {
    // A zombie still has a /proc entry, but it has exited.
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            let close = stat.rfind(')')?;
            stat.get(close + 2..)?
                .split_whitespace()
                .next()
                .map(|state| state != "Z")
        })
        .unwrap_or(false)
}

/// Quits the agent the way a person at its prompt would: Ctrl+D, typed into
/// its pane. Claude Code asks for it twice; the second press is sent only
/// once the first has visibly not ended it, because a press that reached the
/// shell behind an agent already gone would close the pane too. Blocks for a
/// moment, so it is meant for a worker thread.
pub fn quit(session: &Session) -> Result<(), String> {
    let Some(pane) = session.pane_id.as_deref() else {
        return hang_up(session);
    };
    // The row may come from a pass made before the agent exited; a Ctrl+D
    // then reaches the shell left in the pane and closes it.
    if !pane_runs(pane, session.pid) {
        return Err(format!("{} has already exited", session.agent.label()));
    }
    herdr(&["pane", "send-keys", pane, "ctrl+d"])?;
    if session.agent == Agent::Claude {
        thread::sleep(Duration::from_millis(300));
        if alive(session.pid) {
            herdr(&["pane", "send-keys", pane, "ctrl+d"])?;
        }
    }
    Ok(())
}

fn pane_runs(pane: &str, pid: i32) -> bool {
    herdr_json(&["pane", "process-info", "--pane", pane]).is_some_and(|value| {
        value["result"]["process_info"]["foreground_processes"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|proc| proc["pid"].as_i64() == Some(i64::from(pid)))
    })
}

/// Whether `pid` is still the agent the row was made for, not a process
/// that has since been given the same number.
fn still_agent(session: &Session) -> bool {
    alive(session.pid)
        && fs::read_to_string(format!("/proc/{}/comm", session.pid))
            .is_ok_and(|comm| comm.trim() == session.agent.label())
}

/// What closing its terminal would do: SIGHUP. The agent exits and its MCP
/// servers follow when their pipes close; the shell in a herdr pane stays.
pub fn hang_up(session: &Session) -> Result<(), String> {
    if !still_agent(session) {
        return Err(format!("{} has already exited", session.agent.label()));
    }
    signal(session.pid, libc::SIGHUP)
}

/// Stops an orphan's processes that are still the ones it was found with.
pub fn terminate(orphan: &Orphan) -> Result<(), String> {
    for (pid, cmd) in &orphan.procs {
        if read_cmdline(*pid).as_ref() == Some(cmd) {
            signal(*pid, libc::SIGTERM)?;
        }
    }
    Ok(())
}

fn signal(pid: i32, signal: i32) -> Result<(), String> {
    if pid <= 1 {
        return Err("Refusing to signal that process".into());
    }
    if unsafe { libc::kill(pid, signal) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(format!("Could not signal {pid}: {error}"))
}

pub fn focus(pane: &str) -> Result<(), String> {
    herdr(&["agent", "focus", pane])
}

/// Runs `job` off the main loop and hands its result back on it.
pub fn spawn<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + 'static,
) {
    let (tx, rx) = async_channel::bounded(1);
    thread::spawn(move || {
        let _ = tx.send_blocking(job());
    });
    glib::MainContext::default().spawn_local(async move {
        if let Ok(value) = rx.recv().await {
            done(value);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn proc(pid: i32, ppid: i32, comm: &str, cmd: &str) -> Proc {
        Proc {
            pid,
            ppid,
            comm: comm.into(),
            cmd: cmd.split(' ').map(str::to_owned).collect(),
        }
    }

    fn table() -> Vec<Proc> {
        vec![
            proc(1, 0, "systemd", "/sbin/init"),
            proc(900, 1, "systemd", "/usr/lib/systemd/systemd --user"),
            proc(10, 900, "herdr", "herdr server"),
            proc(11, 10, "bash", "/bin/bash"),
            proc(20, 11, "claude", "/home/u/.local/bin/claude"),
            proc(
                21,
                20,
                "node",
                "node /home/u/.local/bin/codegraph serve --mcp",
            ),
            proc(
                22,
                21,
                "MainThread",
                "node codegraph.js serve --mcp --path /w",
            ),
            proc(23, 20, "bash", "bash -c cargo build"),
            proc(24, 23, "cargo", "cargo build"),
            proc(25, 20, "bash", "bash /home/u/bin/claude"),
            proc(26, 20, "claude", "claude -p subtask"),
            proc(30, 11, "omp", "omp --resume=/s.jsonl"),
            proc(
                31,
                30,
                "node",
                "node /x/node_modules/@scope/server-github/dist/index.js --stdio",
            ),
            proc(
                40,
                900,
                "node",
                "node /home/u/.local/bin/codegraph serve --mcp",
            ),
            proc(41, 40, "MainThread", "node codegraph.js serve --mcp"),
            proc(50, 900, "node", "node /srv/app.js"),
            proc(60, 900, "omp", "omp"),
        ]
    }

    #[test]
    fn agents_own_their_trees_and_orphans_are_found() {
        // 60 inherited a pane from its parent but has left herdr's tree.
        let panes = HashMap::from([(20, "wA:p1".to_owned()), (60, "wA:p1".to_owned())]);
        let (sessions, orphans) = build(&table(), &panes, |pid| pid as u64);
        assert_eq!(sessions.len(), 3, "the nested claude belongs to its parent");
        let stray = sessions.iter().find(|s| s.pid == 60).unwrap();
        assert_eq!(stray.pane_id, None);
        let claude = sessions.iter().find(|s| s.agent == Agent::Claude).unwrap();
        assert_eq!(claude.pane_id.as_deref(), Some("wA:p1"));
        let total: u64 = [20, 21, 22, 23, 24, 25, 26].iter().map(|p| *p as u64).sum();
        assert_eq!(claude.kib, total);
        assert_eq!(claude.parts[0].label, "claude (main)");
        assert_eq!(claude.parts[0].kib, 20);
        let part = |label: &str| {
            let part = claude.parts.iter().find(|p| p.label == label).unwrap();
            (part.kib, part.procs)
        };
        assert_eq!(part("codegraph mcp"), (43, 2));
        // Both shells, whatever they run, are one row.
        assert_eq!(part("shell"), (23 + 24 + 25, 3));
        assert_eq!(part("claude"), (26, 1));
        // Largest first, after the agent itself.
        let order: Vec<&str> = claude.parts.iter().map(|p| p.label.as_str()).collect();
        assert_eq!(order, ["claude (main)", "shell", "codegraph mcp", "claude"]);
        let omp = sessions.iter().find(|s| s.pid == 30).unwrap();
        assert_eq!(omp.parts[1].label, "server-github");
        assert_eq!(omp.pane_id, None);
        assert_eq!(
            orphans.len(),
            1,
            "a plain node service is not an MCP orphan"
        );
        assert_eq!(orphans[0].pid, 40);
        assert_eq!(orphans[0].kib, 81);
        let pids: Vec<i32> = orphans[0].procs.iter().map(|(pid, _)| *pid).collect();
        assert_eq!(pids, [40, 41]);
        assert_eq!(orphans[0].label, "codegraph mcp");
    }

    #[test]
    fn transcripts_are_tallied_as_they_grow() {
        let dir = env::temp_dir().join(format!("sysi-tally-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let reply = |id: &str, input: u64, read: u64| {
            format!(
                r#"{{"message":{{"id":"{id}","usage":{{"input_tokens":{input},"cache_read_input_tokens":{read},"cache_creation_input_tokens":0,"output_tokens":5}}}}}}"#
            )
        };
        // Two blocks of one reply, then a line still being written.
        fs::write(
            &path,
            format!(
                "{}\n{}\n{{\"user\":1}}\n{}",
                reply("a", 10, 100),
                reply("a", 10, 100),
                &reply("b", 1, 200)[..20]
            ),
        )
        .unwrap();
        let tallied = tally(Agent::Claude, &path).unwrap();
        assert_eq!((tallied.tokens.total(), tallied.context), (115, 110));
        fs::write(
            &path,
            format!(
                "{}\n{}\n{{\"user\":1}}\n{}\n",
                reply("a", 10, 100),
                reply("a", 10, 100),
                reply("b", 1, 200)
            ),
        )
        .unwrap();
        let tallied = tally(Agent::Claude, &path).unwrap();
        assert_eq!((tallied.tokens.total(), tallied.context), (115 + 206, 201));
        // A compaction takes the context down before the next reply arrives.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        std::io::Write::write_all(&mut file, b"{\"type\":\"system\",\"subtype\":\"compact_boundary\",\"compactMetadata\":{\"preTokens\":201,\"postTokens\":9}}\n").unwrap();
        let tallied = tally(Agent::Claude, &path).unwrap();
        assert_eq!((tallied.tokens.total(), tallied.context), (115 + 206, 9));
        let omp = dir.join("o.jsonl");
        fs::write(&omp, "{\"message\":{\"provider\":\"p\",\"model\":\"m\",\"usage\":{\"input\":3,\"output\":4,\"cacheRead\":50,\"cacheWrite\":7}}}\n{\"type\":\"compaction\",\"tokensBefore\":60,\"tokensAfter\":12}\n").unwrap();
        let tallied = tally(Agent::Omp, &omp).unwrap();
        assert_eq!((tallied.tokens.total(), tallied.context), (64, 12));
        assert_eq!(tallied.model.as_deref(), Some("p/m"));
        assert_eq!(
            resume_arg(&["omp".into(), "--resume=/a.jsonl".into()]).as_deref(),
            Some("/a.jsonl")
        );
        assert_eq!(
            resume_arg(&["claude".into(), "--resume".into(), "id".into()]).as_deref(),
            Some("id")
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn context_limits_follow_the_model() {
        assert_eq!(claude_context_limit("claude-opus-5-5"), 1_000_000);
        assert_eq!(claude_context_limit("claude-sonnet-4-6"), 1_000_000);
        assert_eq!(claude_context_limit("claude-haiku-4-5"), 200_000);
        assert_eq!(claude_context_limit("claude-3-7-sonnet"), 200_000);
        let catalog = parse_omp_catalog(&json!({"models": [
            {"provider": "google-antigravity", "id": "gemini-3.8-flash", "contextWindow": 1048576},
            {"provider": "fpt", "id": "x", "selector": "fpt/x", "contextWindow": 500000},
            {"provider": "fpt", "id": "y"}
        ]}));
        assert_eq!(
            catalog.get("google-antigravity/gemini-3.8-flash"),
            Some(&1_048_576)
        );
        assert_eq!(catalog.get("fpt/x"), Some(&500_000));
        assert_eq!(catalog.len(), 2);
        assert_eq!(
            script_name("/x/node_modules/@scope/server-github/dist/index.js"),
            "server-github"
        );
        assert_eq!(script_name("/home/u/.local/bin/codegraph"), "codegraph");
    }

    #[test]
    fn stat_and_rollup_parse() {
        assert_eq!(
            parse_stat("123 (Web (Content)) S 45 123 0"),
            Some(("Web (Content)".into(), 45))
        );
        assert_eq!(
            parse_rollup(
                "Rss: 900 kB\nPss: 100 kB\nPss_Anon: 80 kB\nSwap: 70 kB\nSwapPss: 50 kB\n"
            ),
            150
        );
    }

    #[test]
    fn herdr_panes_carry_their_workspace_and_session() {
        let panes = json!({"result": {"panes": [
            {"pane_id": "wE:p4", "workspace_id": "wE", "agent_status": "working",
             "terminal_title_stripped": " Practice routine ",
             "agent_session": {"value": "8c57"}},
            {"pane_id": "wE:p5", "workspace_id": "wE", "agent_status": "idle",
             "terminal_title_stripped": ""},
            {"pane_id": "wE:p6", "workspace_id": "wE",
             "terminal_title_stripped": "π > 🇬🇧 ENG: Learn"}
        ]}});
        let workspaces = json!({"result": {"workspaces": [
            {"workspace_id": "wE", "label": "aptis"}
        ]}});
        let (panes, order) = parse_herdr(&panes, &workspaces);
        assert_eq!(order, ["aptis"]);
        let busy = &panes["wE:p4"];
        assert_eq!(busy.workspace, "aptis");
        assert_eq!(busy.title.as_deref(), Some("Practice routine"));
        assert!(busy.working);
        assert_eq!(busy.session.as_deref(), Some("8c57"));
        assert_eq!(panes["wE:p5"].title, None);
        assert_eq!(panes["wE:p6"].title.as_deref(), Some("🇬🇧 ENG: Learn"));
        assert!(!panes["wE:p5"].working);
    }

    #[test]
    fn resume_commands_quote_what_needs_it() {
        let mut session = Session {
            agent: Agent::Claude,
            pid: 2,
            cwd: "/home/u/my work".into(),
            pane_id: None,
            pane: Some(Pane::default()),
            session: Some("8c57".into()),
            active_ms: Some(1_000),
            context: None,
            context_limit: None,
            model: None,
            tokens: None,
            kib: 0,
            parts: Vec::new(),
            pids: Vec::new(),
        };
        assert_eq!(
            session.resume_command().as_deref(),
            Some("cd '/home/u/my work' && claude --resume 8c57")
        );
        assert_eq!(session.idle_ms(61_000), Some(60_000));
        // Outside herdr nobody says whether it is busy.
        let pane = session.pane.take();
        assert_eq!(session.idle_ms(61_000), None);
        assert_eq!(session.quiet_ms(61_000), Some(60_000));
        session.pane = pane;
        session.agent = Agent::Omp;
        session.cwd = "/tmp".into();
        session.session = Some("/s/a b.jsonl".into());
        assert_eq!(
            session.resume_command().as_deref(),
            Some("cd /tmp && omp --resume='/s/a b.jsonl'")
        );
        session.pane.as_mut().unwrap().working = true;
        assert_eq!(session.idle_ms(61_000), None);
        session.active_ms = None;
        assert_eq!(session.resume_command(), None, "nothing written yet");
    }
}
