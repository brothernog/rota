//! Host-memory admission for speculative texture work and reusable upload storage.
//!
//! Adapted from TaynanGT/Vice-Tay at fab5286c (GPL-3.0-or-later), crates/victory-gpu/src/residency.rs;
//! the switch and the hooks follow avatarmanz/VIctory-7900XTX at c9c932cb (`VICTORY_RESIDENCY`).
//! Our hooks differ from theirs (see `crate::draw`, `crate::pool`, `crate::host`).
//!
//! When the PC's free memory or commit runs low this limits what a texture pass prefetches,
//! gives back idle staging blocks, holds off for 500 ms after an `E_OUTOFMEMORY`, and pauses
//! the pass. `VICTORY_RESIDENCY`: unset, on with 32 GiB of RAM or less; `0`, off (every
//! function here then says "no pressure" and nothing changes); `1`, on.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
static PRESSURED: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

fn switch(var: Option<&std::ffi::OsStr>, total_physical: Option<u64>) -> bool {
    match var.and_then(|v| v.to_str()) {
        Some("0") => false,
        Some("1") => true,
        _ => total_physical.is_some_and(|total| total <= 32 * GIB),
    }
}

/// Whether the module acts at all (`VICTORY_RESIDENCY`, see the top).
pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    cfg!(test)
        || *ON.get_or_init(|| {
            switch(std::env::var_os("VICTORY_RESIDENCY").as_deref(), memory_status().map(|s| s.ullTotalPhys))
        })
}

/// Counts for the line in the log: throttled passes, passes paused or
/// slots left due, staging refused, back-offs after `E_OUTOFMEMORY`, bytes
/// of staging given back, times pressure began.
static STATS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
const THROTTLED: usize = 0;
const PAUSED: usize = 1;
const DEFERRED: usize = 2;
const BACKOFFS: usize = 3;
const FREED: usize = 4;
const PRESSURES: usize = 5;

fn count(i: usize, n: u64) {
    STATS[i].fetch_add(n, Ordering::Relaxed);
}

/// Staging a pool gave back because of pressure (called by `crate::pool`).
pub(crate) fn note_freed(bytes: u64) {
    count(FREED, bytes);
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

/// A failed allocation: after `E_OUTOFMEMORY`, uploads wait 500 ms.
pub(crate) fn allocation_error(error: &windows::core::Error) {
    if enabled() && error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        count(BACKOFFS, 1);
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

thread_local! {
    static STAGING_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_staging_deferred() {
    count(DEFERRED, 1);
    STAGING_DEFERRED.set(true);
}

pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

pub(crate) fn upload_refused() -> bool {
    crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    // Growing a fresh 64 MiB upload block for speculative bindless prefetch
    // requires more headroom than reusing slices already committed inside the
    // staging pool.
    commit >= if bake { 256 * MIB } else { 1024 * MIB }
}

/// Leave commit for game allocations and submissions already in flight while
/// still letting bindless textures and one-shot material bakes upload.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    if !enabled() {
        return true;
    }
    let now = crate::host::coarse_ms();
    let admit = now >= RETRY_UPLOAD_AT.load(Ordering::Relaxed) && has_upload_reserve(sample_commit(now), bake);
    if !admit {
        count(PAUSED, 1);
    }
    admit
}

/// Whether the staging allocator may commit a brand-new OS upload buffer
/// (reusing existing free ranges in already-committed pool blocks stays open
/// whenever `admit_texture_pass` is true).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    if !enabled() {
        return true;
    }
    let now = crate::host::coarse_ms();
    now >= RETRY_UPLOAD_AT.load(Ordering::Relaxed) && has_growth_reserve(sample_commit(now), bake)
}

/// On (see [`enabled`]): the PC counts as one with little memory.
pub(crate) fn limited_host_ram() -> bool {
    enabled()
}

fn next_pressure(was: bool, physical: u64, commit: u64) -> bool {
    if was {
        physical < 5 * GIB || commit < 6 * GIB
    } else {
        physical <= 3 * GIB || commit <= 4 * GIB
    }
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    if !enabled() {
        return;
    }
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    let was = pressured();
    let pressure = next_pressure(was, physical, commit);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            count(PRESSURES, 1);
            tracing::warn!(
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

/// The smaller of the PC's free memory and free commit, as of the last sweep.
pub(crate) fn headroom() -> Option<u64> {
    let bytes = HEADROOM.load(Ordering::Relaxed);
    (bytes != u64::MAX).then_some(bytes)
}

pub(crate) fn pressured() -> bool {
    PRESSURED.load(Ordering::Relaxed)
}

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    if !enabled() {
        return false;
    }
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

/// `bytes` a texture pass may stage in a frame, less under pressure.
pub(crate) fn limit_prefetch(bytes: u64) -> u64 {
    if !enabled() {
        return bytes;
    }
    let limit =
        prefetch_limit(bytes, pressured(), critical_commit(), limited_host_ram(), crate::texture::live_staging_bytes());
    if limit < bytes {
        count(THROTTLED, 1);
    }
    limit
}

/// Once in about five seconds, beside the frame line: how much this did.
/// Numbers only; the counts are those since the line before.
pub(crate) fn log_stats() {
    static NEXT: AtomicU64 = AtomicU64::new(5000);
    static TOLD_OFF: AtomicBool = AtomicBool::new(false);
    let now = crate::host::coarse_ms();
    if now < NEXT.load(Ordering::Relaxed) {
        return;
    }
    NEXT.store(now + 5000, Ordering::Relaxed);
    if !enabled() {
        if !TOLD_OFF.swap(true, Ordering::Relaxed) {
            tracing::info!(on = 0, "host RAM admission");
        }
        return;
    }
    let [throttled, paused, deferred, backoffs, freed, pressures] = STATS.each_ref().map(|s| s.swap(0, Ordering::Relaxed));
    tracing::info!(
        on = 1,
        pressured = u8::from(pressured()),
        physical_or_commit_free_mib = headroom().map_or(0, |bytes| bytes >> 20),
        throttled,
        paused,
        deferred,
        backoffs,
        freed_mb = freed >> 20,
        pressures,
        "host RAM admission, last five seconds"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switch_follows_the_variable_then_the_ram() {
        use std::ffi::OsStr;
        assert!(switch(None, Some(32 * GIB)));
        assert!(!switch(None, Some(32 * GIB + 1)));
        assert!(!switch(None, None));
        assert!(!switch(Some(OsStr::new("0")), Some(8 * GIB)));
        assert!(switch(Some(OsStr::new("1")), Some(64 * GIB)));
        assert!(switch(Some(OsStr::new("")), Some(16 * GIB)));
    }

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead, growth) in [
            (128 * MIB, false, false, false),
            (256 * MIB, true, false, false),
            (512 * MIB, true, true, false),
            (768 * MIB, true, true, false),
            (1024 * MIB, true, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
            assert_eq!(has_growth_reserve(commit, false), growth);
        }
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        assert!(next_pressure(false, 12 * GIB, 4 * GIB));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB));
        assert!(next_pressure(true, 6 * GIB, 5 * GIB));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB));
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, false, 0), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, false, 0), 192 << 20);
    }
}
