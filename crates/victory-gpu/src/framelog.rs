//! Diagnostics: one line per frame that says where the frame's wall time
//! went (`VICTORY_FRAME_LOG=<file>`, a CSV; off by default, and nothing of
//! it in a build without `diagnostics`).
//!
//! Every thread keeps one byte that says what of ours it is in ([`State`]:
//! a list call, a submission, a map call, a wait of the title's...), and a
//! few global words say what the submission under the lock and the thread
//! that lands flights are doing. Setting them is a store to a line of the
//! thread's own. A thread of ours looks at them about every millisecond
//! (a high-resolution waitable timer, no change of the system's timer) and
//! gives the time since its last look to one bucket ([`Bucket`]), chosen
//! from the state of the thread that presents and, while that thread waits
//! or the title runs nothing on it, from what of ours is going on anywhere.
//! The intervals are cut at each present's time, so a frame's buckets add up
//! to its wall time exactly; what nothing explains is a bucket of its own.
//!
//! Beside them, exact counts of what the title asked for in the frame (map
//! calls and pages, its waits, textures, pipelines, DirectStorage reads),
//! summed over all threads, and gauges. Numbers only: no name, no path.

#[cfg(all(windows, feature = "diagnostics"))]
mod imp;

#[cfg(all(windows, feature = "diagnostics"))]
pub use imp::{Reading, Xmem, XmemCall, ds_pending, dstorage, on, reading, xmem};
#[cfg(all(windows, feature = "diagnostics"))]
pub(crate) use imp::{
    BY_FENCE, BY_TITLE, K, Phase, State, count, enter, fence_event, phase, presented, presenting, relay, title_wait, waiting_guards, woke,
    waiting_pipeline,
};

/// What a map call is for.
#[cfg(not(all(windows, feature = "diagnostics")))]
#[derive(Clone, Copy)]
pub enum Xmem {
    /// The title's own `XMemMapPhysicalPages(Scatter)`.
    Title,
    /// A queue's `CopyPageMappings`.
    Queue,
}

/// A map call, counted while it lives.
#[cfg(not(all(windows, feature = "diagnostics")))]
pub struct XmemCall;

#[cfg(not(all(windows, feature = "diagnostics")))]
impl XmemCall {
    /// The call maps `n` pages more than it said when it began.
    #[inline(always)]
    pub fn pages(&mut self, _n: u64) {}
}

/// Whether the frame log is on.
#[cfg(not(all(windows, feature = "diagnostics")))]
#[inline(always)]
pub fn on() -> bool {
    false
}

/// A map call of `pages` pages begins.
#[cfg(not(all(windows, feature = "diagnostics")))]
#[inline(always)]
pub fn xmem(_kind: Xmem, _pages: u64) -> XmemCall {
    XmemCall
}

/// A DirectStorage read, counted while it lives.
#[cfg(not(all(windows, feature = "diagnostics")))]
pub struct Reading;

/// A DirectStorage read begins.
#[cfg(not(all(windows, feature = "diagnostics")))]
#[inline(always)]
pub fn reading() -> Reading {
    Reading
}

/// `n` DirectStorage reads are submitted and not done.
#[cfg(not(all(windows, feature = "diagnostics")))]
#[inline(always)]
pub fn ds_pending(_n: u32) {}

/// A DirectStorage read of `bytes` is done.
#[cfg(not(all(windows, feature = "diagnostics")))]
#[inline(always)]
pub fn dstorage(_bytes: u64, _title_ns: u64, _queue_ns: u64, _service_ns: u64) {}
