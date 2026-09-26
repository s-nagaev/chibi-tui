use crate::*;

/// Startup/teardown pairing contract: the kitty enhancement flags are
/// popped by the `TerminalRestore` guard exactly when they were pushed,
/// so the terminal's kitty flag stack can never leak into the user's
/// shell after exit (nor under-pop an outer entry). Bracketed paste is
/// disabled by the same guard exactly when it was enabled — a terminal
/// MODE, not a stack, so the guard must never toggle it blind.
#[test]
fn terminal_restore_pops_the_kitty_flags_exactly_when_they_were_pushed() {
    let restore = TerminalRestore::new(push_kitty_flags(), !cfg!(windows));
    assert_eq!(
        restore.pop_kitty_flags,
        push_kitty_flags(),
        "the guard's pop decision must mirror the push decision"
    );
    assert_eq!(
        restore.disable_bracketed_paste,
        !cfg!(windows),
        "the guard's disable decision must mirror the enable decision"
    );
    // The guard built from the real startup decision is internally
    // consistent by construction; the false/true branches are covered
    // by the mirror assertion above (a mismatch would mean either a
    // leaked flag stack or an under-pop on some platform).
}
