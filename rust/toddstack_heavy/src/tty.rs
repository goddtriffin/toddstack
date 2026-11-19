//! Terminal ownership. The child runs in its own process group, so for a job that reads the terminal (an
//! interactive prompt, a pager, ssh asking for a password) the child's group must be the terminal's
//! foreground group while it runs, and heavy's group gets it back afterwards.

use std::io::{IsTerminal, stdin};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use nix::unistd::{Pid, getpgrp, tcgetpgrp, tcsetpgrp};
use signal_hook::consts::SIGTTOU;

/// Where the terminal goes: `child` while the job runs, `own` (heavy's group) the rest of the time.
#[derive(Debug, Clone, Copy)]
pub struct Terminal {
    own: Pid,
    child: Pid,
}

impl Terminal {
    /// Hands the terminal to `child`'s group, but only when stdin is a terminal and heavy's own group is
    /// its foreground group right now (a heavy started in the background must not steal the terminal).
    /// SIGTTOU is caught (not ignored, so children keep the default), because `tcsetpgrp` from a group that
    /// is about to lose the foreground would otherwise stop heavy.
    #[must_use]
    pub fn claim(child: Pid) -> Option<Self> {
        let input = stdin();
        if !input.is_terminal() {
            return None;
        }
        let own = getpgrp();
        if tcgetpgrp(&input).ok()? != own {
            return None;
        }
        // The handler only sets a flag; it exists so that SIGTTOU no longer stops this process.
        signal_hook::flag::register(SIGTTOU, Arc::new(AtomicBool::new(false))).ok()?;
        let term = Self { own, child };
        term.give_to_child();
        Some(term)
    }

    pub fn give_to_child(self) {
        let _ = tcsetpgrp(stdin(), self.child);
    }

    pub fn take_back(self) {
        let _ = tcsetpgrp(stdin(), self.own);
    }
}

/// Returns the terminal to heavy's group when dropped.
pub struct TerminalGuard(pub Option<Terminal>);

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            t.take_back();
        }
    }
}
