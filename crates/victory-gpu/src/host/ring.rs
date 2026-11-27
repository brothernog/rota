//! The ring of pieces on their way back (`VICTORY_CARD_RING`; off by
//! default). The compare at the end of a flight ([`CARD_COMPARE`] with
//! `CARD_RING`, told 3) also copies each piece it flags, whole as it hashed
//! it, into a slot of a buffer the CPU maps, and writes the slot's number
//! into the ring's index at the piece's dword of the compare's results. It
//! runs on the drawing queue behind everything the flight wrote and ahead of
//! all that was sent after it, so a slot holds what the flight ended with:
//! once its fence has passed the bytes are in PC memory, with no fetch.
//!
//! Slot numbers only grow (31 bits, then round again); slot `s` lies at
//! `s % slots`. The compare takes one only while it is less than `slots`
//! past the tail it was given, the first number not landed yet, which the
//! CPU reads when it records the compare: an old tail only makes more pieces
//! not fit, never a slot taken from under a landing. A piece that did not fit
//! gets its number with the top bit set, and the tail moves past it too.
//!
//! `VICTORY_CARD_RING=3`, the shadow: the ring is filled and checked against
//! the compare's hashes and against what the fetch brings, and the fetch
//! still lands everything.
//! `VICTORY_CARD_RING_MB`: its size (128 by default), as slots of 64 KiB,
//! rounded down to a power of two.

// shortcut: nothing calls these until the shadow is wired in.
#![allow(dead_code)]

use super::*;

/// Bytes of the index ahead of the slots: as the compare's results.
pub(super) const INDEX: usize = 1 << 20;
/// A slot: 64 KiB and the 16-byte widening of the hash at both ends.
pub(super) const SLOT: usize = 65_600;
/// Of an index entry: the number did not fit.
pub(super) const FULL: u32 = 0x8000_0000;
const NUMBER: u32 = 0x7FFF_FFFF;
/// The most slots: the end of the last one fits in the shader's 32 bits.
const MOST: u32 = 1 << 15;

/// `VICTORY_CARD_RING`: 3 is the shadow; anything else is off.
pub(super) fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_CARD_RING").as_deref() {
        Ok("3") => 3,
        Ok("" | "0") | Err(_) => 0,
        Ok(other) => {
            tracing::warn!(value = other, "VICTORY_CARD_RING: only 3 (the shadow) is made so far; the ring is off");
            0
        }
    })
}

/// The ring's slots (`VICTORY_CARD_RING_MB`).
pub(super) fn slots() -> u32 {
    let mb = std::env::var("VICTORY_CARD_RING_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(128);
    slots_for(mb)
}

/// Slots of 64 KiB in `mb` megabytes, rounded down to a power of two (the
/// numbers' 31 bits go round the ring evenly), at least one.
fn slots_for(mb: u64) -> u32 {
    let n = mb.saturating_mul(16).clamp(1, u64::from(MOST)) as u32;
    1 << (31 - n.leading_zeros())
}

/// The ring's bytes, index and slots.
pub(super) fn bytes(slots: u32) -> u64 {
    (INDEX + slots as usize * SLOT) as u64
}

/// Where in the ring the slot an index entry names lies; `None` where the
/// piece did not fit.
pub(super) fn slot_at(entry: u32, slots: u32) -> Option<usize> {
    (entry & FULL == 0).then(|| INDEX + (entry & (slots - 1)) as usize * SLOT)
}

/// The first slot number not landed, once a landing has seen `entry` (fit
/// or not): past it if it is not behind `tail`.
pub(super) fn tail_after(tail: u32, entry: u32) -> u32 {
    let s = entry & NUMBER;
    if s.wrapping_sub(tail) & NUMBER < 1 << 30 { (s + 1) & NUMBER } else { tail }
}

/// The first slot number not landed yet: what the compare is given.
pub(super) static TAIL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Of ranges a plan compared, each its first dword and its pieces: the
/// pieces flagged, in order, as (the range, the piece in it, its index
/// entry, its result). A piece under two ranges may be there twice, or once
/// (the second compare found the hash the first stored). Pieces past the
/// results are left out.
pub(super) fn flagged(ranges: &[(u32, u32)], differs: &[u32], index: &[u32]) -> Vec<(usize, u32, u32, u32)> {
    let mut out = Vec::new();
    for (r, &(at, pieces)) in ranges.iter().enumerate() {
        for p in 0..pieces {
            let i = (at + p) as usize;
            if let (Some(&d), Some(&entry)) = (differs.get(i), index.get(i))
                && torn::flagged(d)
            {
                out.push((r, p, entry, d));
            }
        }
    }
    out
}

/// Where each piece (its copy, its number) is in the ring, the last
/// landing's where several landings have it: sorted.
#[derive(Default)]
pub(super) struct Placed(Vec<((usize, u32), usize)>);

impl Placed {
    /// From the pieces found in the ring, in the order of landing.
    pub(super) fn new(found: impl IntoIterator<Item = ((usize, u32), usize)>) -> Placed {
        let mut all: Vec<((usize, u32), usize)> = found.into_iter().collect();
        // Stable: the last of a piece stays last.
        all.sort_by_key(|f| f.0);
        let mut placed: Vec<((usize, u32), usize)> = Vec::with_capacity(all.len());
        for f in all {
            match placed.last_mut() {
                Some(last) if last.0 == f.0 => *last = f,
                _ => placed.push(f),
            }
        }
        Placed(placed)
    }

    /// Where in the ring the part from `lo` of a piece lies, the piece's
    /// slot holding it from `from` (`Shadow::card_hashed`) on.
    pub(super) fn part_at(&self, piece: (usize, u32), lo: usize, from: usize) -> Option<usize> {
        let i = self.0.binary_search_by_key(&piece, |p| p.0).ok()?;
        Some(self.0[i].1 + (lo - from))
    }
}

/// Since the last frame-rate line: pieces found in the ring, their bytes,
/// pieces that did not fit, pieces whose bytes are not what the compare
/// hashed, parts the fetch brought that were looked for in the ring, and
/// those of them not as the ring has them.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_number_wraps_into_the_ring() {
        assert_eq!(slot_at(0, 4), Some(INDEX));
        assert_eq!(slot_at(5, 4), Some(INDEX + SLOT));
        assert_eq!(slot_at(NUMBER, 4), Some(INDEX + 3 * SLOT));
        assert_eq!(slot_at(FULL | 1, 4), None);
        // 128 MB: 2,048 slots (128.1 MiB with the widening), not 1,024.
        assert_eq!(slots_for(128), 2048);
        assert_eq!(slots_for(127), 1024);
        assert_eq!(slots_for(0), 1);
        assert_eq!(slots_for(u64::MAX), MOST);
        assert!(bytes(MOST) <= u64::from(u32::MAX));
    }

    #[test]
    fn the_tail_follows_the_last_number_seen_also_one_that_did_not_fit() {
        let tail = [2, FULL | 5, 3].into_iter().fold(0, tail_after);
        assert_eq!(tail, 6);
        // Round 31 bits.
        assert_eq!(tail_after(NUMBER - 1, NUMBER), 0);
        assert_eq!(tail_after(NUMBER, 0), 1);
        assert_eq!(tail_after(NUMBER, FULL), 1);
        // Behind it: as it was.
        assert_eq!(tail_after(10, 9), 10);
        assert_eq!(tail_after(1, NUMBER), 1);
    }

    #[test]
    fn what_did_not_fit_or_was_not_flagged_is_left_to_the_fetch() {
        // Two ranges: dwords 0-2 and 3-4; 5 is past the results.
        let differs = [0, 1, 3, 2, 5];
        let index = [7, 4, FULL | 5, 7, 6];
        let found = flagged(&[(0, 3), (3, 3)], &differs, &index);
        assert_eq!(found, [(0, 1, 4, 1), (0, 2, FULL | 5, 3), (1, 1, 6, 5)]);
        let slots: Vec<Option<usize>> = found.iter().map(|f| slot_at(f.2, 8)).collect();
        assert_eq!(slots, [Some(INDEX + 4 * SLOT), None, Some(INDEX + 6 * SLOT)]);
    }

    /// Two ranges of a plan hold parts of one piece; the second compare
    /// found the hash the first stored and flagged nothing. One slot holds
    /// both parts.
    #[test]
    fn a_piece_under_two_ranges_lands_both_parts_from_one_slot() {
        let found = flagged(&[(0, 1), (1, 1)], &[1, 0], &[3, 7]);
        assert_eq!(found.len(), 1);
        let copy = 0xC0;
        let placed = Placed::new(found.iter().map(|f| ((copy, 9), slot_at(f.2, 4).unwrap())));
        let from = 9 * 65536 - 32;
        assert_eq!(placed.part_at((copy, 9), from + 100, from), Some(INDEX + 3 * SLOT + 100));
        assert_eq!(placed.part_at((copy, 9), from + 40_000, from), Some(INDEX + 3 * SLOT + 40_000));
        assert_eq!(placed.part_at((copy, 8), from, from), None);
    }

    /// A slot holds its piece from where the hash began (widened to 16
    /// bytes); of two landings that have a piece, the later's slot stands.
    #[test]
    fn a_part_is_read_where_its_piece_starts() {
        let placed = Placed::new([((1, 2), INDEX), ((1, 3), INDEX + SLOT), ((1, 2), INDEX + 2 * SLOT)]);
        // A copy that starts 1,005 bytes into its piece 0: piece 2 is hashed
        // from offset 130,067 rounded down to 130,064.
        assert_eq!(placed.part_at((1, 2), 130_067, 130_064), Some(INDEX + 2 * SLOT + 3));
        assert_eq!(placed.part_at((1, 3), 200_000, 195_600), Some(INDEX + SLOT + 4_400));
    }
}
