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
//! `VICTORY_CARD_RING=4`: pieces land from the ring. Each plan's pieces land
//! in plan order, each with title bytes sent after its own send left out
//! (`CardPlan::sent_as`: the slot holds what its flight ended with). A piece
//! that did not fit, whose slot is not what the compare hashed, or that the
//! compare did not look at, is fetched as before, and lands after the ring.
//! `VICTORY_CARD_RING_MB`: its size (128 by default), as slots of 64 KiB,
//! rounded down to a power of two.

use super::*;

/// Bytes of the index ahead of the slots: as the compare's results.
pub(super) const INDEX: usize = 1 << 20;
/// A slot: 64 KiB and the 16-byte widening of the hash at both ends,
/// rounded up to 256 bytes so every slot starts on whole cache lines (at
/// 65,600 half the slots started mid-line: the card wrote PC memory in
/// partial lines). The compare's source has it as a literal.
pub(super) const SLOT: usize = 65_792;
/// Of an index entry: the number did not fit.
pub(super) const FULL: u32 = 0x8000_0000;
const NUMBER: u32 = 0x7FFF_FFFF;
/// The most slots: the end of the last one fits in the shader's 32 bits.
const MOST: u32 = 1 << 15;

/// `VICTORY_CARD_RING`: 3 is the shadow, 4 lands from the ring; anything
/// else is off.
pub(super) fn mode() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_CARD_RING").as_deref() {
        Ok("3") => 3,
        Ok("4") => 4,
        Ok("" | "0") | Err(_) => 0,
        Ok(other) => {
            tracing::warn!(value = other, "VICTORY_CARD_RING: only 3 (the shadow) and 4 (landing from it) are made; the ring is off");
            0
        }
    })
}

/// Whether pieces land from the ring (`VICTORY_CARD_RING=4`).
pub(super) fn lands() -> bool {
    mode() == 4
}

/// The ring's slots (`VICTORY_CARD_RING_MB`).
pub(super) fn slots() -> u32 {
    let mb = std::env::var("VICTORY_CARD_RING_MB").ok().and_then(|v| v.parse().ok()).unwrap_or(128);
    slots_for(mb)
}

/// `VICTORY_CARD_RING_WC=1`: the ring in write-combined PC memory (the
/// compares with the ring took ~185 ms a frame on an RTX 3050 writing it
/// write-back).
fn write_combined() -> bool {
    std::env::var_os("VICTORY_CARD_RING_WC").is_some_and(|v| v == "1")
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
/// hashed, parts the fetch brought that were looked for in the ring, those
/// of them not as the ring has them, nanoseconds the compares with the ring
/// took on the card, and the compares timed. Then, landing from the ring
/// ([`lands`]): parts landed from it, parts left to the fetch, microseconds
/// merging from the ring, microseconds looking through it (index and
/// hashes), landings with no fetch, and landings.
pub(super) static STATS: [std::sync::atomic::AtomicU64; 14] = [const { std::sync::atomic::AtomicU64::new(0) }; 14];

/// Compares with the ring whose card time is taken: pairs of timestamps.
const STAMPED: u32 = 1024;

/// What the host keeps for the ring (`CardWork::ring`).
pub(super) struct CardRing {
    pub(super) root: ID3D12RootSignature,
    pub(super) pso: ID3D12PipelineState,
    /// Index and slots: PC memory, mapped (buffer, mapping).
    buffer: (ID3D12Resource, usize),
    /// The next slot number, on the card.
    counter: ID3D12Resource,
    slots: u32,
    /// Timestamps ahead of and behind each compare with the ring, the
    /// memory they come back to (mapped), and the card's ticks a second.
    stamps: (ID3D12QueryHeap, ID3D12Resource, usize, u64),
    /// The next pair of them.
    next: u32,
}

impl CardRing {
    /// A pair of timestamps for the next compare with the ring.
    pub(super) fn stamp(&mut self) -> u32 {
        let at = self.next;
        self.next = (at + 1) % STAMPED;
        at
    }

    /// Before the compare's passes (its root signature set): the ring's
    /// buffers, and the first timestamp.
    ///
    /// # Safety
    /// The list is open.
    pub(super) unsafe fn begin(&self, list: &ID3D12GraphicsCommandList, stamp: u32) {
        // SAFETY: the caller's; live buffers.
        unsafe {
            list.SetComputeRootUnorderedAccessView(4, self.buffer.0.GetGPUVirtualAddress());
            list.SetComputeRootUnorderedAccessView(5, self.counter.GetGPUVirtualAddress());
            list.EndQuery(&self.stamps.0, D3D12_QUERY_TYPE_TIMESTAMP, 2 * stamp);
        }
    }

    /// After them: the second timestamp, and both on their way back.
    ///
    /// # Safety
    /// The list is open.
    pub(super) unsafe fn end(&self, list: &ID3D12GraphicsCommandList, stamp: u32) {
        // SAFETY: the caller's; the mapping holds `STAMPED` pairs.
        unsafe {
            list.EndQuery(&self.stamps.0, D3D12_QUERY_TYPE_TIMESTAMP, 2 * stamp + 1);
            list.ResolveQueryData(&self.stamps.0, D3D12_QUERY_TYPE_TIMESTAMP, 2 * stamp, 2, &self.stamps.1, u64::from(stamp) * 16);
        }
    }

    /// The tail and the ring's slots, as the compare takes them.
    pub(super) fn told(&self) -> (u32, u32) {
        (TAIL.load(Ordering::Relaxed), self.slots)
    }
}

impl Host {
    /// The ring's pipeline and buffers ([`mode`] not 0).
    pub(super) fn make_card_ring(&self) -> windows::core::Result<CardRing> {
        let empty = windows::core::Error::empty;
        let (root, pso) = self.make_compare(&card_compare_ring())?;
        // (Not the compare's name: the replay reads that one's constants.)
        #[cfg(feature = "diagnostics")]
        name(&pso, || "victory: card compare with the ring".into());
        let slots = slots();
        let page = if write_combined() { D3D12_CPU_PAGE_PROPERTY_WRITE_COMBINE } else { D3D12_CPU_PAGE_PROPERTY_WRITE_BACK };
        let (buffer, data) = self.shared_buffer_paged(bytes(slots), page).ok_or_else(empty)?;
        name(&buffer, || "card ring".into());
        let counter = self.uav_buffer(64).ok_or_else(empty)?;
        name(&counter, || "card ring, next slot".into());
        let (back, at) = self.shared_buffer(16 * u64::from(STAMPED)).ok_or_else(empty)?;
        // SAFETY: plain object creation; a fresh mapping of that size.
        let (heap, frequency) = unsafe {
            std::ptr::write_bytes(at, 0, 16 * STAMPED as usize);
            let mut heap: Option<ID3D12QueryHeap> = None;
            self.device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 2 * STAMPED,
                    NodeMask: 0,
                },
                &mut heap,
            )?;
            (heap.ok_or_else(empty)?, self.queue.GetTimestampFrequency().unwrap_or(0))
        };
        tracing::info!(
            slots,
            mb = bytes(slots) >> 20,
            mode = mode(),
            "the card ring (VICTORY_CARD_RING: 3 is the shadow, landing still fetches; 4 lands from it)"
        );
        Ok(CardRing {
            root,
            pso,
            buffer: (buffer, data as usize),
            counter,
            slots,
            stamps: (heap, back, at as usize, frequency),
            next: 0,
        })
    }
}

/// What a landing found in the ring, until the fetch has come.
pub(super) struct Look {
    /// Every piece found, for the shadow's check of the fetch.
    placed: Placed,
    /// By plan (in the order given): the pieces its compare put in the ring
    /// as it hashed them, which may land from there.
    own: Vec<Placed>,
    /// The ring's mapping.
    pub(super) data: usize,
    /// The first number not landed once these plans are.
    tail: u32,
}

/// Of one plan's flagged pieces (`flagged`'s), with each piece's copy and
/// number and the bytes its slot holds (`piece`, given the range and the
/// piece in it): each found in `ring` (the mapping, index first), with its
/// slot and whether the slot holds what the compare hashed. Pieces that did
/// not fit are left out. The tail moves past every one.
pub(super) fn sort_out(
    found: &[(usize, u32, u32, u32)],
    ring: &[u8],
    slots: u32,
    tail: &mut u32,
    mut piece: impl FnMut(usize, u32) -> ((usize, u32), usize),
) -> Vec<((usize, u32), usize, bool)> {
    let mut out = Vec::with_capacity(found.len());
    for &(r, p, entry, result) in found {
        *tail = tail_after(*tail, entry);
        let Some(slot) = slot_at(entry, slots) else {
            STATS[2].fetch_add(1, Ordering::Relaxed);
            continue;
        };
        let (key, len) = piece(r, p);
        let Some(bytes) = ring.get(slot..slot + len.min(SLOT)) else {
            continue;
        };
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let as_hashed = torn::hash(bytes) == torn::folded(result);
        if !as_hashed {
            STATS[3].fetch_add(1, Ordering::Relaxed);
        }
        out.push((key, slot, as_hashed));
    }
    out
}

/// For the plans being landed (their compares have run), in order: the
/// pieces their compares put in the ring are hashed and checked against
/// what the compares said, and the card time of those compares counted.
/// `None` without the ring or without such plans.
pub(super) fn look(plans: &[(&CardPlan, u64)], differs: &[u32]) -> Option<Look> {
    if plans.iter().all(|p| p.0.ring.is_none()) {
        return None;
    }
    let looking = std::time::Instant::now();
    let (data, slots, back, frequency) = {
        let work = CARD_WORK.lock().unwrap_or_else(|e| e.into_inner());
        let ring = work.as_ref()?.ring.as_ref()?;
        (ring.buffer.1, ring.slots, ring.stamps.2, ring.stamps.3)
    };
    // SAFETY: the ring's mapping, the index first. These plans' dwords are
    // given to no other plan before they are done with (as `differs`), and
    // no compare takes a slot past the tail, which moves when they are.
    let mapping = unsafe { std::slice::from_raw_parts(data as *const u8, bytes(slots) as usize) };
    // SAFETY: as above; the index is the mapping's first bytes, aligned.
    let index = unsafe { std::slice::from_raw_parts(data as *const u32, INDEX / 4) };
    let mut tail = TAIL.load(Ordering::Relaxed);
    let mut found = Vec::new();
    let mut own = Vec::with_capacity(plans.len());
    for &(plan, _) in plans {
        let Some(stamp) = plan.ring else {
            own.push(Placed::default());
            continue;
        };
        // SAFETY: the mapping holds `STAMPED` pairs; the compare has run.
        let [from, to] = unsafe { (back as *const [u64; 2]).add(stamp as usize).read_volatile() };
        if to > from && frequency > 0 {
            STATS[6].fetch_add((u128::from(to - from) * 1_000_000_000 / u128::from(frequency)) as u64, Ordering::Relaxed);
            STATS[7].fetch_add(1, Ordering::Relaxed);
        }
        let ranges: Vec<&CardRange> = plan.ranges.iter().filter(|r| r.compared && !r.picked).collect();
        let spans: Vec<(u32, u32)> = ranges.iter().map(|r| (r.at, r.span.pieces)).collect();
        let sorted = sort_out(&flagged(&spans, differs, index), mapping, slots, &mut tail, |r, p| {
            let (r, piece) = (ranges[r], ranges[r].span.first + p);
            let (from, to) = r.shadow.card_hashed(piece);
            ((std::sync::Arc::as_ptr(&r.shadow) as usize, piece), to.saturating_sub(from))
        });
        own.push(Placed::new(sorted.iter().filter(|s| s.2).map(|s| (s.0, s.1))));
        found.extend(sorted.into_iter().map(|s| (s.0, s.1)));
    }
    STATS[11].fetch_add(looking.elapsed().as_micros() as u64, Ordering::Relaxed);
    Some(Look { placed: Placed::new(found), own, data, tail })
}

impl Look {
    /// Where in the ring (from `data` on) the part `[lo, hi)` of a piece
    /// lies, for plan `plan` (its place in the plans given), the piece
    /// hashed over `hashed` (`Shadow::card_hashed`): `None` where that
    /// plan's compare did not put the piece there as it hashed it, or the
    /// slot does not hold the part.
    pub(super) fn part(&self, plan: usize, piece: (usize, u32), lo: usize, hi: usize, hashed: (usize, usize)) -> Option<u64> {
        let (from, to) = hashed;
        if lo < from || hi > to || hi - from > SLOT {
            return None;
        }
        Some(self.own.get(plan)?.part_at(piece, lo, from)? as u64)
    }

    /// What the fetch brought (`landed`, from `fetched` on) is compared with
    /// what the ring has of the same pieces.
    ///
    /// # Safety
    /// What was fetched is at `fetched`, as `landed` says.
    pub(super) unsafe fn against(&self, landed: &[torn::Landed], fetched: usize) {
        for (shadow, lo, hi, at) in landed {
            let piece = shadow.card_piece_of(*lo);
            let (from, _) = shadow.card_hashed(piece);
            if *lo < from || hi - from > SLOT {
                continue;
            }
            let Some(slot) = self.placed.part_at((std::sync::Arc::as_ptr(shadow) as usize, piece), *lo, from) else {
                continue;
            };
            // SAFETY: inside the ring's mapping (checked above), and the
            // caller's.
            let (ring, came) = unsafe {
                (
                    std::slice::from_raw_parts((self.data + slot) as *const u8, hi - lo),
                    std::slice::from_raw_parts((fetched + *at as usize) as *const u8, hi - lo),
                )
            };
            STATS[4].fetch_add(1, Ordering::Relaxed);
            if ring != came {
                STATS[5].fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// The landing is done with the ring: later compares may use its slots.
    pub(super) fn done(self) {
        TAIL.store(self.tail, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compare_source_has_the_slot_layout() {
        assert_eq!(SLOT % 256, 0, "slots start on whole lines");
        assert!(SLOT >= (1 << 16) + 32);
        let at = format!("{INDEX}u + (s & (slots - 1u)) * {SLOT}u");
        assert!(CARD_COMPARE.contains(&at), "the shader's slot address is {at}");
        assert!(INDEX as u64 + u64::from(MOST) * SLOT as u64 <= u64::from(u32::MAX));
    }

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

    /// Of three flagged pieces, one did not fit (the ring overflowed) and
    /// one's slot is not what the compare hashed: only the third may land
    /// from the ring, and the tail moves past all three.
    #[test]
    fn what_did_not_fit_or_is_not_as_hashed_lands_through_the_fetch() {
        let slots = 4;
        let len = 4096;
        let mut ring = vec![0u8; bytes(slots) as usize];
        for s in 0..slots as usize {
            ring[INDEX + s * SLOT..][..len].fill(s as u8 + 1);
        }
        // A result: flagged, and the piece's hash as the compare made it.
        let result = |ring: &[u8], s: usize| (torn::hash(&ring[INDEX + s * SLOT..][..len]) << 1) | 1;
        let (fits, flipped) = (result(&ring, 1), result(&ring, 2));
        ring[INDEX + 2 * SLOT + 100] ^= 0xFF;
        let found = [(0, 0, FULL | 4, result(&ring, 0)), (0, 1, 6, flipped), (1, 0, 5, fits)];
        let mut tail = 4;
        let out = sort_out(&found, &ring, slots, &mut tail, |r, p| ((0xC0 + r, p), len));
        assert_eq!(out, [((0xC0, 1), INDEX + 2 * SLOT, false), ((0xC1, 0), INDEX + SLOT, true)]);
        assert_eq!(tail, 7);
        // All did not fit: nothing for the ring, the tail still moves.
        let mut tail = 0;
        let full = [(0, 0, FULL, 1), (0, 1, FULL | 1, 1)];
        assert!(sort_out(&full, &ring, slots, &mut tail, |r, p| ((r, p), len)).is_empty());
        assert_eq!(tail, 2);
    }

    /// A part lands from the ring only from a slot its own plan's compare
    /// filled as it hashed it, and only where the slot holds all of it.
    #[test]
    fn a_part_lands_from_its_own_plans_slot_and_only_inside_it() {
        let look = Look {
            placed: Placed::default(),
            own: vec![Placed::new([((1, 2), INDEX + SLOT)]), Placed::default()],
            data: 0,
            tail: 0,
        };
        let (from, to) = (131_056, 131_056 + 65_552);
        assert_eq!(look.part(0, (1, 2), from + 16, from + 400, (from, to)), Some((INDEX + SLOT + 16) as u64));
        assert_eq!(look.part(0, (1, 2), from, to, (from, to)), Some((INDEX + SLOT) as u64));
        // Another plan's, another piece, no such plan.
        assert_eq!(look.part(1, (1, 2), from + 16, from + 400, (from, to)), None);
        assert_eq!(look.part(0, (1, 3), from + 16, from + 400, (from, to)), None);
        assert_eq!(look.part(2, (1, 2), from + 16, from + 400, (from, to)), None);
        // Outside what the slot holds.
        assert_eq!(look.part(0, (1, 2), from - 16, from + 400, (from, to)), None);
        assert_eq!(look.part(0, (1, 2), from + 16, to + 16, (from, to)), None);
        assert_eq!(look.part(0, (1, 2), from, from + SLOT + 16, (from, from + SLOT + 16)), None);
    }
}
