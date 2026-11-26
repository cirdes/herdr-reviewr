# Plan: run reviewr inside Rex (Send first, then Last Turn)

Status: draft for review. Author: Cirdes (with Claude). Date: 2026-11-26.

## Goal

reviewr (this repo, `persiyanov/herdr-reviewr`, v0.52.2) works fully only inside herdr. Outside it,
"everything but **Send** and the **last turn** scope works" (README). The user moved from herdr to
**Rex** (Superlogical's terminal multiplexer, `rex` CLI 0.1.0-preview.25, Mac app 0.1.0-preview.35)
and needs reviewr there, **Send** above all: paste the review's comments, unsubmitted, into the
input of the coding agent (Claude Code) running in a neighbouring pane, then focus that pane.

Success means, inside Rex, with no herdr running:

1. `s` sends every comment to the only agent in the session, or opens the picker when there are
   several; `S` always opens the picker. Same refusals as today: agent at a prompt (`blocked`),
   no agent, review over the size cap, agent pane gone.
2. The `last turn` scope fills, as it does under herdr.
3. A key in Rex (`⌘R`) toggles a reviewr pane split to the right of the focused pane, in its folder.
4. herdr behaviour is unchanged; the herdr test suite stays green.
5. Later, upstreamable: one concern per PR, `just ci` green, changelog bullet (CONTRIBUTING.md).

Non-goals: Windows support for the Rex path; herdr-plugin features with no Rex counterpart
(plugin state dir, managed plugin dirs, `HERDR_PLUGIN_*`); patching the Rex app.

## How reviewr talks to herdr today

All herdr coupling lives in `src/herdr.rs` (CLI calls via `$HERDR_BIN_PATH`, the send over
`$HERDR_SOCKET_PATH`) and `src/herdr_socket.rs` (a long-lived socket connection). The rest of the
app touches it through these seams:

| Seam | Where used | What it does |
|---|---|---|
| `herdr::PaneIds::from_env()` (`HERDR_WORKSPACE_ID`, `HERDR_PANE_ID`) | `app.rs:901`, `actions.rs:296` | This pane's identity |
| `herdr::send_target(&ids, choose) -> Result<SendTarget, SendError>` | `app.rs:5104` | `herdr agent list`, keeps the workspace's agent panes other than ours; `One` or `Many` (picker) |
| `herdr::send_text(pane, text)` | `export.rs:202` | Re-reads the agent list (`ensure_ready`: refuse when `blocked`, `PaneGone` when absent), then `pane.send_text` over the socket with the text wrapped as one bracketed paste (`pasted()`), cap 256 KiB |
| `herdr::focus(pane)` | `export.rs:204` | `herdr agent focus <pane>` after a send |
| `herdr::label_pane` / `clear_pane_label` | `lib.rs:81,84` | Cosmetic pane label "reviewr" |
| `herdr_socket::Connection::start(socket, pane, repo, tx)` | `lib.rs:917-927` | Pushes `HerdrEvent::{Session{me, visible}, Turn(TurnReport), Lost, TooOld}` and owns the `TurnHost` fed with `AgentSample{cwd, workspace, status}` per agent; `feed()` gives `watch.rs` a `TurnFeed` for worktree writes |
| `actions.rs` (`toggle`/`open`/`close`, `open_plugin_pane`, `focus_tab`, `rename_tab`, `claim_geometry`) | herdr plugin actions | Opening and closing the reviewr pane from a herdr key |
| `herdr::plugin_config_dir` / `HERDR_PLUGIN_CONFIG_DIR` | `config.rs:332,733`, `lib.rs:75` | Where config lives; already falls back to the CLI's dir when unset |

Error types: `HerdrError::{Unanswered, Refused(code), Unreadable, PaneGone}`,
`SendError::{Herdr(_), AtPrompt, NoAgent, TooLarge}`; `export::send_failure` turns them into UI text.

Turn tracking (`src/turn.rs`) is herdr-agnostic: it folds every agent's `Status` (`idle`,
`working`, `blocked`, `done`, else `Unknown`) per worktree and takes a working-tree snapshot on the
rest→work edge. It only needs status samples and their timing.

## What Rex offers (verified on this machine)

Rex model: server ⊃ **session** (≈ herdr workspace) ⊃ **window** (tab) ⊃ **block** (pane). Every
terminal Rex starts gets `REX_BLOCK` (full block id) and `REX_SERVER`; `rex` finds its session from
`REX_BLOCK`.

| Need | Rex | Verified |
|---|---|---|
| This pane's ids | `REX_BLOCK`; `rex whoami --json` → `session_id`, `window_id` | yes |
| List panes of the session | `rex block ls --json` → `blocks[]{block_id, window_id, label, flavor, placement}` | yes |
| Agent detection + status | `rex block call -b B program_status` → `records[]{state, app:"claude-code", msg, updated_at, owner{pid,name}}`, states `idle/working/blocked/done/error` (OSC 7501; Claude Code ≥2.1.295 emits it) | yes, on a live Claude Code block |
| Agent's folder | `rex block call -b B process` → `foreground{cwd, path, argv0, ...}` | yes |
| Paste text | `rex send -b B TEXT` ("enters the text exactly as provided"); or block method `write` (bytes as typed) | `rex send` documented; bracketed-paste passthrough **not yet verified** |
| Focus a pane | `rex focus B` (also switches window) | documented |
| Label a pane | server method `session.set_block_label` | documented |
| Status changes, pushed | `rex events -s SESSION --json` → NDJSON incl. `terminal.program_status_changed {id, record, reason}`, `terminal.program_status_removed`, `session_view_changed`, `terminal.process_changed` | event names documented; **JSON envelope (block id field) not yet captured** |
| Which window/session is visible | client-side; `session.inspect` has `active_window_id`; app focus is not exposed as an event we have confirmed | partial |
| Open a split for reviewr | `rex split -b B --split=right -c DIR -- reviewr` or Lua `rex.session.new_split` from an `init.lua` action bound to `cmd+r` | yes (splits used elsewhere) |

Exit codes of `rex`: 0 ok, 1 failed, 2 bad usage, 3 target missing/ambiguous, 4 server unreachable.

## Design

Introduce a host abstraction chosen once at startup:

- herdr when `HERDR_PANE_ID` (or `HERDR_SOCKET_PATH`) is set — today's code path, untouched;
- Rex when `REX_BLOCK` is set and herdr is not;
- none otherwise (today's standalone behaviour).

Keep the public shapes the app already consumes (`PaneIds`, `AgentChoice`, `SendTarget`,
`SendError`, `HerdrEvent`, `AgentSample`) so `app.rs`, `export.rs`, `ui.rs`, `turn.rs` change only at
the seam. Two options for the seam:

- **A. Dispatch inside `herdr.rs`** functions (`send_target`, `send_text`, `focus`, `label_pane`)
  on the detected host, calling into a new `src/rex.rs`. Smallest diff; names stay herdr-ish.
- **B. A `Host` trait** (`agents()`, `send_text()`, `focus()`, `label()`, `connect()`) with herdr and
  Rex impls. Cleaner, bigger diff, more for the maintainer to review.

Proposal: **A** for the first PR (one concern, minimal churn), mention **B** in the issue.

### Phase 0 — toolchain and baseline

`brew install rustup just` (added to the user's dotfiles Brewfile); `rustup` installs the pinned
1.99.1 on first build. `just ci` on an untouched checkout to record the baseline.

### Phase 1 — Send in Rex (the PR the user cares most about)

New `src/rex.rs`:

- `rex_bin()`: `$REX_BIN_PATH` else `rex` on `PATH`; every call with `--timeout` and the same
  `CALL_BOUND` style deadline used for herdr.
- `agents(session)`: `rex block ls --json`, then per terminal block `program_status` (+ `process`
  for cwd). A block is an agent when a status record has an `app` (map `claude-code`→`claude`,
  `codex`, `opencode`, `pi`; unknown apps pass through). Our own block (`REX_BLOCK`) is excluded.
  Window labels (`rex window ls --json`) fill the picker's tab column. N blocks ⇒ 1 + 2N `rex`
  processes; acceptable for a keypress (measure; parallelise if > 300 ms).
- Map Rex `state` → `Status::from_wire` (same spellings; `error` folds to `Unknown`).
- `send_text(block, text)`: re-read that block's status (`ensure_ready` semantics: `blocked` ⇒
  `AtPrompt`, no status record / block gone ⇒ `PaneGone`), then write the existing `pasted(text)`
  bytes with `rex send -b B` (text via argv; or via stdin if argv size limits bite — cap stays
  256 KiB). Never send Enter.
- `focus(block)`: `rex focus B`.
- `label_pane`: `session.set_block_label` via `rex api call`; best effort.
- Errors map onto existing `HerdrError` variants (exit 3 ⇒ `PaneGone`, 4 ⇒ `Unanswered`, other ⇒
  `Refused(None)`, bad JSON ⇒ `Unreadable`), so `export::send_failure` needs at most wording tweaks
  ("herdr" → host name).
- Tests beside the code with recorded JSON from this machine, named as sentences, mirroring the
  herdr ones (picker excludes own pane, refuses at prompt, gone pane, unknown app passthrough).

Manual check: two-pane Rex window, Claude Code left, reviewr right; comment, `s`; text appears in
Claude's input unsubmitted, focus moves there. Repeat with Claude `blocked` (refusal) and with two
agents (picker).

### Phase 2 — Last Turn and visibility in Rex

New `RexConnection` producing the same `HerdrEvent`s:

- Spawn `rex events -s <session> --json`; on `terminal.program_status_changed` /
  `program_status_removed` / `session_view_changed`, resample agents (Phase 1 code) and feed the
  existing `TurnHost` with `AgentSample{cwd, workspace: session_id, status}` and the event time.
- `Session{me, visible}`: `me` from `REX_BLOCK`; `visible` best effort (active window of our session
  equals ours); if not derivable, always `true` — the same fallback reviewr uses on `Lost`.
- Process exit / parse failure ⇒ `HerdrEvent::Lost`, restart with the existing `Backoff`.
- `feed()` returns the same `TurnFeed` so `watch.rs` keeps reporting worktree writes.

### Phase 3 — toggle key (user config, not upstream)

`~/.config/rex/init.lua` action `reviewr_toggle` bound to `cmd+r`: if the focused window holds a
block labelled `reviewr`, close it; else `new_split` right of the focused block, cwd from its
`process.foreground.cwd`, command `~/.local/bin/herdr-reviewr`. (Rex `cmd+r` is "Refresh Sessions"
by default; the binding overrides it.)

### Phase 4 — upstream

Only with the user's explicit go-ahead: open an issue first asking whether a non-herdr host is
welcome (the project is named *herdr*-reviewr), then fork under the user's account and send
PR 1 (Send in Rex) and later PR 2 (Last Turn in Rex), each with changelog and `just ci`.

## Open questions / risks

1. Does `rex send` deliver `ESC[200~…ESC[201~` untouched so Claude Code sees one paste, and does a
   256 KiB argv work? Fallback: block method `write` with the bytes, or stdin.
2. Exact JSON of `rex events --json` for status events (which field carries the block id).
3. `visible`: is there a reliable signal for "this window is on screen" in Rex? If not, always-true
   costs some background work only.
4. Rex is in preview; CLI/API may change. Pin behaviour behind small parsing functions with tests.
5. Maintainer may decline non-herdr support; then the fork is maintained by the user.
6. Several agents in one Rex session across windows: picker shows window labels; is that enough?
7. Detection precedence if both `HERDR_*` and `REX_BLOCK` are set (herdr inside a Rex terminal):
   herdr wins, since its ids are the more specific.

---

## Revision 2 (after EGM review + spike, 2026-11-26)

This section supersedes the Design and Phase 1–2 details above where they conflict.

### Spike results (run on Rex 0.1.0-preview.25 / app preview.35, Claude Code 2.1.332)

| Question | Result |
|---|---|
| Paste route | **`block.event {type:"paste", payload:{text}}`** delivers one paste: Rex adds the bracketed-paste markers itself when the program enabled mode 2004. Claude Code shows `[Pasted text #1 +8 lines]`, unsubmitted, status stays `idle`. `rex send` (argv or stdin) and the terminal `write` method also pass hand-made `ESC[200~…ESC[201~` through untouched and Claude reads them as one paste. **Chosen: the paste event** (no hand-made markers, no argv, payload is JSON over stdin via `rex api call ... -`). |
| Exit codes | missing block → **3** (both `rex send` and `rex api call block.event`); unreachable server → **4**. |
| Event envelope | `{"type":"session_event","payload":{"session_id":…,"event":{"type":"block_event","payload":{"block_id":…,"name":"program_status_changed","payload":{"id":"","record":{"state":"working","app":"claude-code","updated_at":"…"},"reason":"report"}}}}}` — sample in `docs/rex-events-sample.ndjson`. A Claude turn went `working` → `done` in ~1 s. |
| Visibility | `active_window_changed {window_id}` exists in the session event stream. |

### Changes adopted from the review

- **Seam:** a closed `enum Host { Herdr, Rex, None }` in `src/host.rs`, chosen once at startup (herdr wins when `HERDR_PANE_ID` is set; Rex when `REX_BLOCK` is set), `match` at the call sites (`app.rs` send target, `export.rs` send + focus, `lib.rs` label + connection). `herdr.rs` stays unchanged; new `src/rex.rs` + `src/rex_events.rs`.
- **Ids:** `PaneIds` for Rex = `{workspace: session_id, pane: REX_BLOCK}` plus `window_id`, from `rex whoami --json` at startup; refreshed on `session_view_changed`.
- **Targeting:** reuse `target_among` semantics with the window in place of the tab: the only agent in reviewr's own window gets `s` directly, else the picker; `S` always picks.
- **Errors:** a `RexError` with its own user-facing strings (no "herdr didn't answer" inside Rex); `SendError` gains a host-neutral variant or is generic over the host error.
- **Runner:** extend `proc` to return the exit code and accept stdin (`RunError::Failed` keeps `code`).
- **Lookup:** `rex block ls --json` + `program_status` per terminal block, in parallel threads, one ~2 s overall deadline; drop the `process` call (cwd only needed for turn tracking, taken from the event stream's `process_changed`/a lazy read); window labels only when the picker opens.
- **Status:** one record per block (the agent's: has `app`); `error` folds to resting so a turn can end.
- **Turn tracking:** fold `record.state` straight from each `program_status_changed` line, stamped at arrival (keeps `RACE_BEFORE` and the `heard`/`gap()` rules); resample the block list only on `program_status_removed`, `session_view_changed`, `terminal.process_changed`. Unparseable line ⇒ skip + log, not `Lost`; child process killed on `Drop`; stream end ⇒ `Lost` + `Backoff`.
- **Visibility:** `visible = active_window_id == our window`, from the initial view and `active_window_changed`.
- **Test isolation (PR 1, mandatory):** `justfile` test/ci, the AGENTS.md single-test command and `scripts/idle_check.py` also unset `REX_BLOCK`/`REX_SERVER`/`REX_SESSION` and set `REX_BIN_PATH=false`, so no test can reach a live pane. A `fake_rex` helper mirrors `fake_herdr` for integration tests.
- **Upstream:** issue first (ask whether a non-herdr host is welcome; offer the enum, or a host-neutral `send_command` config as a smaller alternative). One concern per PR: PR 1 Send, PR 2 Last Turn.

### Order

1. ✅ `⌘R` toggle in the user's Rex config (`dotfiles/rex/reviewr.lua`, uses the current release binary).
2. ✅ Spike (above).
3. Toolchain (`rustup`, `just`) + baseline `just ci`.
4. Issue upstream (user's go-ahead required).
5. PR 1: Send in Rex. 6. PR 2: Last Turn in Rex.
