//! GPU time by pipeline (`VICTORY_GPU_RANK=1`, diagnostics builds): where
//! the card's time goes, before anything is cut.
//!
//! Each host list writes a timestamp when it opens, one each time the
//! pipeline or the kind of command changes (a run of draws with the same
//! pipeline costs one timestamp, not one each), one before the work its
//! closing records and one at its end. The list is read back when it is
//! reset, which is only after its last submission finished: nothing waits
//! for the card here. Every five seconds the twenty pipelines that took the
//! most card time per frame are logged by their shader hashes, with the
//! share of the lists' time the ranking covers.
//!
//! A run's time goes from its first command to the next run's (or the
//! closing work's) timestamp: what the list records in between (barriers,
//! copies, the next draw's preparation) is counted to the run.
//!
//! Off by default, and off with `VICTORY_GPU_TIMING` (that mode times each
//! noted command in the same slots).
#![cfg_attr(not(feature = "diagnostics"), allow(dead_code))]

use std::collections::HashMap;
use std::sync::Arc;

/// What a ranked command is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Kind {
    /// A draw, indexed or not, direct or indirect.
    Draw,
    /// A mesh dispatch, direct or indirect.
    Mesh,
    /// A compute dispatch, direct or indirect.
    Dispatch,
}

impl Kind {
    fn word(self) -> &'static str {
        match self {
            Kind::Draw => "draw",
            Kind::Mesh => "mesh",
            Kind::Dispatch => "dispatch",
        }
    }
}

/// Consecutive commands of one list with the same pipeline and kind.
#[derive(Clone, Debug)]
struct Run {
    pipeline: usize,
    name: Arc<str>,
    kind: Kind,
    calls: u32,
    /// The timestamp slot written before its first command.
    start: u32,
}

/// What one host list timed since it was opened.
#[derive(Debug, Default)]
pub(crate) struct ListRank {
    /// The list wrote its opening timestamp (slot 0).
    open: bool,
    runs: Vec<Run>,
    /// The next free slot.
    next: u32,
    /// The slot that ends the last run.
    end: Option<u32>,
    /// The slot written at the list's end.
    last: Option<u32>,
    /// Commands after the slots ran out.
    untimed: u32,
    /// The list went to the card since it was opened.
    sent: bool,
}

impl ListRank {
    /// The list was opened: the slot of its opening timestamp.
    pub(crate) fn begin(&mut self) -> u32 {
        *self = ListRank { open: true, next: 1, ..ListRank::default() };
        0
    }

    /// A command of `kind` with `pipeline` (a host object, named by `name`)
    /// is recorded: the slot to write a timestamp in before it, if any.
    /// `cap` is the slots the list has; the last two are kept for the
    /// closing ones.
    pub(crate) fn command(&mut self, pipeline: usize, kind: Kind, name: impl FnOnce() -> Arc<str>, cap: u32) -> Option<u32> {
        if !self.open {
            return None;
        }
        if self.end.is_some() {
            self.untimed += 1;
            return None;
        }
        if let Some(run) = self.runs.last_mut()
            && run.pipeline == pipeline
            && run.kind == kind
        {
            run.calls += 1;
            return None;
        }
        let slot = self.next;
        self.next += 1;
        if slot + 2 >= cap {
            // Out of slots: this one ends the last run, and the rest is
            // counted as untimed.
            self.end = Some(slot);
            self.untimed += 1;
        } else {
            self.runs.push(Run { pipeline, name: name(), kind, calls: 1, start: slot });
        }
        Some(slot)
    }

    /// The list's closing work begins: the slot that ends the last run.
    pub(crate) fn close_runs(&mut self) -> Option<u32> {
        if !self.open || self.end.is_some() {
            return None;
        }
        let slot = self.next;
        self.next += 1;
        self.end = Some(slot);
        Some(slot)
    }

    /// The list is closed: the slot of its last timestamp, and how many
    /// slots to read back.
    pub(crate) fn close_list(&mut self) -> Option<(u32, u32)> {
        if !self.open || self.last.is_some() {
            return None;
        }
        self.close_runs();
        let slot = self.next;
        self.next += 1;
        self.last = Some(slot);
        Some((slot, self.next))
    }

    /// The list was submitted.
    pub(crate) fn sent(&mut self) {
        self.sent = true;
    }

    /// How many slots to read back when the list is reset, if it ran.
    pub(crate) fn written(&self) -> Option<u32> {
        (self.sent && self.last.is_some()).then_some(self.next)
    }

    /// Adds what the list's timestamps say to `tally`. A slot never written
    /// reads 0: what it bounds is left out.
    pub(crate) fn harvest(&self, stamps: &[u64], tally: &mut Tally) {
        let (Some(end), Some(last)) = (self.end, self.last) else {
            return;
        };
        let at = |slot: u32| stamps.get(slot as usize).copied().filter(|&t| t != 0);
        if let (Some(a), Some(b)) = (at(0), at(last)) {
            tally.list_ticks += b.saturating_sub(a);
            tally.lists += 1;
        }
        for (i, run) in self.runs.iter().enumerate() {
            let to = self.runs.get(i + 1).map_or(end, |r| r.start);
            let (Some(a), Some(b)) = (at(run.start), at(to)) else {
                continue;
            };
            let row = tally.rows.entry((run.name.clone(), run.kind)).or_default();
            row.calls += u64::from(run.calls);
            row.runs += 1;
            row.ticks += b.saturating_sub(a);
        }
        tally.untimed += u64::from(self.untimed);
    }
}

/// What one pipeline did in the window.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Row {
    calls: u64,
    runs: u64,
    ticks: u64,
}

/// The card time by pipeline since the last line.
#[derive(Debug, Default)]
pub(crate) struct Tally {
    rows: HashMap<(Arc<str>, Kind), Row>,
    /// The timed lists' time from their first timestamp to their last.
    list_ticks: u64,
    lists: u64,
    untimed: u64,
}

impl Tally {
    /// The `n` rows with the most time, most first (ties by name).
    fn top(&self, n: usize) -> Vec<(&str, Kind, Row)> {
        let mut rows: Vec<(&str, Kind, Row)> = self.rows.iter().map(|((name, kind), row)| (&**name, *kind, *row)).collect();
        rows.sort_by(|a, b| b.2.ticks.cmp(&a.2.ticks).then_with(|| a.0.cmp(b.0)).then_with(|| a.1.word().cmp(b.1.word())));
        rows.truncate(n);
        rows
    }

    /// The log lines: a summary, then one per pipeline of the top `n`.
    /// `frames` presented in `wall_ms`; `ticks_per_ms` of the card's clock.
    pub(crate) fn lines(&self, frames: u64, wall_ms: f64, ticks_per_ms: f64, n: usize) -> Vec<String> {
        let frames_f = frames.max(1) as f64;
        let ms = |ticks: u64| ticks as f64 / ticks_per_ms.max(1e-9) / frames_f;
        let all: u64 = self.rows.values().map(|r| r.ticks).sum();
        let runs: u64 = self.rows.values().map(|r| r.runs).sum();
        let calls: u64 = self.rows.values().map(|r| r.calls).sum();
        let top = self.top(n);
        let top_ticks: u64 = top.iter().map(|r| r.2.ticks).sum();
        let share = |ticks: u64| if self.list_ticks == 0 { 0.0 } else { 100.0 * ticks as f64 / self.list_ticks as f64 };
        let mut lines = vec![format!(
            "GPU rank over {frames} frames ({:.1} ms/frame wall): host lists {:.2} ms/frame ({:.1} lists/frame), \
             ranked {:.2} ms/frame ({:.1}% of lists), top {} {:.2} ms/frame ({:.1}% of lists), \
             {:.0} commands in {:.0} runs per frame, {:.1} untimed commands/frame",
            wall_ms / frames_f,
            ms(self.list_ticks),
            self.lists as f64 / frames_f,
            ms(all),
            share(all),
            top.len(),
            ms(top_ticks),
            share(top_ticks),
            calls as f64 / frames_f,
            runs as f64 / frames_f,
            self.untimed as f64 / frames_f,
        )];
        for (i, (name, kind, row)) in top.iter().enumerate() {
            lines.push(format!(
                "GPU rank {:2}: {:7.3} ms/frame {:5.1}% {:8} {:8.1} calls/frame {:7.4} ms/call {:6.1} runs/frame {name}",
                i + 1,
                ms(row.ticks),
                share(row.ticks),
                kind.word(),
                row.calls as f64 / frames_f,
                row.ticks as f64 / ticks_per_ms.max(1e-9) / row.calls.max(1) as f64,
                row.runs as f64 / frames_f,
            ));
        }
        lines
    }
}

#[cfg(feature = "diagnostics")]
pub(crate) use live::*;

/// The switch, the names of pipelines, the tally and its line.
#[cfg(feature = "diagnostics")]
mod live {
    use super::{Kind, Tally};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    /// Whether pipelines are ranked (`VICTORY_GPU_RANK=1`, not with
    /// `VICTORY_GPU_TIMING`).
    pub(crate) fn on() -> bool {
        static ON: OnceLock<bool> = OnceLock::new();
        *ON.get_or_init(|| crate::switch::on("VICTORY_GPU_RANK") && !super::super::timing())
    }

    /// A pipeline's name by its host object, made once (and again if the
    /// address comes to name another).
    pub(crate) fn name(pipeline: usize, text: &str) -> Arc<str> {
        static NAMES: Mutex<Option<HashMap<usize, Arc<str>>>> = Mutex::new(None);
        let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
        let name = names.get_or_insert_with(HashMap::new).entry(pipeline).or_insert_with(|| text.into());
        if &**name != text {
            *name = text.into();
        }
        name.clone()
    }

    static TALLY: Mutex<Option<Tally>> = Mutex::new(None);

    /// Adds a finished list's timestamps.
    pub(crate) fn harvest(list: &super::ListRank, stamps: &[u64]) {
        let mut tally = TALLY.lock().unwrap_or_else(|e| e.into_inner());
        list.harvest(stamps, tally.get_or_insert_with(Tally::default));
    }

    /// Counts a presented frame; every five seconds logs the ranking.
    pub(crate) fn presented() {
        if !on() {
            return;
        }
        static WINDOW: Mutex<Option<(std::time::Instant, u64)>> = Mutex::new(None);
        let mut window = WINDOW.lock().unwrap_or_else(|e| e.into_inner());
        let (since, frames) = window.get_or_insert_with(|| (std::time::Instant::now(), 0));
        *frames += 1;
        if since.elapsed() < std::time::Duration::from_secs(5) {
            return;
        }
        let wall_ms = since.elapsed().as_secs_f64() * 1e3;
        let frames = std::mem::take(frames);
        *since = std::time::Instant::now();
        drop(window);
        let Some(tally) = TALLY.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let Some(host) = super::super::host() else {
            return;
        };
        // SAFETY: a plain query.
        let frequency = unsafe { host.queue.GetTimestampFrequency() }.unwrap_or(0);
        if frequency == 0 {
            return;
        }
        for line in tally.lines(frames, wall_ms, frequency as f64 / 1e3, 20) {
            tracing::info!("{line}");
        }
    }

    /// The kind of an indirect execution of the title's argument type.
    pub(crate) fn indirect(kind: u32) -> Kind {
        match kind {
            2 => Kind::Dispatch,
            10 => Kind::Mesh,
            _ => Kind::Draw,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAP: u32 = 16;

    fn name(text: &'static str) -> impl FnOnce() -> Arc<str> {
        move || text.into()
    }

    /// Commands with the same pipeline and kind in a row are one run with
    /// one timestamp; another kind or pipeline starts a run.
    #[test]
    fn runs_of_one_pipeline_take_one_slot() {
        let mut list = ListRank::default();
        assert_eq!(list.command(1, Kind::Draw, name("a"), CAP), None, "not timed before it opens");
        assert_eq!(list.begin(), 0);
        assert_eq!(list.command(1, Kind::Draw, name("a"), CAP), Some(1));
        assert_eq!(list.command(1, Kind::Draw, name("a"), CAP), None);
        assert_eq!(list.command(1, Kind::Draw, name("a"), CAP), None);
        assert_eq!(list.command(1, Kind::Mesh, name("a"), CAP), Some(2));
        assert_eq!(list.command(2, Kind::Mesh, name("b"), CAP), Some(3));
        assert_eq!(list.command(1, Kind::Draw, name("a"), CAP), Some(4));
        assert_eq!(list.close_runs(), Some(5));
        assert_eq!(list.close_list(), Some((6, 7)));
        assert_eq!(list.close_list(), None);
        assert_eq!(list.runs.iter().map(|r| r.calls).collect::<Vec<_>>(), [3, 1, 1, 1]);
        assert_eq!(list.written(), None, "never sent");
        list.sent();
        assert_eq!(list.written(), Some(7));
    }

    /// Past the slots, commands are counted as untimed, the last run ends
    /// where they begin, and the closing slots stay inside the heap.
    #[test]
    fn slots_never_run_past_the_heap() {
        let mut list = ListRank::default();
        list.begin();
        let mut slots = Vec::new();
        for i in 0..40 {
            slots.extend(list.command(i, Kind::Dispatch, name("x"), CAP));
        }
        assert_eq!(list.close_runs(), None, "the last run already ended");
        let (last, count) = list.close_list().unwrap();
        slots.push(last);
        assert!(slots.iter().all(|&s| s < CAP), "{slots:?}");
        assert_eq!(count, CAP);
        assert_eq!(list.runs.len(), CAP as usize - 3);
        assert_eq!(list.end, Some(CAP - 2));
        assert_eq!(list.untimed, 40 - (CAP - 3));
    }

    /// Each run lasts until the next run's timestamp, the last until the
    /// closing work's; unwritten slots leave their run out.
    #[test]
    fn runs_are_timed_to_the_next_timestamp() {
        let mut list = ListRank::default();
        list.begin();
        list.command(1, Kind::Draw, name("a"), CAP); // slot 1
        list.command(1, Kind::Draw, name("a"), CAP);
        list.command(2, Kind::Mesh, name("b"), CAP); // slot 2
        list.command(3, Kind::Dispatch, name("c"), CAP); // slot 3
        list.command(1, Kind::Draw, name("a"), CAP); // slot 4
        list.close_list(); // end 5, last 6
        let stamps = [100, 110, 150, 0, 200, 260, 300];
        let mut tally = Tally::default();
        list.harvest(&stamps, &mut tally);
        let a = tally.rows[&("a".into(), Kind::Draw)];
        assert_eq!(a, Row { calls: 3, runs: 2, ticks: 40 + 60 });
        assert!(!tally.rows.contains_key(&("b".into(), Kind::Mesh)), "ends at an unwritten slot");
        assert!(!tally.rows.contains_key(&("c".into(), Kind::Dispatch)), "starts at an unwritten slot");
        assert_eq!((tally.list_ticks, tally.lists), (200, 1));
    }

    /// The same pipeline from two lists (or two host objects of the same
    /// shaders) is one row; the top is by time, most first, cut to `n`.
    #[test]
    fn the_top_is_by_time_across_lists() {
        let mut tally = Tally::default();
        for (pipeline, text, ticks) in [(1, "a", 10), (2, "b", 50), (3, "a", 30), (4, "c", 20)] {
            let mut list = ListRank::default();
            list.begin();
            list.command(pipeline, Kind::Mesh, name(text), CAP);
            list.close_list();
            list.harvest(&[1, 1, 1 + ticks, 2 + ticks], &mut tally);
        }
        let top = tally.top(2);
        assert_eq!(top.iter().map(|r| (r.0, r.2.ticks)).collect::<Vec<_>>(), [("b", 50), ("a", 40)]);
        assert_eq!(top[1].2.calls, 2);
        assert_eq!(tally.lists, 4);
    }

    /// The lines give milliseconds per frame, the share of the lists' time
    /// and the time per call.
    #[test]
    fn lines_are_per_frame() {
        let mut tally = Tally::default();
        let mut list = ListRank::default();
        list.begin();
        for _ in 0..3 {
            list.command(1, Kind::Mesh, name("[mesh 0123, pixel 4567]"), CAP);
        }
        list.command(2, Kind::Dispatch, name("[compute 89ab]"), CAP);
        list.close_list();
        // Ticks of a microsecond: 6 ms of mesh, 2 ms of dispatch, 10 ms of list.
        list.harvest(&[1_000, 2_000, 8_000, 10_000, 11_000], &mut tally);
        let lines = tally.lines(2, 500.0, 1_000.0, 20);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("2 frames (250.0 ms/frame wall)"), "{}", lines[0]);
        assert!(lines[0].contains("host lists 5.00 ms/frame"), "{}", lines[0]);
        assert!(lines[0].contains("ranked 4.00 ms/frame (80.0% of lists)"), "{}", lines[0]);
        assert!(lines[0].contains("2 commands in 1 runs per frame"), "{}", lines[0]);
        assert!(lines[1].starts_with("GPU rank  1:   3.000 ms/frame  60.0% mesh"), "{}", lines[1]);
        assert!(lines[1].contains("1.5 calls/frame  2.0000 ms/call"), "{}", lines[1]);
        assert!(lines[1].ends_with("[mesh 0123, pixel 4567]"), "{}", lines[1]);
        assert!(lines[2].contains("dispatch"), "{}", lines[2]);
    }
}
