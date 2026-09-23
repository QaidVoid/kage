//! Print-mode SIGINT handling. The first Ctrl-C flips a flag that the
//! event callback bridges into the loop's [`CancelFlag`], so the run
//! unwinds at the loop's next safe point and the session file closes
//! cleanly. A second Ctrl-C only sets the same flag again: while the
//! loop is blocked inside a provider read that never yields, Ctrl-C
//! cannot interrupt it (use Ctrl-\\ to hard-kill). Tradeoff accepted
//! because the workspace forbids `unsafe`, ruling out a raw handler
//! that restores the default disposition on the second signal.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::sync::OnceLock;

fn flag() -> &'static Arc<AtomicBool> {
    static FLAG: OnceLock<Arc<AtomicBool>> = OnceLock::new();
    FLAG.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// Install the SIGINT flag handler. Returns an error message when the
/// OS refuses; callers should warn and continue (the default
/// kill-on-Ctrl-C disposition stays in effect then).
pub(crate) fn install() -> Result<(), String> {
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(flag()))
        .map(|_| ())
        .map_err(|e| format!("register SIGINT handler: {e}"))
}

/// Whether SIGINT has been delivered since [`install`].
pub(crate) fn requested() -> bool {
    flag().load(Ordering::SeqCst)
}
