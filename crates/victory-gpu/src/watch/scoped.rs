//! Reads of known title memory ranges, and changes of the views that hold
//! them (`VICTORY_TEXTURE_SCOPED_REMAP=1`): a reader waits only for a change
//! that overlaps its range. A change announces its ranges before it waits
//! for the readers there, so new overlapping readers cannot starve it.
//! No range known: [`ALL`], which overlaps everything.
//!
//! Ported from fablinoo/kelvaroq af560cd (GPL-3.0),
//! `native/patches/0037-texture-remap-ranges.patch`; a change here holds
//! several ranges (a batch of pages), not one.

use std::ops::Range;
use std::sync::{Condvar, Mutex};

pub(super) const ALL: Range<usize> = 0..usize::MAX;
pub(super) static GATE: Gate = Gate::new();

#[derive(Default)]
struct State {
    readers: Vec<Range<usize>>,
    writers: Vec<Range<usize>>,
}

pub(super) struct Gate {
    state: Mutex<State>,
    changed: Condvar,
}

fn overlaps(active: &[Range<usize>], areas: &[Range<usize>]) -> bool {
    active.iter().any(|a| areas.iter().any(|b| a.start < b.end && b.start < a.end))
}

/// A reader's or a change's place until dropped.
pub(super) struct Permit<'a> {
    gate: &'a Gate,
    areas: Vec<Range<usize>>,
    writer: bool,
}

impl Gate {
    const fn new() -> Self {
        Self { state: Mutex::new(State { readers: Vec::new(), writers: Vec::new() }), changed: Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `None` if `wait` is off and a change overlaps `area`. The mutex is
    /// only bookkeeping: never held across a mapping, a copy or a callback.
    pub(super) fn read(&self, area: Range<usize>, wait: bool) -> Option<Permit<'_>> {
        let areas = vec![area];
        let mut state = self.lock();
        if overlaps(&state.writers, &areas) {
            if !wait {
                return None;
            }
            state = self.changed.wait_while(state, |s| overlaps(&s.writers, &areas)).unwrap_or_else(|e| e.into_inner());
        }
        state.readers.extend(areas.iter().cloned());
        Some(Permit { gate: self, areas, writer: false })
    }

    /// Announces the change, then waits for the readers of its ranges.
    pub(super) fn write(&self, areas: Vec<Range<usize>>) -> Permit<'_> {
        let mut state = self.lock();
        state.writers.extend(areas.iter().cloned());
        let permit = Permit { gate: self, areas, writer: true };
        drop(self.changed.wait_while(state, |s| overlaps(&s.readers, &permit.areas)).unwrap_or_else(|e| e.into_inner()));
        permit
    }
}

impl Drop for Permit<'_> {
    fn drop(&mut self) {
        let mut state = self.gate.lock();
        let active = if self.writer { &mut state.writers } else { &mut state.readers };
        for area in &self.areas {
            if let Some(at) = active.iter().position(|a| a == area) {
                active.swap_remove(at);
            }
        }
        self.gate.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    const A: Range<usize> = 0x10000..0x20000;
    const B: Range<usize> = 0x40000..0x50000;

    #[test]
    fn a_disjoint_reader_and_a_change_proceed_together() {
        let gate = Gate::new();
        let reader = gate.read(A, true).unwrap();
        // Neither side waits: the change is there with A's reader still in.
        let change = gate.write(vec![B]);
        assert!(gate.read(0x20000..0x30000, false).is_some());
        assert!(gate.read(0x50000..0x60000, false).is_some());
        drop(change);
        drop(reader);
    }

    #[test]
    fn an_overlapping_reader_waits_for_the_change() {
        let gate = Gate::new();
        let change = gate.write(vec![0x10000..0x40000]);
        assert!(gate.read(0x30000..0x40000, false).is_none());
        let (entered, got) = mpsc::channel();
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let _reader = gate.read(0x30000..0x40000, true).unwrap();
                entered.send(()).unwrap();
            });
            assert!(got.recv_timeout(Duration::from_millis(100)).is_err(), "the reader got in during the change");
            drop(change);
            got.recv_timeout(Duration::from_secs(5)).expect("the reader never got in");
        });
    }

    #[test]
    fn a_change_waits_for_an_overlapping_reader_only() {
        let gate = Gate::new();
        let reader = gate.read(A, true).unwrap();
        let (done, got) = mpsc::channel();
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let _change = gate.write(vec![B, 0x18000..0x30000]);
                done.send(()).unwrap();
            });
            assert!(got.recv_timeout(Duration::from_millis(100)).is_err(), "the change ran with A read");
            // Announced: a new reader of A waits behind it, B's does not.
            assert!(gate.read(A, false).is_none());
            drop(reader);
            got.recv_timeout(Duration::from_secs(5)).expect("the change never ran");
        });
    }

    #[test]
    fn a_change_of_unknown_range_blocks_every_reader() {
        let gate = Gate::new();
        let change = gate.write(vec![ALL]);
        assert!(gate.read(A, false).is_none());
        assert!(gate.read(B, false).is_none());
        assert!(gate.read(ALL, false).is_none());
        drop(change);
        assert!(gate.read(A, false).is_some());
        // And it waits for every reader: a global one is in the way.
        let reader = gate.read(B, true).unwrap();
        let (done, got) = mpsc::channel();
        std::thread::scope(|threads| {
            threads.spawn(|| {
                let _change = gate.write(vec![ALL]);
                done.send(()).unwrap();
            });
            assert!(got.recv_timeout(Duration::from_millis(100)).is_err());
            drop(reader);
            got.recv_timeout(Duration::from_secs(5)).expect("the change never ran");
        });
    }
}
