//! Rex integration: the agent panes of this session, the send, and the focus, via the `rex` CLI.

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::herdr::{AgentChoice, SendError, SendTarget, var};
use crate::turn::Status;

/// How one `rex` call failed, read from its exit status.
#[derive(Debug, PartialEq, Eq)]
pub enum RexError {
    /// rex gave no answer: not run, server unreachable (exit 4), or past the bound.
    Unanswered,
    /// rex exited non-zero for another reason.
    Refused,
    /// rex exited 0 without the shape the call documents.
    Unreadable,
    /// The addressed block no longer exists (exit 3).
    PaneGone,
}

impl fmt::Display for RexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unanswered => write!(f, "rex gave no answer"),
            Self::Refused => write!(f, "rex refused the call"),
            Self::Unreadable => write!(f, "rex's answer was unreadable"),
            Self::PaneGone => write!(f, "the pane is gone"),
        }
    }
}

impl std::error::Error for RexError {}

/// How long one rex call may run.
const CALL_BOUND: Duration = Duration::from_secs(4);
/// The largest review a send pastes, as herdr's cap.
const MAX_PASTE_BYTES: usize = 256 * 1024;
/// rex's exit status for a target that is missing or ambiguous.
const EXIT_TARGET_MISSING: i32 = 3;
/// rex's exit status for a server it cannot reach.
const EXIT_UNREACHABLE: i32 = 4;

/// This terminal's block, which Rex sets in every terminal it starts.
pub fn my_block() -> Option<String> {
    var("REX_BLOCK")
}

/// The rex binary `$REX_BIN_PATH` names, else `rex` on `PATH`.
fn rex_bin() -> String {
    var("REX_BIN_PATH").unwrap_or_else(|| "rex".into())
}

/// Run a rex subcommand, `input` on its stdin: its stdout, or the classified failure.
fn call(args: &[&str], input: Option<Vec<u8>>) -> Result<String, RexError> {
    use crate::proc::RunError;
    let mut cmd = crate::proc::command(rex_bin());
    cmd.args(args);
    let deadline = Instant::now() + CALL_BOUND;
    match crate::proc::run_tree_fed(cmd, input, || Instant::now() >= deadline) {
        Ok(stdout) => Ok(stdout),
        Err(RunError::Failed { stderr, code }) => {
            logln!("rex {args:?} failed ({code:?}): {}", stderr.trim());
            Err(match code {
                Some(EXIT_TARGET_MISSING) => RexError::PaneGone,
                Some(EXIT_UNREACHABLE) => RexError::Unanswered,
                _ => RexError::Refused,
            })
        }
        Err(RunError::Stopped) => {
            logln!("rex {args:?} unanswered after {CALL_BOUND:?}");
            Err(RexError::Unanswered)
        }
        Err(error) => {
            logln!("rex {args:?} could not run: {error:?}");
            Err(RexError::Unanswered)
        }
    }
}

fn parse<T: serde::de::DeserializeOwned>(json: &str) -> Result<T, RexError> {
    serde_json::from_str(json).map_err(|error| {
        logln!("rex answer unreadable: {error}");
        RexError::Unreadable
    })
}

/// One `rex block ls --json` entry.
#[derive(Debug, Deserialize)]
struct Block {
    #[serde(rename = "block_id")]
    id: String,
    #[serde(default)]
    window_id: Option<String>,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    creator_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BlockList {
    blocks: Vec<Block>,
}

/// One program status record a terminal holds (OSC 7501).
#[derive(Debug, Deserialize)]
struct StatusRecord {
    state: String,
    #[serde(default)]
    app: Option<String>,
}

#[derive(Debug, Deserialize)]
struct StatusList {
    #[serde(default)]
    records: Vec<StatusRecord>,
}

/// The `rex window ls --json` fields the picker reads.
#[derive(Debug, Deserialize)]
struct WindowList {
    #[serde(default)]
    windows: Vec<Window>,
}

#[derive(Debug, Deserialize)]
struct Window {
    window_id: String,
    #[serde(default)]
    label: Option<String>,
}

/// An agent pane in this session: a terminal whose program reports a status with an app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RexAgent {
    pub block: String,
    pub window: Option<String>,
    pub app: String,
    pub state: String,
    pub label: Option<String>,
}

impl RexAgent {
    /// The pane's own label when the user gave one, else the agent's short name.
    fn row_name(&self) -> String {
        match self.label.as_deref() {
            Some(label) if !label.is_empty() && label != "shell" => label.to_owned(),
            _ => short_app(&self.app).to_owned(),
        }
    }

    fn choice(&self, windows: &HashMap<String, String>) -> AgentChoice {
        AgentChoice {
            pane_id: self.block.clone(),
            name: self.row_name(),
            state: self.state.clone(),
            tab: self.window.as_ref().and_then(|w| windows.get(w)).cloned().unwrap_or_default(),
        }
    }
}

/// `claude-code` reads as `claude`; other apps keep their own name.
fn short_app(app: &str) -> &str {
    match app {
        "claude-code" => "claude",
        other => other,
    }
}

/// The agent record among a terminal's status records: the one naming its app.
fn agent_record(list: StatusList) -> Option<StatusRecord> {
    list.records.into_iter().find(|record| record.app.as_deref().is_some_and(|a| !a.is_empty()))
}

/// The session's terminal blocks other than `me`, each read for an agent in parallel.
pub(crate) fn agents(me: Option<&str>) -> Result<(Vec<RexAgent>, Option<String>), RexError> {
    let list: BlockList = parse(&call(&["block", "ls", "--json"], None)?)?;
    let my_window =
        me.and_then(|me| list.blocks.iter().find(|b| b.id == me).and_then(|b| b.window_id.clone()));
    let terminals: Vec<&Block> = list
        .blocks
        .iter()
        .filter(|b| Some(b.id.as_str()) != me)
        .filter(|b| b.creator_name.as_deref().is_none_or(|c| c == "com.superlogical.terminal"))
        .collect();
    let found = std::thread::scope(|scope| {
        let reads: Vec<_> = terminals
            .iter()
            .map(|block| {
                scope.spawn(move || {
                    let json =
                        call(&["block", "call", "-b", &block.id, "program_status"], None).ok()?;
                    let record = agent_record(serde_json::from_str(&json).ok()?)?;
                    Some(RexAgent {
                        block: block.id.clone(),
                        window: block.window_id.clone(),
                        app: record.app.unwrap_or_default(),
                        state: record.state,
                        label: block.label.clone(),
                    })
                })
            })
            .collect();
        reads.into_iter().filter_map(|read| read.join().ok().flatten()).collect()
    });
    Ok((found, my_window))
}

/// Window id → label for this session; best effort, never failing the send.
fn window_labels() -> HashMap<String, String> {
    call(&["window", "ls", "--json"], None)
        .ok()
        .and_then(|json| serde_json::from_str::<WindowList>(&json).ok())
        .map(|list| {
            list.windows.into_iter().filter_map(|w| Some((w.window_id, w.label?))).collect()
        })
        .unwrap_or_default()
}

/// The lone agent, or the lone one in reviewr's window, unless `choose`; else the picker.
pub fn send_target(choose: bool) -> Result<SendTarget, SendError> {
    let me = my_block();
    let (agents, my_window) = agents(me.as_deref()).map_err(SendError::Rex)?;
    if agents.is_empty() {
        return Err(SendError::NoAgent);
    }
    let shows_picker = choose || agents.len() > 1;
    let windows = if shows_picker { window_labels() } else { HashMap::new() };
    Ok(target_among(&agents, my_window.as_deref(), &windows, choose))
}

fn target_among(
    agents: &[RexAgent],
    my_window: Option<&str>,
    windows: &HashMap<String, String>,
    choose: bool,
) -> SendTarget {
    let mut beside =
        agents.iter().filter(|a| a.window.is_some() && a.window.as_deref() == my_window);
    let lone = match agents {
        [only] => Some(only),
        _ => match (beside.next(), beside.next()) {
            (Some(agent), None) => Some(agent),
            _ => None,
        },
    };
    match lone.filter(|_| !choose) {
        Some(agent) => SendTarget::One(agent.choice(windows)),
        None => SendTarget::Many(agents.iter().map(|a| a.choice(windows)).collect()),
    }
}

/// Refuse a send to an agent at a prompt, read fresh: a prompt drops a paste.
fn ensure_ready(block: &str) -> Result<(), SendError> {
    let json =
        call(&["block", "call", "-b", block, "program_status"], None).map_err(SendError::Rex)?;
    let list: StatusList = parse(&json).map_err(SendError::Rex)?;
    match agent_record(list) {
        None => {
            logln!("rex block {block} holds no agent any more");
            Err(SendError::Rex(RexError::PaneGone))
        }
        Some(record) if Status::from_wire(&record.state) == Status::Blocked => {
            Err(SendError::AtPrompt)
        }
        Some(_) => Ok(()),
    }
}

/// The bracketed-paste end marker, which inside the text would end the paste early.
const PASTE_END: &str = "\x1b[201~";

/// `text` with every inner paste terminator stripped in one pass, as herdr's send does.
fn unterminated(text: &str) -> String {
    let mut body = String::with_capacity(text.len());
    for ch in text.chars() {
        body.push(ch);
        if body.ends_with(PASTE_END) {
            body.truncate(body.len() - PASTE_END.len());
        }
    }
    body
}

/// The `block.event` request that pastes `text` into `block`, as one paste and unsubmitted.
fn paste_request(block: &str, text: &str) -> String {
    let text = unterminated(text);
    serde_json::json!({
        "block_id": block,
        "event": {"type": "paste", "payload": {"text": text}},
    })
    .to_string()
}

/// Paste `text` into the agent's input; Rex brackets it as the program asked.
pub fn send_text(block: &str, text: &str) -> Result<(), SendError> {
    if text.len() > MAX_PASTE_BYTES {
        return Err(SendError::TooLarge);
    }
    ensure_ready(block)?;
    let request = paste_request(block, text).into_bytes();
    // A full block id names its session too, and the request goes on stdin, never argv.
    call(&["api", "call", "-s", block, "block.event", "-"], Some(request))
        .map(drop)
        .map_err(SendError::Rex)
}

/// Focus the agent's block, switching to its window, so the reviewer can add context and submit.
pub fn focus(block: &str) -> Result<(), RexError> {
    call(&["focus", block], None).map(drop)
}

#[cfg(test)]
mod tests {
    use super::{
        HashMap, RexAgent, StatusList, WindowList, agent_record, paste_request, short_app,
        target_among,
    };
    use crate::herdr::SendTarget;

    fn agent(block: &str, window: &str, state: &str) -> RexAgent {
        RexAgent {
            block: block.into(),
            window: Some(window.into()),
            app: "claude-code".into(),
            state: state.into(),
            label: Some("shell".into()),
        }
    }

    fn picked(target: &SendTarget) -> Vec<&str> {
        match target {
            SendTarget::One(choice) => vec![choice.pane_id.as_str()],
            SendTarget::Many(rows) => rows.iter().map(|row| row.pane_id.as_str()).collect(),
        }
    }

    #[test]
    fn the_only_agent_in_the_session_gets_the_send_wherever_it_sits() {
        let agents = [agent("b1", "w2", "idle")];
        let target = target_among(&agents, Some("w1"), &HashMap::new(), false);
        assert!(
            matches!(target, SendTarget::One(ref c) if c.pane_id == "b1" && c.name == "claude")
        );
    }

    #[test]
    fn the_lone_agent_in_reviewrs_window_wins_over_agents_in_other_windows() {
        let agents = [agent("b1", "w1", "idle"), agent("b2", "w2", "working")];
        assert!(matches!(
            target_among(&agents, Some("w1"), &HashMap::new(), false),
            SendTarget::One(ref c) if c.pane_id == "b1"
        ));
    }

    #[test]
    fn two_agents_beside_reviewr_or_a_chosen_send_open_the_picker() {
        let agents = [agent("b1", "w1", "idle"), agent("b2", "w1", "idle")];
        let target = target_among(&agents, Some("w1"), &HashMap::new(), false);
        assert!(matches!(target, SendTarget::Many(_)));
        assert_eq!(picked(&target), ["b1", "b2"]);
        let lone = [agent("b1", "w1", "idle")];
        assert!(matches!(
            target_among(&lone, Some("w1"), &HashMap::new(), true),
            SendTarget::Many(_)
        ));
    }

    #[test]
    fn picker_rows_carry_the_window_label_and_prefer_a_users_pane_label() {
        let mut named = agent("b2", "w2", "working");
        named.label = Some("api agent".into());
        let agents = [agent("b1", "w1", "idle"), named];
        let windows = HashMap::from([("w1".to_string(), "main".to_string())]);
        let SendTarget::Many(rows) = target_among(&agents, None, &windows, true) else {
            panic!("picker expected")
        };
        assert_eq!((rows[0].name.as_str(), rows[0].tab.as_str()), ("claude", "main"));
        assert_eq!((rows[1].name.as_str(), rows[1].tab.as_str()), ("api agent", ""));
        assert_eq!(rows[1].state, "working");
    }

    #[test]
    fn the_agent_record_is_the_one_naming_its_app() {
        let json = r#"{"records":[{"state":"working"},{"state":"blocked","app":"claude-code","msg":null}]}"#;
        let record = agent_record(serde_json::from_str::<StatusList>(json).unwrap()).unwrap();
        assert_eq!(record.state, "blocked");
        assert!(
            agent_record(serde_json::from_str::<StatusList>(r#"{"records":[]}"#).unwrap())
                .is_none()
        );
        assert!(agent_record(serde_json::from_str::<StatusList>("{}").unwrap()).is_none());
    }

    #[test]
    fn a_paste_is_one_block_event_no_inner_terminator_can_end() {
        let text = "a\nb\x1b[201~c\x1b[20\x1b[201~1~d";
        let request: serde_json::Value =
            serde_json::from_str(&paste_request("block:1", text)).unwrap();
        assert_eq!(request["block_id"], "block:1");
        assert_eq!(request["event"]["type"], "paste");
        assert_eq!(request["event"]["payload"]["text"], "a\nbcd");
    }

    #[test]
    fn window_labels_parse_from_rexs_window_listing() {
        let json = r#"{"session_id":"s","windows":[{"window_id":"w1","label":"main","layers":[]},{"window_id":"w2"}]}"#;
        let list: WindowList = serde_json::from_str(json).unwrap();
        assert_eq!(list.windows.len(), 2);
        assert_eq!(list.windows[0].label.as_deref(), Some("main"));
    }

    #[test]
    fn known_apps_read_by_their_short_name() {
        assert_eq!(short_app("claude-code"), "claude");
        assert_eq!(short_app("codex"), "codex");
    }
}
