//! The multiplexer reviewr runs in, picked once: herdr, Rex, or neither.

use std::sync::OnceLock;

use crate::herdr::{self, PaneIds, SendError, SendTarget, var};
use crate::rex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Host {
    Herdr,
    Rex,
    /// Neither: the send takes herdr's path, which reports no agent, as before.
    Standalone,
}

impl Host {
    /// herdr's ids win, since herdr can run inside a Rex terminal; then Rex's block.
    fn detect(herdr_pane: Option<String>, rex_block: Option<String>) -> Self {
        match (herdr_pane, rex_block) {
            (Some(_), _) => Self::Herdr,
            (None, Some(_)) => Self::Rex,
            (None, None) => Self::Standalone,
        }
    }
}

/// The host this process runs in, read from its environment once.
pub fn current() -> Host {
    static HOST: OnceLock<Host> = OnceLock::new();
    *HOST.get_or_init(|| Host::detect(var("HERDR_PANE_ID"), var("REX_BLOCK")))
}

/// Where `Send` goes: one agent, or the picker's rows.
pub fn send_target(ids: &PaneIds, choose: bool) -> Result<SendTarget, SendError> {
    match current() {
        Host::Rex => rex::send_target(choose),
        Host::Herdr | Host::Standalone => herdr::send_target(ids, choose),
    }
}

/// Paste the review into the agent pane's input, unsubmitted.
pub fn send_text(pane: &str, text: &str) -> Result<(), SendError> {
    match current() {
        Host::Rex => rex::send_text(pane, text),
        Host::Herdr | Host::Standalone => herdr::send_text(pane, text),
    }
}

/// Focus the agent pane after a send; best effort.
pub fn focus(pane: &str) {
    let _ = match current() {
        Host::Rex => rex::focus(pane).map_err(drop),
        Host::Herdr | Host::Standalone => herdr::focus(pane).map_err(drop),
    };
}

#[cfg(test)]
mod tests {
    use super::Host;

    #[test]
    fn herdr_inside_a_rex_terminal_is_herdr_and_a_bare_rex_block_is_rex() {
        let some = |s: &str| Some(s.to_string());
        assert_eq!(Host::detect(some("w1:p1"), some("block:1")), Host::Herdr);
        assert_eq!(Host::detect(None, some("block:1")), Host::Rex);
        assert_eq!(Host::detect(None, None), Host::Standalone);
    }
}
