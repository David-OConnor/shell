//! For handling user input like key commands.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, RepeatCount};
use shell::state::{HistoryItem, RecentDir};

use crate::{CliNav, NavAxis, render};

/// Rustyline key handler bound to one of the four arrow keys.
pub(crate) struct ArrowHandler {
    pub(crate) history: Arc<Mutex<Vec<HistoryItem>>>,
    pub(crate) recent_dirs: Arc<Mutex<Vec<RecentDir>>>,
    pub(crate) home: Option<PathBuf>,
    pub(crate) nav: Arc<Mutex<CliNav>>,
    pub(crate) axis: NavAxis,
    /// Direction: Up / Left: `true` (older); Down / Right:`false`.
    pub(crate) backward: bool,
}

impl ConditionalEventHandler for ArrowHandler {
    fn handle(
        &self,
        _evt: &Event,
        _n: RepeatCount,
        _positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        let mut nav = self.nav.lock().ok()?;
        match self.axis {
            NavAxis::His => {
                let history = self.history.lock().ok()?;
                match nav.nav.step_his(&history, self.backward, ctx.line()) {
                    Some(text) => {
                        nav.pending_restart = Some(text);
                        Some(Cmd::Interrupt)
                    }
                    None => Some(Cmd::Noop),
                }
            }
            NavAxis::Cd => {
                // Preserve normal cursor movement when the user is editing.
                if !ctx.line().is_empty() && nav.nav.cd_cursor.is_none() {
                    return None;
                }
                let recent = self.recent_dirs.lock().ok()?;
                let home = self.home.clone();
                let result = nav.nav.step_cd(&recent, self.backward, ctx.line(), |path| {
                    format!("cd {}", render::render_with_tilde(path, home.as_deref()))
                });
                match result {
                    Some(text) => {
                        nav.pending_restart = Some(text);
                        Some(Cmd::Interrupt)
                    }
                    None => Some(Cmd::Noop),
                }
            }
        }
    }
}
