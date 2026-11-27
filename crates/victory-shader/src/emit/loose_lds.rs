//! The loose-LDS fallback (fork PR #799 by Massive-Adrian): console wave64 programs
//! that read and write LDS inside one lane-branch region, run as two host waves of
//! 32 that meet only where such a region is entered and left.
//!
//! Everything from that port, and the wave64 `ds_bpermute_b32` half fix, is off
//! unless `VICTORY_WAVE_LDS_LOOSE=1` (it joins the shader cache key).

thread_local! {
    static LDS_LOOSE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    #[cfg(test)]
    static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether `VICTORY_WAVE_LDS_LOOSE=1` is set: the switch for this whole port.
pub(crate) fn enabled() -> bool {
    #[cfg(test)]
    if FORCED.get() {
        return true;
    }
    std::env::var_os("VICTORY_WAVE_LDS_LOOSE").is_some_and(|v| v == "1")
}

/// Tests: runs `f` with the switch on for this thread (no process-wide env change).
#[cfg(test)]
pub(crate) fn force<R>(f: impl FnOnce() -> R) -> R {
    let was = FORCED.replace(true);
    let r = f();
    FORCED.set(was);
    r
}

/// Whether the lowering runs inside [`with`].
pub(super) fn on() -> bool {
    LDS_LOOSE.get()
}

/// Runs `f` with LDS reads and writes inside one lane-branch region accepted
/// (`lower::lds_meetings`): the two host waves of a console wave meet only where
/// such a region is entered and left. APPROXIMATE: a value one half writes and
/// the other half reads inside the region may be read stale. The last resort
/// for console wave64 programs a 32-lane host would otherwise refuse.
pub(super) fn with<R>(f: impl FnOnce() -> R) -> R {
    let was = LDS_LOOSE.replace(true);
    let r = f();
    LDS_LOOSE.set(was);
    r
}
