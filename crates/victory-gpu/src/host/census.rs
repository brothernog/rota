//! Diagnostics: the submitting thread's waits for flights' title memory,
//! taken apart (`VICTORY_WAIT_CENSUS=1`; off by default).
//!
//! [`super::blocked_by`] stops at the first read in the way and the blame
//! names only that one. Here every range in the way is listed, and once the
//! wait is over each is looked at again: did the flights change its bytes at
//! all, and does the card read what it becomes (a fill of upload memory, a
//! texture, a movie plane), or is it title memory again (a buffer copy)?
//! About once a second one line, per frame: the ms waited for every reason,
//! the ms of waits for title memory read, the ms of those on bytes no flight
//! changed, where the card reads what waited, and either; the five ranges
//! (bytes, declared region's bytes) that waited longest; and why
//! [`super::card_steps`] left the submission to the wait. Only numbers.
//!
//! "Changed" is a byte that differs after the wait from what it was before.
//! A wait that begins while flights are being landed may see bytes already
//! put back: every range that a put of that landing touched counts as
//! changed (`shadow::put_changed` tells [`put`] before it writes). So is a
//! byte the title's own threads wrote meanwhile. The unchanged ms are a
//! floor.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::{HostList, Step, WHY_READ, card_steps, place, read_blocked, touches};

/// `VICTORY_WAIT_CENSUS=1`.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WAIT_CENSUS").is_some_and(|v| v == "1"))
}

/// What waited.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Fill,
    Texture,
    Plane,
    CopySource,
    CopyDestination,
    Placed,
}

impl Kind {
    /// Whether what is read goes to the card (a step that copies it there).
    fn card_reads(self) -> bool {
        matches!(self, Kind::Fill | Kind::Texture | Kind::Plane)
    }

    fn name(self) -> &'static str {
        match self {
            Kind::Fill => "a fill of upload memory",
            Kind::Texture => "a texture",
            Kind::Plane => "a movie plane",
            Kind::CopySource => "a buffer copy's source",
            Kind::CopyDestination => "a buffer copy's destination",
            Kind::Placed => "a buffer copy at its place",
        }
    }
}

/// Every step of `runs` in the way of flights that write `written`, as
/// (what, address, bytes); and whether a step of ours runs.
fn conflicts(written: &[(usize, usize)], runs: &[&HostList]) -> (Vec<(Kind, usize, usize)>, bool) {
    let mut all = Vec::new();
    let mut ours = false;
    let mut placed: Vec<(usize, usize, usize)> = Vec::new();
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    if read_blocked(written, src, len) {
                        all.push((Kind::CopySource, src, len));
                    }
                    if touches(written, dst, dst + len) {
                        all.push((Kind::CopyDestination, dst, len));
                    }
                }
                &Step::Fill(_, src, len, _) => {
                    if read_blocked(written, src, len) {
                        all.push((Kind::Fill, src, len));
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if touches(written, at, at + len) {
                        all.push((Kind::Texture, at, len));
                    }
                }
                &Step::Plane((at, len), _) => {
                    if touches(written, at, at.saturating_add(len)) {
                        all.push((Kind::Plane, at, len));
                    }
                }
                Step::Run(_) => ours = true,
                Step::Placed(p) => match p.waits(written) {
                    place::Waits::No => {}
                    place::Waits::ForReaders => placed.push((p.dst, p.dst + p.len, p.len)),
                    place::Waits::Yes => all.push((Kind::Placed, p.src, p.len)),
                },
                Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(..) => {}
            }
        }
    }
    if !placed.is_empty() && place::read_by(runs, placed.iter().map(|p| (p.0, p.1)).collect()) {
        all.extend(placed.into_iter().map(|(dst, _, len)| (Kind::Placed, dst, len)));
    }
    (all, ours)
}

/// The bytes of a range looked at again after the wait; a bigger one is
/// not (unknown).
const MOST_BYTES: usize = 1 << 20;
/// Nor more than this many in one wait.
const MOST_WAIT: usize = 8 << 20;

struct Conflict {
    kind: Kind,
    len: usize,
    /// The bytes of the declared write it lies in.
    declared: usize,
    at: usize,
    before: Option<Vec<u8>>,
    /// A put of the landing under way touched it before it was read.
    put: bool,
}

/// A wait about to begin ([`before`]).
pub(super) struct Pending {
    conflicts: Vec<Conflict>,
    ours: bool,
    card_copy: bool,
    landing: bool,
    refused: &'static str,
    began: std::time::Instant,
}

/// The puts of the landing under way (address, bytes); and whether there
/// were too many to keep.
static PUTS: Mutex<(Vec<(usize, usize)>, bool)> = Mutex::new((Vec::new(), false));

/// `shadow::put_changed` is about to write `len` bytes at `at`.
pub(crate) fn put(at: usize, len: usize) {
    let mut puts = PUTS.lock().unwrap_or_else(|e| e.into_inner());
    if puts.0.len() < 1 << 16 {
        puts.0.push((at, at + len));
    } else {
        puts.1 = true;
    }
}

/// A landing begins (under the line's lock).
pub(super) fn landing_begins() {
    *PUTS.lock().unwrap_or_else(|e| e.into_inner()) = (Vec::new(), false);
}

/// The submission of `runs` is about to wait for flights in the air.
pub(super) fn before(runs: &[&HostList]) -> Option<Pending> {
    if !on() {
        return None;
    }
    // Under the line's lock no landing begins; one under way puts bytes
    // meanwhile, and tells [`put`] before each.
    let line = super::line();
    let (found, ours) = conflicts(&line.written, runs);
    let card_copy = card_steps::copied_read(&card_steps::copies(&line), runs);
    let mut budget = MOST_WAIT;
    let mut conflicts: Vec<Conflict> = found
        .into_iter()
        .map(|(kind, at, len)| {
            let i = line.written.partition_point(|r| r.1 <= at);
            let declared = line.written.get(i).map_or(0, |r| r.1 - r.0);
            let before = (len <= MOST_BYTES && len <= budget)
                .then(|| {
                    let mut bytes = vec![0; len];
                    budget -= len;
                    super::read_into(at as *const u8, &mut bytes).then_some(bytes)
                })
                .flatten();
            Conflict { kind, len, declared, at, before, put: false }
        })
        .collect();
    let landing = line.settling;
    if landing {
        // After the bytes were read: a put they saw was told before it.
        let puts = PUTS.lock().unwrap_or_else(|e| e.into_inner());
        for c in &mut conflicts {
            c.put = puts.1 || puts.0.iter().any(|&(a, b)| a < c.at + c.len && c.at < b);
        }
    }
    drop(line);
    let refused = super::BLAMED.lock().unwrap_or_else(|e| e.into_inner()).1;
    Some(Pending { conflicts, ours, card_copy, landing, refused, began: std::time::Instant::now() })
}

/// Of a wait's ranges, (changed, the card reads it) each, `None` for
/// unknown: whether no range changed, whether the card reads every one, and
/// whether each did not change or the card reads it; `None` where one is
/// unknown.
fn verdict(ranges: &[(Option<bool>, bool)]) -> (Option<bool>, bool, Option<bool>) {
    let unknown = ranges.iter().any(|r| r.0.is_none());
    let unchanged = if ranges.iter().any(|r| r.0 == Some(true)) {
        Some(false)
    } else {
        (!unknown).then_some(true)
    };
    let card = ranges.iter().all(|r| r.1);
    let either = if ranges.iter().any(|r| r.0 == Some(true) && !r.1) {
        Some(false)
    } else {
        ranges.iter().all(|r| r.0 == Some(false) || r.1).then_some(true)
    };
    (unchanged, card, either)
}

#[derive(Default)]
struct Stats {
    frames: u64,
    waits: u64,
    us: u64,
    unchanged_us: u64,
    card_us: u64,
    either_us: u64,
    unknown_us: u64,
    landing_us: u64,
    /// Waits with no range listed: a step of ours, a copy done on the card.
    other_us: u64,
    ranges: u64,
    unchanged_ranges: u64,
    /// (what, bytes, declared bytes): ranges, unchanged ones, µs of the
    /// waits they were in.
    top: HashMap<(Kind, usize, usize), (u64, u64, u64)>,
    refused: HashMap<&'static str, (u64, u64)>,
}

static STATS: Mutex<Option<Stats>> = Mutex::new(None);
/// µs waited for flights, by why (the `WHY_` places), since the last line.
static WAITED: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];

/// A wait for flights for reason `why` took `us` ([`super::relay_wait`]).
pub(super) fn waited(why: usize, us: u64) {
    WAITED[why.min(10)].fetch_add(us, Ordering::Relaxed);
}

/// The wait [`before`] began is over.
pub(super) fn after(pending: Pending) {
    let us = pending.began.elapsed().as_micros() as u64;
    let mut ranges = Vec::with_capacity(pending.conflicts.len());
    let mut changed_of = Vec::with_capacity(pending.conflicts.len());
    for c in &pending.conflicts {
        let changed = if c.put {
            Some(true)
        } else {
            c.before.as_ref().map(|before| {
                let mut now = vec![0; before.len()];
                !super::read_into(c.at as *const u8, &mut now) || now != *before
            })
        };
        changed_of.push(changed);
        ranges.push((changed, c.kind.card_reads()));
    }
    let mut all = STATS.lock().unwrap_or_else(|e| e.into_inner());
    let s = all.get_or_insert_with(|| Stats { frames: crate::queue::presented(), ..Default::default() });
    s.waits += 1;
    s.us += us;
    if pending.landing {
        s.landing_us += us;
    }
    if ranges.is_empty() || pending.ours || pending.card_copy {
        s.other_us += us;
    } else {
        let (unchanged, card, either) = verdict(&ranges);
        s.unchanged_us += if unchanged == Some(true) { us } else { 0 };
        s.card_us += if card { us } else { 0 };
        s.either_us += if either == Some(true) { us } else { 0 };
        s.unknown_us += if unchanged.is_none() { us } else { 0 };
    }
    let mut seen = std::collections::HashSet::new();
    for (c, changed) in pending.conflicts.iter().zip(&changed_of) {
        s.ranges += 1;
        let unchanged = u64::from(*changed == Some(false));
        s.unchanged_ranges += unchanged;
        let key = (c.kind, c.len, c.declared);
        let e = s.top.entry(key).or_default();
        e.0 += 1;
        e.1 += unchanged;
        if seen.insert(key) {
            e.2 += us;
        }
    }
    if !pending.refused.is_empty() {
        let e = s.refused.entry(pending.refused).or_default();
        *e = (e.0 + 1, e.1 + us);
    }
    if s.waits > 0 && LAST.load(Ordering::Relaxed) + 1000 <= super::now_ms() {
        let s = all.take().unwrap_or_default();
        drop(all);
        log(s);
    }
}

/// When the last line was written (ms, [`super::now_ms`]).
static LAST: AtomicU64 = AtomicU64::new(0);

fn log(s: Stats) {
    let now = super::now_ms();
    let seconds = (now - LAST.swap(now, Ordering::Relaxed).min(now)) as f64 / 1e3;
    let frames = crate::queue::presented().saturating_sub(s.frames).max(1) as f64;
    let ms = |us: u64| us as f64 / 1e3 / frames;
    let w: Vec<u64> = WAITED.iter().map(|a| a.swap(0, Ordering::Relaxed)).collect();
    let all: u64 = w.iter().sum();
    tracing::info!(
        "wait census per frame ({frames} frames in {seconds:.1} s, VICTORY_WAIT_CENSUS=1): {:.1} ms waited for flights in all \
         (read {:.1}, present/sweep {:.1}, value {:.1}, stale {:.1}, our step {:.1}, lists {:.1}, fence {:.1}); census of reads: \
         {:.1} ms in {:.2} waits, {:.1} ms on bytes no flight changed, {:.1} ms where the card reads every range, {:.1} ms where \
         each range is unchanged or read by the card, {:.1} ms unknown (a range over 1 MiB), {:.1} ms with nothing listed \
         (our step or a copy on the card), {:.1} ms begun during a landing; {:.1} ranges, {:.1} of them unchanged",
        ms(all),
        ms(w[WHY_READ]),
        ms(w[super::WHY_SYNC]),
        ms(w[super::WHY_VALUE]),
        ms(w[super::WHY_STALE]),
        ms(w[super::WHY_RUN]),
        ms(w[super::WHY_MOST]),
        ms(w[super::WHY_FENCE]),
        ms(s.us),
        s.waits as f64 / frames,
        ms(s.unchanged_us),
        ms(s.card_us),
        ms(s.either_us),
        ms(s.unknown_us),
        ms(s.other_us),
        ms(s.landing_us),
        s.ranges as f64 / frames,
        s.unchanged_ranges as f64 / frames,
    );
    let mut top: Vec<_> = s.top.into_iter().collect();
    top.sort_unstable_by_key(|(_, (_, _, us))| std::cmp::Reverse(*us));
    for ((kind, len, declared), (n, unchanged, us)) in top.into_iter().take(5) {
        tracing::info!(
            "wait census, per frame: {:.1} ms of waits with {} +{len:#x} in a declared write of {declared:#x} ({:.2} of them, \
             {:.2} unchanged)",
            ms(us),
            kind.name(),
            n as f64 / frames,
            unchanged as f64 / frames,
        );
    }
    let mut refused: Vec<_> = s.refused.into_iter().collect();
    refused.sort_unstable_by_key(|(_, (_, us))| std::cmp::Reverse(*us));
    for (why, (n, us)) in refused.into_iter().take(3) {
        tracing::info!("wait census, per frame: {:.1} ms in {:.2} waits not done on the card: {why}", ms(us), n as f64 / frames);
    }
}

#[cfg(test)]
mod tests {
    use super::verdict;

    #[test]
    fn a_wait_counts_by_all_its_ranges() {
        // (changed, the card reads it)
        assert_eq!(verdict(&[(Some(false), true), (Some(false), false)]), (Some(true), false, Some(true)));
        assert_eq!(verdict(&[(Some(true), true), (Some(false), false)]), (Some(false), false, Some(true)));
        assert_eq!(verdict(&[(Some(true), false), (Some(false), true)]), (Some(false), false, Some(false)));
        // Unknown: unchanged is not known, but a range the card reads is
        // avoidable either way.
        assert_eq!(verdict(&[(None, true), (Some(false), true)]), (None, true, Some(true)));
        assert_eq!(verdict(&[(None, false), (Some(false), true)]), (None, false, None));
        // A changed range already says "not all unchanged".
        assert_eq!(verdict(&[(None, false), (Some(true), false)]), (Some(false), false, Some(false)));
    }
}
