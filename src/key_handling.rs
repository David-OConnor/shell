//! For handling user input like key commands.

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};

use rustyline::{Cmd, ConditionalEventHandler, Event, EventContext, Movement, RepeatCount};
use shell::{
    HintCache,
    state::{HistoryItem, RecentDir},
};

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

/// Accepts the greyed suggestion (see [shell::input_hint]) with the caret at
/// the end of the line, as Peregrine does. Bound to → (once `arrow` has passed
/// on it, i.e. when it isn't walking recent dirs), End and Ctrl+E. Anywhere
/// else, passes the key on so it moves the caret as usual.
pub(crate) struct HintHandler {
    pub(crate) hints: Arc<Mutex<HintCache>>,
    pub(crate) arrow: Option<ArrowHandler>,
}

impl ConditionalEventHandler for HintHandler {
    fn handle(
        &self,
        evt: &Event,
        n: RepeatCount,
        positive: bool,
        ctx: &EventContext<'_>,
    ) -> Option<Cmd> {
        if let Some(cmd) = self
            .arrow
            .as_ref()
            .and_then(|a| a.handle(evt, n, positive, ctx))
        {
            return Some(cmd);
        }
        if !ctx.has_hint() || ctx.pos() != ctx.line().len() {
            return None;
        }
        let hints = self.hints.lock().ok()?;
        let hint = hints.last_for(ctx.line())?;
        // One that carries on the line is rustyline's to insert; one that
        // rewrites a word replaces the line.
        Some(if hint.line.starts_with(ctx.line()) {
            Cmd::CompleteHint
        } else {
            Cmd::Replace(Movement::WholeLine, Some(hint.line.clone()))
        })
    }
}
