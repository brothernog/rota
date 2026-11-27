//! The frame log's states, counters and the thread that looks at them.

use std::cell::Cell;
use std::io::Write;
use std::sync::atomic::Ordering::{Acquire, Relaxed, Release};
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, AtomicUsize};
use std::sync::{Mutex, Once, OnceLock};
use std::time::{Duration, Instant};

/// `VICTORY_FRAME_LOG=<file>`.
pub fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| path().is_some())
}

/// `VICTORY_FRAME_LOG_MORE=1` (with the frame log): also the columns that
/// cost more to gather — copies by write-combined memory, list calls by
/// site, the process's and the system's memory. Without it those columns
/// stay empty and nothing of theirs runs.
pub(crate) fn more() -> bool {
    static MORE: OnceLock<bool> = OnceLock::new();
    *MORE.get_or_init(|| on() && std::env::var_os("VICTORY_FRAME_LOG_MORE").is_some_and(|v| v == "1"))
}

fn path() -> Option<std::path::PathBuf> {
    std::env::var_os("VICTORY_FRAME_LOG").filter(|v| !v.is_empty()).map(Into::into)
}

/// What of ours a thread is in (one byte per thread, [`enter`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum State {
    /// Nothing of ours: the title's own code, or a call we do not know.
    Title = 0,
    /// One of the title's waits (its imports of the system's waits).
    TitleWait,
    /// A wait of the title's on an event a fence of ours sets.
    FenceWait,
    /// A list call.
    List,
    /// Waiting for a host pipeline being made.
    PipelineWait,
    /// Waiting for a lock of the write trackers (the guards among them).
    Guards,
    /// In a map call of the title's, or of a queue's page-mapping copy.
    XmemTitle,
    XmemQueue,
    /// In a submission (what it does is [`PHASE`]'s to say).
    Submit,
    /// In the title's present.
    Present,
    /// In the title's frame wait (its pacing).
    Pace,
}

impl State {
    fn of(byte: u8) -> State {
        const ALL: [State; 11] = [
            State::Title,
            State::TitleWait,
            State::FenceWait,
            State::List,
            State::PipelineWait,
            State::Guards,
            State::XmemTitle,
            State::XmemQueue,
            State::Submit,
            State::Present,
            State::Pace,
        ];
        ALL.get(byte as usize).copied().unwrap_or(State::Title)
    }
}

/// One thread's state, in a line of its own.
#[repr(C, align(64))]
struct Slot {
    state: AtomicU8,
    thread: AtomicU32,
}

const SLOTS: usize = 1024;
static SLOT: [Slot; SLOTS] = [const { Slot { state: AtomicU8::new(0), thread: AtomicU32::new(0) } }; SLOTS];
static NEXT: AtomicUsize = AtomicUsize::new(0);

/// This thread's slot (none once all are given out).
fn mine() -> Option<usize> {
    thread_local!(static MINE: Cell<usize> = const { Cell::new(usize::MAX) });
    MINE.try_with(|mine| {
        if mine.get() == usize::MAX {
            let i = NEXT.fetch_add(1, Relaxed);
            if i < SLOTS {
                // SAFETY: a plain query.
                SLOT[i].thread.store(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }, Relaxed);
            } else {
                BANK[0].0[K::SlotsFull as usize].fetch_add(1, Relaxed);
            }
            mine.set(i.min(SLOTS));
        }
        mine.get()
    })
    .ok()
    .filter(|&i| i < SLOTS)
}

/// This thread is in `state` while it lives; then back in the one before.
pub(crate) struct In(Option<(usize, u8)>);

#[inline]
pub(crate) fn enter(state: State) -> In {
    if !on() {
        return In(None);
    }
    In(mine().map(|i| (i, SLOT[i].state.swap(state as u8, Relaxed))))
}

impl Drop for In {
    #[inline]
    fn drop(&mut self) {
        if let Some((i, was)) = self.0 {
            SLOT[i].state.store(was, Relaxed);
        }
    }
}

/// What the submission under the lock is doing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub(crate) enum Phase {
    /// Preparing it (copies, textures, steps). (0: no submission.)
    Preparing = 1,
    /// Waiting for the card to run it (or flights it settles).
    Card,
    /// Copying back what the card wrote.
    Back,
    /// Waiting for flights to land (`relay_wait`).
    Landing,
}

static PHASE: AtomicU8 = AtomicU8::new(0);

/// The submission lock's holder is at `phase` while it lives; then at the
/// one before (callers hold the submission lock).
pub(crate) struct PhaseIn(Option<u8>);

pub(crate) fn phase(phase: Phase) -> PhaseIn {
    PhaseIn(on().then(|| PHASE.swap(phase as u8, Relaxed)))
}

impl PhaseIn {
    pub(crate) fn set(&self, phase: Phase) {
        if self.0.is_some() {
            PHASE.store(phase as u8, Relaxed);
        }
    }
}

impl Drop for PhaseIn {
    fn drop(&mut self) {
        if let Some(was) = self.0 {
            PHASE.store(was, Relaxed);
        }
    }
}

/// What the thread that lands flights is doing: 0 nothing, 1 waiting for
/// the card, 2 landing.
static RELAY: AtomicU8 = AtomicU8::new(0);

/// The landing thread is at `what` while it lives; then idle again.
pub(crate) struct RelayIn(bool);

pub(crate) fn relay(what: u8) -> RelayIn {
    if on() {
        RELAY.store(what, Relaxed);
    }
    RelayIn(on())
}

impl Drop for RelayIn {
    fn drop(&mut self) {
        if self.0 {
            RELAY.store(0, Relaxed);
        }
    }
}

/// Threads in map calls (the title's, the queues'), waiting for pipelines,
/// waiting for the write trackers' locks.
static XMEM: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
static PIPELINES: AtomicU32 = AtomicU32::new(0);
static GUARDS: AtomicU32 = AtomicU32::new(0);
/// DirectStorage reads being read; reads submitted and not done.
static READING: AtomicU32 = AtomicU32::new(0);
static DS_PENDING: AtomicU32 = AtomicU32::new(0);

/// `n` DirectStorage reads are submitted and not done (queued or read).
pub fn ds_pending(n: u32) {
    DS_PENDING.store(n, Relaxed);
}

/// Exact counts, summed over all threads: never reset (the looking thread
/// takes differences), each name a CSV column (`_ns` ones are written in ms
/// as `_ms`).
#[derive(Clone, Copy)]
pub(crate) enum K {
    XmemTitleCalls,
    XmemTitlePages,
    XmemTitleNs,
    XmemQueueCalls,
    XmemQueuePages,
    XmemQueueNs,
    TitleWaits,
    TitleWaitNs,
    ListCalls,
    ListNs,
    Submissions,
    SubmitLockNs,
    TexUploads,
    TexSkipped,
    PipelinesAsked,
    PipelineWaits,
    PipelineWaitNs,
    Dropped,
    DroppedRtOff,
    DsReads,
    DsBytes,
    DsTitleNs,
    DsQueueNs,
    DsServiceNs,
    GuardWaits,
    GuardWaitNs,
    GuardFaults,
    /// The presenting thread's waits, by what ended them: a thread of the
    /// title's (a set, a release, a wake), a fence of ours, a sleep, or
    /// something else (a time-out, a waker we do not wrap).
    FrameWaitTitleNs,
    FrameWaitFenceNs,
    FrameWaitTimerNs,
    FrameWaitOtherNs,
    /// Bytes copied from title memory up to the card's copies, and bytes of
    /// the copy-back (compared after a submission; put into title memory),
    /// split by whether the title allocated that memory write-combined.
    UpWcBytes,
    UpOtherBytes,
    BackComparedWcBytes,
    BackComparedOtherBytes,
    BackWcBytes,
    BackOtherBytes,
    /// Waiting for a title list's state lock in list calls.
    ListLockNs,
    SlotsFull,
}

const NAMES: [&str; 39] = [
    "xmem_title_calls",
    "xmem_title_pages",
    "xmem_title_ns",
    "xmem_queue_calls",
    "xmem_queue_pages",
    "xmem_queue_ns",
    "title_waits",
    "title_wait_ns",
    "list_calls",
    "list_ns",
    "submissions",
    "submit_lock_ns",
    "tex_uploads",
    "tex_unchanged",
    "pipelines_asked",
    "pipeline_waits",
    "pipeline_wait_ns",
    "dropped",
    "dropped_rt_off",
    "ds_reads",
    "ds_bytes",
    "ds_title_ns",
    "ds_queue_ns",
    "ds_service_ns",
    "guard_waits",
    "guard_wait_ns",
    "guard_faults",
    "frame_wait_title_ns",
    "frame_wait_fence_ns",
    "frame_wait_timer_ns",
    "frame_wait_other_ns",
    "up_wc_bytes",
    "up_other_bytes",
    "back_compared_wc_bytes",
    "back_compared_other_bytes",
    "back_wc_bytes",
    "back_other_bytes",
    "list_lock_ns",
    "slots_full",
];
const _: () = assert!(NAMES.len() == K::SlotsFull as usize + 1);

#[repr(align(64))]
struct Bank([AtomicU64; NAMES.len()]);
const BANKS: usize = 16;
static BANK: [Bank; BANKS] = [const { Bank([const { AtomicU64::new(0) }; NAMES.len()]) }; BANKS];

/// Adds `n` to `k`.
#[inline]
pub(crate) fn count(k: K, n: u64) {
    if !on() {
        return;
    }
    BANK[mine_bank()].0[k as usize].fetch_add(n, Relaxed);
}

/// This thread's bank of counters.
#[inline]
fn mine_bank() -> usize {
    mine().unwrap_or(SLOTS) % BANKS
}

fn total(i: usize) -> u64 {
    BANK.iter().map(|b| b.0[i].load(Relaxed)).sum()
}

/// A wait counted in `n` and `ns`, with `waiters` one more while it lives.
pub(crate) struct Waiting {
    _in: In,
    of: Option<(&'static AtomicU32, K, K, Instant)>,
}

fn waiting(state: State, waiters: &'static AtomicU32, n: K, ns: K) -> Waiting {
    if !on() {
        return Waiting { _in: In(None), of: None };
    }
    waiters.fetch_add(1, Relaxed);
    Waiting { _in: enter(state), of: Some((waiters, n, ns, Instant::now())) }
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some((waiters, n, ns, began)) = self.of {
            waiters.fetch_sub(1, Relaxed);
            count(n, 1);
            count(ns, began.elapsed().as_nanos() as u64);
        }
    }
}

/// Waiting for a lock of the write trackers.
pub(crate) fn waiting_guards() -> Waiting {
    waiting(State::Guards, &GUARDS, K::GuardWaits, K::GuardWaitNs)
}

/// Waiting for a host pipeline.
pub(crate) fn waiting_pipeline() -> Waiting {
    waiting(State::PipelineWait, &PIPELINES, K::PipelineWaits, K::PipelineWaitNs)
}

/// What a map call is for.
#[derive(Clone, Copy)]
pub enum Xmem {
    /// The title's own `XMemMapPhysicalPages(Scatter)`.
    Title,
    /// A queue's `CopyPageMappings`.
    Queue,
}

thread_local!(static XMEM_DEPTH: Cell<u32> = const { Cell::new(0) });

/// A map call, counted while it lives (the outermost of a thread only: the
/// calls nest).
pub struct XmemCall(Option<(Xmem, u64, Instant, In)>);

impl XmemCall {
    /// The call maps `n` pages more than it said when it began.
    pub fn pages(&mut self, n: u64) {
        if let Some(call) = &mut self.0 {
            call.1 += n;
        }
    }
}

/// A map call of `pages` pages begins.
pub fn xmem(kind: Xmem, pages: u64) -> XmemCall {
    if !on() {
        return XmemCall(None);
    }
    let depth = XMEM_DEPTH.with(|d| d.replace(d.get() + 1));
    if depth > 0 {
        return XmemCall(None);
    }
    XMEM[kind as usize].fetch_add(1, Relaxed);
    let state = match kind {
        Xmem::Title => State::XmemTitle,
        Xmem::Queue => State::XmemQueue,
    };
    XmemCall(Some((kind, pages, Instant::now(), enter(state))))
}

impl Drop for XmemCall {
    fn drop(&mut self) {
        if !on() {
            return;
        }
        XMEM_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        let Some((kind, pages, began, _)) = self.0.take() else {
            return;
        };
        XMEM[kind as usize].fetch_sub(1, Relaxed);
        let ns = began.elapsed().as_nanos() as u64;
        let (calls, n, t) = match kind {
            Xmem::Title => (K::XmemTitleCalls, K::XmemTitlePages, K::XmemTitleNs),
            Xmem::Queue => (K::XmemQueueCalls, K::XmemQueuePages, K::XmemQueueNs),
        };
        count(calls, 1);
        count(n, pages);
        count(t, ns);
    }
}

/// A DirectStorage read, counted as being read while it lives.
pub struct Reading(bool);

/// A DirectStorage read begins.
pub fn reading() -> Reading {
    if on() {
        READING.fetch_add(1, Relaxed);
    }
    Reading(on())
}

impl Drop for Reading {
    fn drop(&mut self) {
        if self.0 {
            READING.fetch_sub(1, Relaxed);
        }
    }
}

/// A DirectStorage read of `bytes` is done: from the title's enqueue to its
/// submit, from the submit to the read's start, and the read.
pub fn dstorage(bytes: u64, title_ns: u64, queue_ns: u64, service_ns: u64) {
    count(K::DsReads, 1);
    count(K::DsBytes, bytes);
    count(K::DsTitleNs, title_ns);
    count(K::DsQueueNs, queue_ns);
    count(K::DsServiceNs, service_ns);
}

/// The title memory allocated write-combined (protection bit 0x400): start
/// to end, not overlapping.
static WC: std::sync::RwLock<std::collections::BTreeMap<usize, usize>> = std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Leaves `[at, end)` out of the write-combined ranges.
fn cut(map: &mut std::collections::BTreeMap<usize, usize>, at: usize, end: usize) {
    let hit: Vec<(usize, usize)> = map.range(..end).rev().take_while(|&(_, &e)| e > at).map(|(&s, &e)| (s, e)).collect();
    for (s, e) in hit {
        map.remove(&s);
        if s < at {
            map.insert(s, at);
        }
        if e > end {
            map.insert(end, e);
        }
    }
}

/// The title allocated `[at, at + len)`, write-combined or not.
pub fn title_memory_kind(at: usize, len: usize, write_combined: bool) {
    if !more() || len == 0 {
        return;
    }
    let mut map = WC.write().unwrap_or_else(|e| e.into_inner());
    cut(&mut map, at, at.saturating_add(len));
    if write_combined {
        map.insert(at, at.saturating_add(len));
    }
    WC_CHANGES.fetch_add(1, Relaxed);
}

/// Changes of [`WC`]: a stretch a thread keeps ([`SEGMENT`]) holds while
/// this does.
static WC_CHANGES: AtomicU64 = AtomicU64::new(0);

thread_local! {
    /// The last stretch this thread looked up: [`WC_CHANGES`] then, start,
    /// end, and whether it is write-combined. Copies come in long runs
    /// over one buffer, so most need no look at the map, whose lock all
    /// copying threads share.
    static SEGMENT: Cell<(u64, usize, usize, bool)> = const { Cell::new((u64::MAX, 0, 0, false)) };
}

/// The stretch of memory around `at` that is all write-combined or all
/// not: a range of the map, or the gap between two.
fn segment(map: &std::collections::BTreeMap<usize, usize>, at: usize) -> (usize, usize, bool) {
    match map.range(..=at).next_back() {
        Some((&s, &e)) if e > at => (s, e, true),
        before => {
            let from = before.map_or(0, |(_, &e)| e);
            let to = map.range(at..).next().map_or(usize::MAX, |(&s, _)| s);
            (from, to, false)
        }
    }
}

/// The bytes of `[at, end)` the title allocated write-combined.
fn wc_of(at: usize, end: usize) -> usize {
    let changes = WC_CHANGES.load(Relaxed);
    let (seen, s, e, wc) = SEGMENT.get();
    if seen == changes && s <= at && end <= e {
        return if wc { end - at } else { 0 };
    }
    let map = WC.read().unwrap_or_else(|e| e.into_inner());
    let (s, e, wc) = segment(&map, at);
    SEGMENT.set((changes, s, e, wc));
    if end <= e { if wc { end - at } else { 0 } } else { wc_bytes(&map, at, end) }
}

/// The title freed `[at, at + len)` (`len` 0: the allocation at `at`).
pub(crate) fn title_memory_gone(at: usize, len: usize) {
    if !more() {
        return;
    }
    let mut map = WC.write().unwrap_or_else(|e| e.into_inner());
    if len == 0 {
        map.remove(&at);
    } else {
        cut(&mut map, at, at.saturating_add(len));
    }
    WC_CHANGES.fetch_add(1, Relaxed);
}

/// The bytes of `[at, end)` the title allocated write-combined.
fn wc_bytes(map: &std::collections::BTreeMap<usize, usize>, at: usize, end: usize) -> usize {
    map.range(..end).rev().take_while(|&(_, &e)| e > at).map(|(&s, &e)| e.min(end) - s.max(at)).sum()
}

/// Which way a copy of title memory goes.
#[derive(Clone, Copy)]
pub(crate) enum Copied {
    Up,
    BackCompared,
    Back,
}

/// `bytes` were copied for the title memory `[at, at + len)`.
///
/// shortcut: a copy of part of the range (pages the write watch found) is
/// split in proportion to the range's write-combined bytes, which is exact
/// while a range lies in one allocation, as a buffer's does.
pub(crate) fn copied(way: Copied, at: usize, len: usize, bytes: u64) {
    if !more() || len == 0 || bytes == 0 {
        return;
    }
    let wc = wc_of(at, at.saturating_add(len)) as u64;
    let wc = (bytes as u128 * wc as u128 / len as u128) as u64;
    let (w, o) = match way {
        Copied::Up => (K::UpWcBytes, K::UpOtherBytes),
        Copied::BackCompared => (K::BackComparedWcBytes, K::BackComparedOtherBytes),
        Copied::Back => (K::BackWcBytes, K::BackOtherBytes),
    };
    if wc > 0 {
        count(w, wc);
    }
    if bytes > wc {
        count(o, bytes - wc);
    }
}

/// List calls by where in `list.rs` they come from (a line; 0: free),
/// with their calls and nanoseconds. Lines are few (one per method).
const SITES: usize = 128;
static SITE_LINE: [AtomicU32; SITES] = [const { AtomicU32::new(0) }; SITES];
/// Per bank, as [`count`]'s: the recording threads add to lines of their own.
static SITE_CALLS: [[AtomicU64; SITES]; BANKS] = [const { [const { AtomicU64::new(0) }; SITES] }; BANKS];
static SITE_NS: [[AtomicU64; SITES]; BANKS] = [const { [const { AtomicU64::new(0) }; SITES] }; BANKS];

/// A list call from `line` of `list.rs` took `ns`, `lock_ns` of them
/// waiting for the list's state.
pub(crate) fn list_call(line: u32, ns: u64, lock_ns: u64) {
    if !more() {
        return;
    }
    count(K::ListLockNs, lock_ns);
    let mut i = line as usize % SITES;
    for _ in 0..SITES {
        let held = SITE_LINE[i].load(Relaxed);
        let mine = held == line
            || (held == 0
                && match SITE_LINE[i].compare_exchange(0, line, Relaxed, Relaxed) {
                    Ok(_) => true,
                    Err(now) => now == line,
                });
        if mine {
            let bank = mine_bank();
            SITE_CALLS[bank][i].fetch_add(1, Relaxed);
            SITE_NS[bank][i].fetch_add(ns, Relaxed);
            return;
        }
        i = (i + 1) % SITES;
    }
}

/// The process's working set and private bytes, and the memory the system
/// has available (MB).
fn memory_mb() -> (u64, u64, u64) {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS_EX { cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32, ..Default::default() };
    let mut m = MEMORYSTATUSEX { dwLength: size_of::<MEMORYSTATUSEX>() as u32, ..Default::default() };
    // SAFETY: our own process and sized structures.
    unsafe {
        K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast::<PROCESS_MEMORY_COUNTERS>(), c.cb);
        GlobalMemoryStatusEx(&mut m);
    }
    (c.WorkingSetSize as u64 >> 20, c.PrivateUsage as u64 >> 20, m.ullAvailPhys >> 20)
}

/// The system's reads from disk for page faults, as counts so far: the
/// reads and the pages they brought in (PDH, `\Memory\Page Reads/sec` and
/// `\Memory\Pages Input/sec` raw).
struct PageReads(*mut std::ffi::c_void, [*mut std::ffi::c_void; 2]);

impl PageReads {
    fn new() -> Option<PageReads> {
        use windows_sys::Win32::System::Performance::{PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhOpenQueryW};
        let wide = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let (mut query, mut counters) = (std::ptr::null_mut(), [std::ptr::null_mut(); 2]);
        // SAFETY: out handles and NUL-terminated paths.
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            for (path, counter) in [r"\Memory\Page Reads/sec", r"\Memory\Pages Input/sec"].into_iter().zip(&mut counters) {
                if PdhAddEnglishCounterW(query, wide(path).as_ptr(), 0, counter) != 0 {
                    PdhCloseQuery(query);
                    return None;
                }
            }
            PdhCollectQueryData(query);
        }
        Some(PageReads(query, counters))
    }

    /// Reads and pages so far.
    fn read(&self) -> (u64, u64) {
        use windows_sys::Win32::System::Performance::{PDH_RAW_COUNTER, PdhCollectQueryData, PdhGetRawCounterValue};
        let mut out = [0u64; 2];
        // SAFETY: our query and counters, and sized out values.
        unsafe {
            if PdhCollectQueryData(self.0) != 0 {
                return (0, 0);
            }
            for (counter, out) in self.1.iter().zip(&mut out) {
                let mut raw = PDH_RAW_COUNTER::default();
                if PdhGetRawCounterValue(*counter, std::ptr::null_mut(), &mut raw) == 0 {
                    *out = raw.FirstValue as u64;
                }
            }
        }
        (out[0], out[1])
    }
}

/// The events fences of ours set for the title.
static FENCE_EVENTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// The title asked a fence to set `event`.
pub(crate) fn fence_event(event: usize) {
    if !on() {
        return;
    }
    let mut events = FENCE_EVENTS.lock().unwrap_or_else(|e| e.into_inner());
    if !events.contains(&event) {
        events.push(event);
    }
}

/// What the presenting thread waits on now (0: nothing, or a sleep), and
/// what woke it ([`woke`]).
static FRAME_WAIT: AtomicU64 = AtomicU64::new(0);
static WAKER: AtomicU8 = AtomicU8::new(0);
/// Wakers: a thread of the title's, a fence of ours.
pub(crate) const BY_TITLE: u8 = 1;
pub(crate) const BY_FENCE: u8 = 2;

/// `object` was set, released or woken `by` one of [`BY_TITLE`], [`BY_FENCE`].
#[inline]
pub(crate) fn woke(object: u64, by: u8) {
    if object != 0 && FRAME_WAIT.load(Relaxed) == object {
        WAKER.store(by, Relaxed);
    }
}

/// One of the title's waits, on `object` (0: none, or a lock).
pub(crate) fn title_wait<T>(object: u64, call: impl FnOnce() -> T) -> T {
    use windows_sys::Win32::Foundation::{GetLastError, SetLastError};
    // SAFETY: plain queries of this thread's own.
    let error = unsafe { GetLastError() };
    let began = Instant::now();
    let presenting = mine().is_some_and(|i| i == FRAME_SLOT.load(Relaxed));
    let fence = presenting
        && object != 0
        && FENCE_EVENTS.lock().unwrap_or_else(|e| e.into_inner()).contains(&(object as usize));
    let state = enter(if fence { State::FenceWait } else { State::TitleWait });
    if presenting {
        WAKER.store(0, Relaxed);
        FRAME_WAIT.store(object, Relaxed);
    }
    // SAFETY: as above.
    unsafe { SetLastError(error) };
    let result = call();
    // SAFETY: as above.
    let error = unsafe { GetLastError() };
    drop(state);
    let ns = began.elapsed().as_nanos() as u64;
    if presenting {
        FRAME_WAIT.store(0, Relaxed);
        let by = match (object, WAKER.load(Relaxed)) {
            (0, _) => K::FrameWaitTimerNs,
            (_, BY_TITLE) => K::FrameWaitTitleNs,
            (_, BY_FENCE) => K::FrameWaitFenceNs,
            _ => K::FrameWaitOtherNs,
        };
        count(by, ns);
    }
    count(K::TitleWaits, 1);
    count(K::TitleWaitNs, ns);
    // SAFETY: as above.
    unsafe { SetLastError(error) };
    result
}

/// Presents so far, when the last one ended (microseconds of [`now_us`]),
/// and the slot of the thread that presents.
static PRESENTS: AtomicU64 = AtomicU64::new(0);
static PRESENT_US: AtomicU64 = AtomicU64::new(0);
static FRAME_SLOT: AtomicUsize = AtomicUsize::new(usize::MAX);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_us() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// The title's present begins on this thread (it is in it while this lives).
pub(crate) fn presenting() -> In {
    if !on() {
        return In(None);
    }
    if let Some(i) = mine() {
        FRAME_SLOT.store(i, Relaxed);
    }
    static START: Once = Once::new();
    START.call_once(start);
    enter(State::Present)
}

/// The title's present is over: the frame ends here.
pub(crate) fn presented() {
    if !on() {
        return;
    }
    PRESENT_US.store(now_us(), Relaxed);
    PRESENTS.fetch_add(1, Release);
}

/// Where a tick of the frame's wall time goes (the CSV's bucket columns, in
/// this order).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Bucket {
    /// The title's present (what of it is not a submission).
    Present,
    /// The title's frame wait.
    Pace,
    /// Our preparing of a submission (copies, textures, steps).
    Preparing,
    /// In a submission otherwise: waiting for the submission lock, the
    /// sweep, landing waits while nothing lands.
    Submit,
    /// Waiting for the card (a submission's, or flights' while the landing
    /// thread waits for it).
    GpuWait,
    /// Copying back what the card wrote (or flights being landed).
    CopyBack,
    /// Waiting for a lock of the write trackers.
    Guards,
    /// Map calls: the title's, a queue's page-mapping copies.
    XmemTitle,
    XmemQueue,
    /// Waiting for host pipelines.
    Pipelines,
    /// List calls: of the thread that presents, or of others while it waits.
    Recording,
    /// DirectStorage reads being read while the thread that presents waits.
    Dstorage,
    /// The thread that presents running (its processor time).
    TitleRunning,
    /// A wait of the title's with nothing of ours going on.
    TitleWait,
    /// A wait on a fence event with nothing of ours going on.
    FenceWait,
    /// Nothing seen.
    Unexplained,
}

const BUCKETS: [&str; 16] = [
    "present",
    "pace",
    "preparing",
    "submit",
    "gpu_wait",
    "copy_back",
    "guards",
    "xmem_title",
    "xmem_queue",
    "pipelines",
    "recording",
    "dstorage",
    "title_running",
    "title_wait",
    "fence_wait",
    "unexplained",
];
const _: () = assert!(BUCKETS.len() == Bucket::Unexplained as usize + 1);

/// What a look saw.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    /// The state of the thread that presents.
    pub frame: State,
    pub phase: u8,
    pub relay: u8,
    pub xmem: [bool; 2],
    pub pipelines: bool,
    pub guards: bool,
    /// Another thread is in a list call.
    pub recording: bool,
    pub reading: bool,
    /// DirectStorage reads are submitted and not done.
    pub ds_pending: bool,
    /// After a present, before the first list call.
    pub pre_list: bool,
}

/// A wait for flights: by what the landing thread does.
fn landing(relay: u8) -> Bucket {
    match relay {
        1 => Bucket::GpuWait,
        2 => Bucket::CopyBack,
        _ => Bucket::Submit,
    }
}

/// What the submission under the lock is doing, if one is.
fn submission(s: &Seen) -> Option<Bucket> {
    match s.phase {
        1 => Some(Bucket::Preparing),
        2 => Some(Bucket::GpuWait),
        3 => Some(Bucket::CopyBack),
        4 => Some(landing(s.relay)),
        _ => None,
    }
}

/// What of ours goes on anywhere, most direct first.
fn ours(s: &Seen) -> Option<Bucket> {
    submission(s)
        .or(s.xmem[0].then_some(Bucket::XmemTitle))
        .or(s.xmem[1].then_some(Bucket::XmemQueue))
        .or(s.pipelines.then_some(Bucket::Pipelines))
        .or(s.guards.then_some(Bucket::Guards))
        .or(s.recording.then_some(Bucket::Recording))
        .or(s.reading.then_some(Bucket::Dstorage))
}

/// The bucket of a tick that is not the presenting thread's running time.
pub(crate) fn bucket(s: &Seen) -> Bucket {
    match s.frame {
        State::Present => submission(s).unwrap_or(Bucket::Present),
        State::Pace => Bucket::Pace,
        State::List => Bucket::Recording,
        State::PipelineWait => Bucket::Pipelines,
        State::Guards => Bucket::Guards,
        State::XmemTitle => Bucket::XmemTitle,
        State::XmemQueue => Bucket::XmemQueue,
        State::Submit => submission(s).unwrap_or(Bucket::Submit),
        State::FenceWait => submission(s)
            .or(match s.relay {
                1 => Some(Bucket::GpuWait),
                2 => Some(Bucket::CopyBack),
                _ => None,
            })
            .or_else(|| ours(s))
            .unwrap_or(Bucket::FenceWait),
        State::TitleWait => ours(s).unwrap_or(Bucket::TitleWait),
        State::Title => ours(s).unwrap_or(Bucket::Unexplained),
    }
}

/// One frame being summed up.
pub(crate) struct Frame {
    pub presents: u64,
    pub began: u64,
    pub buckets: [u64; BUCKETS.len()],
    pub pre_list: u64,
    pub xmem: u64,
    /// The wall time with DirectStorage reads pending, and the part of it
    /// that went to an unknown bucket.
    pub ds_pending: u64,
    pub unknown_ds: u64,
    /// The presenting thread's processor time not yet given to ticks.
    credit: u64,
}

impl Frame {
    pub(crate) fn new(presents: u64, began: u64, credit: u64) -> Frame {
        Frame { presents, began, buckets: [0; BUCKETS.len()], pre_list: 0, xmem: 0, ds_pending: 0, unknown_ds: 0, credit }
    }

    /// The presenting thread ran `us` more.
    pub(crate) fn ran(&mut self, us: u64, frame: State) {
        // Its time in code of ours is that code's.
        self.credit = if frame == State::Title { (self.credit + us).min(50_000) } else { 0 };
    }

    /// `[from, to)` (microseconds) as `seen`.
    pub(crate) fn add(&mut self, seen: &Seen, from: u64, to: u64) {
        let mut us = to.saturating_sub(from);
        if seen.pre_list {
            self.pre_list += us;
        }
        if seen.xmem[0] || seen.xmem[1] {
            self.xmem += us;
        }
        if seen.frame == State::Title {
            let running = self.credit.min(us);
            self.credit -= running;
            self.buckets[Bucket::TitleRunning as usize] += running;
            us -= running;
        }
        let b = bucket(seen);
        self.buckets[b as usize] += us;
        if seen.ds_pending {
            self.ds_pending += to.saturating_sub(from);
            if matches!(b, Bucket::TitleWait | Bucket::FenceWait | Bucket::Unexplained) {
                self.unknown_ds += us;
            }
        }
    }

    /// The wall time so far, ended at `at`.
    pub(crate) fn wall(&self, at: u64) -> u64 {
        at.saturating_sub(self.began)
    }
}

/// The CSV's first line.
pub(crate) fn header() -> String {
    let mut h = String::from("frame,t_s,wall_ms,presents");
    for b in BUCKETS {
        h.push(',');
        h.push_str(b);
    }
    h.push_str(",pre_list_ms,xmem_active_ms,ds_pending_ms,unknown_ds_pending_ms,title_cpu_ms");
    for n in NAMES {
        h.push(',');
        match n.strip_suffix("_ns") {
            Some(stem) => {
                h.push_str(stem);
                h.push_str("_ms");
            }
            None => h.push_str(n),
        }
    }
    h.push_str(
        ",pipelines_made,pipelines_loaded,pipelines_making,textures_waiting,process_cpu_ms,page_faults,ticks,tick_max_ms",
    );
    h.push_str(",working_set_mb,private_mb,available_mb,page_reads,pages_input,shadows_live,shadow_mb");
    for i in 1..=TOP {
        let _ = std::fmt::Write::write_fmt(&mut h, format_args!(",list_top{i}_line,list_top{i}_ms,list_top{i}_calls"));
    }
    h.push_str(",menu_bits");
    h
}

/// What the looking thread keeps between frames.
struct Looker {
    counts: [u64; NAMES.len()],
    pipelines: (u64, u64),
    cpu: u64,
    faults: u64,
    /// The thread that presents: its id and a handle; its cycles so far.
    thread: (u32, isize, u64),
    /// Processor cycles a microsecond.
    cycles_per_us: f64,
    page_reads: Option<PageReads>,
    reads: (u64, u64),
    /// List calls by site so far: calls, nanoseconds.
    sites: [(u64, u64); SITES],
}

/// List call sites named per line, the slowest of the frame first.
const TOP: usize = 3;

fn ms(us: u64) -> String {
    format!("{:.2}", us as f64 / 1e3)
}

impl Looker {
    /// What is going on now, and the presenting thread's running time since
    /// the last look (microseconds).
    fn look(&mut self) -> (Seen, u64) {
        let slot = FRAME_SLOT.load(Relaxed);
        let (frame, thread) = SLOT.get(slot).map_or((State::Title, 0), |s| (State::of(s.state.load(Relaxed)), s.thread.load(Relaxed)));
        let mut seen = Seen {
            frame,
            phase: PHASE.load(Relaxed),
            relay: RELAY.load(Relaxed),
            xmem: [XMEM[0].load(Relaxed) > 0, XMEM[1].load(Relaxed) > 0],
            pipelines: PIPELINES.load(Relaxed) > 0,
            guards: GUARDS.load(Relaxed) > 0,
            recording: false,
            reading: READING.load(Relaxed) > 0,
            ds_pending: DS_PENDING.load(Relaxed) > 0,
            pre_list: crate::copystat::phase() == 0,
        };
        // Other threads' list calls, looked for only when they could matter.
        if matches!(seen.frame, State::Title | State::TitleWait | State::FenceWait) {
            let given = NEXT.load(Relaxed).min(SLOTS);
            seen.recording = SLOT[..given]
                .iter()
                .enumerate()
                .any(|(i, s)| i != slot && s.state.load(Relaxed) == State::List as u8);
        }
        (seen, self.ran(thread))
    }

    /// The presenting thread's running time since the last look.
    fn ran(&mut self, thread: u32) -> u64 {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenThread, THREAD_QUERY_LIMITED_INFORMATION};
        use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
        if thread == 0 {
            return 0;
        }
        if self.thread.0 != thread {
            if self.thread.1 != 0 {
                // SAFETY: our own handle.
                unsafe { CloseHandle(self.thread.1 as _) };
            }
            // SAFETY: a plain query.
            let handle = unsafe { OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, thread) } as isize;
            self.thread = (thread, handle, 0);
            let mut cycles = 0;
            // SAFETY: a handle of ours or null, and an out pointer.
            if handle != 0 && unsafe { QueryThreadCycleTime(handle as _, &mut cycles) } != 0 {
                self.thread.2 = cycles;
            }
            return 0;
        }
        let mut cycles = 0;
        // SAFETY: as above.
        if self.thread.1 == 0 || unsafe { QueryThreadCycleTime(self.thread.1 as _, &mut cycles) } == 0 {
            return 0;
        }
        let ran = cycles.saturating_sub(self.thread.2);
        self.thread.2 = cycles;
        (ran as f64 / self.cycles_per_us) as u64
    }

    /// The frame's line.
    fn row(&mut self, f: &Frame, at: u64, presents: u64, cpu_us: u64, ticks: (u64, u64)) -> String {
        use std::fmt::Write as _;
        let t = crate::overlay::run_start()
            .and_then(|start| (epoch() + Duration::from_micros(at)).checked_duration_since(start))
            .map_or(String::new(), |t| format!("{:.3}", t.as_secs_f64()));
        let mut row = format!("{},{t},{},{}", f.presents + 1, ms(f.wall(at)), presents - f.presents);
        for b in f.buckets {
            let _ = write!(row, ",{}", ms(b));
        }
        let _ = write!(row, ",{},{},{},{},{}", ms(f.pre_list), ms(f.xmem), ms(f.ds_pending), ms(f.unknown_ds), ms(cpu_us));
        for (i, name) in NAMES.iter().enumerate() {
            let now = total(i);
            let n = now - self.counts[i].min(now);
            self.counts[i] = now;
            if name.ends_with("_ns") {
                let _ = write!(row, ",{:.2}", n as f64 / 1e6);
            } else {
                let _ = write!(row, ",{n}");
            }
        }
        let (made, loaded) = crate::pipeline::made_loaded();
        let (cpu, faults) = (crate::copystat::process_cpu_us(), crate::copystat::page_faults());
        let _ = write!(
            row,
            ",{},{},{},{},{},{},{},{}",
            made - self.pipelines.0.min(made),
            loaded - self.pipelines.1.min(loaded),
            crate::pipeline::making(),
            crate::draw::HEAP_TEXTURES_WAITING.load(Relaxed),
            ms(cpu - self.cpu.min(cpu)),
            faults - self.faults.min(faults),
            ticks.0,
            ms(ticks.1),
        );
        self.pipelines = (made, loaded);
        (self.cpu, self.faults) = (cpu, faults);
        if more() {
            let (working, private, available) = memory_mb();
            let reads = self.page_reads.as_ref().map_or((0, 0), PageReads::read);
            let _ = write!(
                row,
                ",{working},{private},{available},{},{}",
                reads.0 - self.reads.0.min(reads.0),
                reads.1 - self.reads.1.min(reads.1)
            );
            self.reads = reads;
        } else {
            row.push_str(",,,,,");
        }
        let (shadows, bytes) = crate::shadow::live();
        let _ = write!(row, ",{shadows},{}", bytes >> 20);
        let mut sites: Vec<(u64, u64, u32)> = (0..SITES)
            .filter_map(|i| {
                let line = SITE_LINE[i].load(Relaxed);
                let now = (0..BANKS).fold((0, 0), |(c, n), b| (c + SITE_CALLS[b][i].load(Relaxed), n + SITE_NS[b][i].load(Relaxed)));
                let was = std::mem::replace(&mut self.sites[i], now);
                (line != 0 && now.1 > was.1).then(|| (now.1 - was.1, now.0 - was.0.min(now.0), line))
            })
            .collect();
        sites.sort_unstable_by_key(|s| std::cmp::Reverse(s.0));
        for i in 0..TOP {
            let (ns, calls, line) = sites.get(i).copied().unwrap_or((0, 0, 0));
            let _ = write!(row, ",{line},{:.2},{calls}", ns as f64 / 1e6);
        }
        // The switches menu's state at the line (empty without the menu).
        row.push(',');
        if crate::ab::menu() {
            let _ = write!(row, "{}", crate::ab::menu_bits());
        }
        row
    }
}

/// Processor cycles a microsecond, measured against the clock.
fn cycles_per_us() -> f64 {
    // SAFETY: reads the time-stamp counter, which the system's cycle times count.
    let tsc = || unsafe { core::arch::x86_64::_rdtsc() };
    let (t0, c0) = (Instant::now(), tsc());
    std::thread::sleep(Duration::from_millis(100));
    let (us, cycles) = (t0.elapsed().as_micros() as f64, tsc().wrapping_sub(c0) as f64);
    if us > 0.0 && cycles > 0.0 { cycles / us } else { 3000.0 }
}

/// Starts the looking thread.
fn start() {
    let Some(path) = path() else {
        return;
    };
    let file = match std::fs::File::create(&path) {
        Ok(file) => file,
        Err(e) => {
            tracing::warn!(error = %e, "the frame log could not be made (VICTORY_FRAME_LOG)");
            return;
        }
    };
    let _ = epoch();
    match std::thread::Builder::new().name("victory-frame-log".into()).spawn(move || look(file)) {
        Ok(_) => tracing::info!("the frame log begins (VICTORY_FRAME_LOG)"),
        Err(e) => tracing::warn!(error = %e, "the frame log's thread did not start"),
    }
}

/// The looking thread: a look about every millisecond, a line per present.
fn look(file: std::fs::File) {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CreateWaitableTimerExW, GetCurrentThread, INFINITE,
        SetThreadPriority, SetWaitableTimer, THREAD_PRIORITY_TIME_CRITICAL, TIMER_ALL_ACCESS, WaitForSingleObject,
    };
    // SAFETY: plain calls; a timer of our own.
    let timer = unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
        CreateWaitableTimerExW(
            std::ptr::null(),
            std::ptr::null(),
            CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
            TIMER_ALL_ACCESS,
        )
    };
    if timer.is_null() {
        tracing::warn!("no high-resolution timer: the frame log looks every 1-16 ms");
    }
    let tick = || {
        // A millisecond from now, in 100 ns.
        let due: i64 = -10_000;
        // SAFETY: our timer; no routine.
        let waited = !timer.is_null()
            && unsafe {
                SetWaitableTimer(timer, &due, 0, None, std::ptr::null(), 0) != 0
                    && WaitForSingleObject(timer, INFINITE) == WAIT_OBJECT_0
            };
        if !waited {
            std::thread::sleep(Duration::from_millis(1));
        }
    };
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let _ = writeln!(out, "{}", header());
    let mut looker = Looker {
        counts: std::array::from_fn(total),
        pipelines: crate::pipeline::made_loaded(),
        cpu: crate::copystat::process_cpu_us(),
        faults: crate::copystat::page_faults(),
        thread: (0, 0, 0),
        cycles_per_us: cycles_per_us(),
        page_reads: if more() { PageReads::new() } else { None },
        reads: (0, 0),
        sites: [(0, 0); SITES],
    };
    looker.reads = looker.page_reads.as_ref().map_or((0, 0), PageReads::read);
    let mut last = now_us();
    let mut frame = Frame::new(PRESENTS.load(Acquire), last, 0);
    let (mut cpu, mut ticks, mut longest) = (0u64, 0u64, 0u64);
    let mut flushed = Instant::now();
    loop {
        tick();
        let now = now_us();
        let (seen, ran) = looker.look();
        frame.ran(ran, seen.frame);
        cpu += ran;
        ticks += 1;
        longest = longest.max(now - last);
        let presents = PRESENTS.load(Acquire);
        if presents != frame.presents {
            let at = PRESENT_US.load(Relaxed).clamp(last, now);
            frame.add(&seen, last, at);
            let line = looker.row(&frame, at, presents, cpu, (ticks, longest));
            let _ = writeln!(out, "{line}");
            frame = Frame::new(presents, at, frame.credit);
            frame.add(&seen, at, now);
            (cpu, ticks, longest) = (0, 0, 0);
        } else {
            frame.add(&seen, last, now);
        }
        last = now;
        // play.ps1 closes the window at the end of a run: a second at most lost.
        if flushed.elapsed() >= Duration::from_secs(1) {
            let _ = out.flush();
            flushed = Instant::now();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(frame: State) -> Seen {
        Seen {
            frame,
            phase: 0,
            relay: 0,
            xmem: [false; 2],
            pipelines: false,
            guards: false,
            recording: false,
            reading: false,
            ds_pending: false,
            pre_list: false,
        }
    }

    /// A wait of the title's goes to what of ours goes on, the submission
    /// first; with nothing of ours it is the title's wait.
    #[test]
    fn waits_go_to_what_of_ours_runs() {
        let mut s = seen(State::TitleWait);
        assert_eq!(bucket(&s), Bucket::TitleWait);
        s.reading = true;
        assert_eq!(bucket(&s), Bucket::Dstorage);
        s.recording = true;
        assert_eq!(bucket(&s), Bucket::Recording);
        s.guards = true;
        assert_eq!(bucket(&s), Bucket::Guards);
        s.pipelines = true;
        assert_eq!(bucket(&s), Bucket::Pipelines);
        s.xmem[1] = true;
        assert_eq!(bucket(&s), Bucket::XmemQueue);
        s.xmem[0] = true;
        assert_eq!(bucket(&s), Bucket::XmemTitle);
        s.phase = Phase::Preparing as u8;
        assert_eq!(bucket(&s), Bucket::Preparing);
        s.phase = Phase::Landing as u8;
        s.relay = 2;
        assert_eq!(bucket(&s), Bucket::CopyBack);
        s.relay = 1;
        assert_eq!(bucket(&s), Bucket::GpuWait);
    }

    /// The presenting thread's own state of ours wins over what goes on
    /// elsewhere; the title's code with nothing seen is unexplained.
    #[test]
    fn own_states_win() {
        let mut s = seen(State::List);
        s.xmem = [true, true];
        s.phase = 1;
        assert_eq!(bucket(&s), Bucket::Recording);
        s.frame = State::Pace;
        assert_eq!(bucket(&s), Bucket::Pace);
        s.frame = State::Present;
        assert_eq!(bucket(&s), Bucket::Preparing);
        s.phase = 0;
        assert_eq!(bucket(&s), Bucket::Present);
        s.frame = State::Submit;
        assert_eq!(bucket(&s), Bucket::Submit);
        assert_eq!(bucket(&seen(State::Title)), Bucket::Unexplained);
        let mut fence = seen(State::FenceWait);
        assert_eq!(bucket(&fence), Bucket::FenceWait);
        fence.relay = 1;
        assert_eq!(bucket(&fence), Bucket::GpuWait);
        // The landing thread waiting for the card is no reason for a title's
        // wait on its own threads.
        let mut wait = seen(State::TitleWait);
        wait.relay = 1;
        assert_eq!(bucket(&wait), Bucket::TitleWait);
    }

    /// The buckets of a frame add up to its wall time, the presenting
    /// thread's processor time taking ticks of the title's code first.
    #[test]
    fn buckets_add_up_to_the_wall_time() {
        let mut f = Frame::new(0, 1_000, 0);
        let mut s = seen(State::Title);
        f.ran(300, State::Title);
        f.add(&s, 1_000, 2_000);
        s.frame = State::TitleWait;
        s.xmem[0] = true;
        s.pre_list = true;
        f.add(&s, 2_000, 2_750);
        f.ran(400, State::List);
        s.frame = State::Title;
        s.pre_list = false;
        f.add(&s, 2_750, 3_000);
        assert_eq!(f.buckets.iter().sum::<u64>(), f.wall(3_000));
        assert_eq!(f.buckets[Bucket::TitleRunning as usize], 300);
        assert_eq!(f.buckets[Bucket::Unexplained as usize], 700);
        // In code of ours its running time is not the title's.
        assert_eq!(f.buckets[Bucket::XmemTitle as usize], 750 + 250);
        assert_eq!((f.pre_list, f.xmem), (750, 1_000));
    }

    /// Write-combined ranges are cut by later allocations and frees, and
    /// a copy is split by its write-combined bytes.
    #[test]
    fn write_combined_ranges() {
        let mut map = std::collections::BTreeMap::new();
        map.insert(100, 200);
        map.insert(300, 400);
        assert_eq!(wc_bytes(&map, 150, 350), 50 + 50);
        assert_eq!(segment(&map, 150), (100, 200, true));
        assert_eq!(segment(&map, 250), (200, 300, false));
        assert_eq!(segment(&map, 50), (0, 100, false));
        assert_eq!(segment(&map, 400), (400, usize::MAX, false));
        cut(&mut map, 120, 330);
        assert_eq!(map.into_iter().collect::<Vec<_>>(), vec![(100, 120), (330, 400)]);
    }

    /// The header has a name for every number of a line.
    #[test]
    fn the_header_names_every_column() {
        let h = header();
        assert_eq!(h.split(',').count(), 4 + BUCKETS.len() + 5 + NAMES.len() + 8 + 7 + 3 * TOP + 1);
        assert!(h.contains(",xmem_title_ms,") && h.contains(",unexplained,") && !h.contains("_ns"));
    }
}
