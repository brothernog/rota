//! Diagnostics: switches that change while the title runs, so that one run
//! measures both sides in the same scene. `VICTORY_AB=<seconds>,<switch>[,<switch>..]`:
//! every so many seconds, at a present, the next phase begins; switch number
//! n is on in the phases whose number has bit n, so that all combinations
//! come one after another, again and again. Each phase's frame times are
//! logged at its end (the frames of its first second left out). A switch
//! named here does not go by its own variable any more.
//!
//! `VICTORY_MENU=1` (without `VICTORY_AB`): the switches of [`MENU`] change
//! when the overlay's menu is clicked, at the next present, and a phase is
//! the time between two changes. A switch goes by its own variable until it
//! is first clicked, so a run with the menu starts as one without it.

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// The switches the menu changes: name, whether the code has it on when its
/// variable is unset, and what it does. Each is read where it is used, at
/// every use, and has nothing made only at start that its other side needs.
pub(crate) const MENU: &[(&str, bool, &str)] = &[
    ("VICTORY_CARD_REHASH_RUNS", false, "rehash card pieces in runs"),
    ("VICTORY_TEXTURE_NO_WAIT", false, "texture refresh skips the remap lock"),
    ("VICTORY_ASYNC_SUBMIT", false, "prepare submissions on a thread of ours"),
    ("VICTORY_PLACED_ORDER", false, "placed copies in submission order"),
    ("VICTORY_JOIN_PARTS", false, "join submission parts (8, off: 1)"),
    ("VICTORY_KEEP_SHOWN", true, "slot keeps old texture (on: 2, off: 0)"),
    ("VICTORY_LIST_VIEWS", true, "one view per list table"),
    ("VICTORY_READY_ONCE", true, "unchanged tables not gone through again"),
    ("VICTORY_LAND_ECHO", true, "landed bytes not sent up again"),
    ("VICTORY_UNDER_WIDE", true, "compare written pages on several threads"),
    ("VICTORY_QUICK_FIND", true, "buffer lookups cached"),
    ("VICTORY_SAME_RECORDS", true, "skip records already written"),
    ("VICTORY_TAKE_PARTS", true, "take only written parts of a page"),
    ("VICTORY_OPEN_ONE_VIEW", true, "open one view of a guarded page"),
    ("VICTORY_GUARD_REST", true, "guards rest before blocking"),
];

struct Asked {
    seconds: f64,
    switches: Vec<String>,
    /// The overlay's menu changes the switches, not the clock.
    menu: bool,
}

fn parse(text: &str) -> Option<Asked> {
    let mut parts = text.split(',').map(str::trim);
    let seconds = parts.next()?.parse::<f64>().ok().filter(|s| *s > 1.0)?;
    let switches: Vec<String> = parts.filter(|s| !s.is_empty()).map(str::to_string).collect();
    (!switches.is_empty() && switches.len() <= 8).then_some(Asked { seconds, switches, menu: false })
}

fn asked() -> Option<&'static Asked> {
    static ASKED: OnceLock<Option<Asked>> = OnceLock::new();
    ASKED
        .get_or_init(|| match std::env::var("VICTORY_AB").ok().and_then(|t| parse(&t)) {
            Some(a) => {
                TAKEN.store(u32::MAX, Relaxed);
                Some(a)
            }
            None => crate::switch::on("VICTORY_MENU").then(|| Asked {
                seconds: f64::INFINITY,
                switches: MENU.iter().map(|(n, ..)| (*n).to_string()).collect(),
                menu: true,
            }),
        })
        .as_ref()
}

/// The switches that are on now, a bit each.
static BITS: AtomicU32 = AtomicU32::new(0);
/// The switches that go by [`BITS`] and not by their own variable (all of
/// them with `VICTORY_AB`; with the menu, those clicked once).
static TAKEN: AtomicU32 = AtomicU32::new(0);
/// Menu clicks not applied yet: the switches to turn over at the next present.
static PENDING: AtomicU32 = AtomicU32::new(0);

/// Whether `switch` is on now, if it is one of those that change.
pub(crate) fn pick(switch: &str) -> Option<bool> {
    bit(switch).and_then(taken)
}

/// The number of `switch` among those that change.
pub(crate) fn bit(switch: &str) -> Option<u32> {
    Some(asked()?.switches.iter().position(|s| s == switch)? as u32)
}

/// Whether the switch of that number is on now, if it goes by [`BITS`].
pub(crate) fn taken(bit: u32) -> Option<bool> {
    (TAKEN.load(std::sync::atomic::Ordering::Acquire) >> bit & 1 != 0).then(|| BITS.load(Relaxed) >> bit & 1 != 0)
}

/// Whether the menu is on (`VICTORY_MENU=1` and no `VICTORY_AB`).
pub(crate) fn menu() -> bool {
    asked().is_some_and(|a| a.menu)
}

/// What a menu switch is now: on or off, and whether it still goes by its
/// own variable (then guessed from it: `0` off, any other value on, unset as
/// [`MENU`] says).
pub(crate) fn menu_state(bit: u32) -> (bool, bool) {
    match taken(bit) {
        Some(on) => (on, false),
        None => {
            let (name, unset_on, _) = MENU[bit as usize];
            (std::env::var_os(name).map_or(unset_on, |v| v != "0"), true)
        }
    }
}

/// What every menu switch is now, a bit each in [`MENU`] order (the overlay
/// CSV's `menu_bits`).
pub(crate) fn menu_bits() -> u32 {
    (0..MENU.len() as u32).filter(|&n| menu_state(n).0).fold(0, |b, n| b | 1 << n)
}

/// A menu click on switch `bit`: it turns over at the next present (a
/// second click before then takes the first back).
pub(crate) fn request(bit: u32) {
    if menu() && (bit as usize) < MENU.len() {
        PENDING.fetch_xor(1 << bit, Relaxed);
    }
}

/// The menu clicks not applied yet.
pub(crate) fn pending() -> u32 {
    PENDING.load(Relaxed)
}

/// Frames and median frame time (ms) of the phase under way, and of the one
/// before it, for the menu.
pub(crate) fn phases() -> [(usize, f32); 2] {
    let p = PHASE.lock().unwrap_or_else(|e| e.into_inner());
    let now = p.as_ref().map_or((0, 0.0), |p| {
        let mut f = p.frames.clone();
        (f.len(), summary(&mut f).1)
    });
    [now, *LAST.lock().unwrap_or_else(|e| e.into_inner())]
}

/// The phase that ended last: frames and median frame time (ms).
static LAST: Mutex<(usize, f32)> = Mutex::new((0, 0.0));
static PHASE: Mutex<Option<Phase>> = Mutex::new(None);

/// What the run has seen of the phase under way.
struct Phase {
    started: std::time::Instant,
    number: u64,
    began: std::time::Instant,
    last: std::time::Instant,
    drawn: u64,
    frames: Vec<f32>,
    draws: u64,
}

/// The frames' mean, median and nine tenths.
fn summary(frames: &mut [f32]) -> (f32, f32, f32) {
    if frames.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = frames.iter().sum::<f32>() / frames.len() as f32;
    frames.sort_by(f32::total_cmp);
    (mean, frames[frames.len() / 2], frames[frames.len() * 9 / 10])
}

/// The title's present ends on this thread. `change`: called when another
/// phase begins, before any switch changes, with what `switch` is and what it
/// will be.
pub(crate) fn presented(change: impl Fn(&str, bool, bool)) {
    let Some(asked) = asked() else {
        return;
    };
    let now = std::time::Instant::now();
    let drawn = crate::draw::drawn();
    let mut phase = PHASE.lock().unwrap_or_else(|e| e.into_inner());
    let p = phase.get_or_insert_with(|| Phase {
        started: now,
        number: 0,
        began: now,
        last: now,
        drawn,
        frames: Vec::new(),
        draws: 0,
    });
    if (now - p.began).as_secs_f64() > 1.0 {
        p.frames.push((now - p.last).as_secs_f32() * 1000.0);
        p.draws += drawn.saturating_sub(p.drawn);
    }
    (p.last, p.drawn) = (now, drawn);
    let (number, was, will, take) = if asked.menu {
        let flip = PENDING.swap(0, Relaxed);
        if flip == 0 {
            return;
        }
        let was = menu_bits();
        let t = crate::overlay::run_start().map_or(0.0, |s| (now - s).as_secs_f64());
        for n in (0..MENU.len()).filter(|n| flip >> n & 1 != 0) {
            tracing::info!(t_s = format!("{t:.1}"), "menu: {} {}", MENU[n].0, if was >> n & 1 != 0 { "off" } else { "on" });
        }
        (p.number + 1, was, was ^ flip, flip)
    } else {
        let number = ((now - p.started).as_secs_f64() / asked.seconds) as u64;
        if number == p.number {
            return;
        }
        let bits = |number: u64| (number % (1 << asked.switches.len())) as u32;
        (number, bits(p.number), bits(number), 0)
    };
    let (mean, median, most) = summary(&mut p.frames);
    *LAST.lock().unwrap_or_else(|e| e.into_inner()) = (p.frames.len(), median);
    let on: Vec<&str> = asked.switches.iter().enumerate().filter(|(n, _)| was >> n & 1 != 0).map(|(_, s)| s.as_str()).collect();
    tracing::info!(
        "A/B phase {} bits {was} on [{}]: {} frames, mean {mean:.1} ms, median {median:.1}, p90 {most:.1}, {:.0} draws a frame",
        p.number,
        on.join(" "),
        p.frames.len(),
        p.draws as f64 / p.frames.len().max(1) as f64
    );
    for (n, switch) in asked.switches.iter().enumerate() {
        change(switch, was >> n & 1 != 0, will >> n & 1 != 0);
    }
    BITS.store(will, Relaxed);
    // After the bits: a switch first clicked goes from its variable straight to its new side.
    TAKEN.fetch_or(take, std::sync::atomic::Ordering::Release);
    (p.number, p.began, p.draws) = (number, now, 0);
    p.frames.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_and_seconds_are_read_and_frames_summed_up() {
        let a = parse("10, VICTORY_A,VICTORY_B").unwrap();
        assert_eq!((a.seconds, a.switches), (10.0, vec!["VICTORY_A".to_string(), "VICTORY_B".to_string()]));
        assert!(parse("10").is_none());
        assert!(parse("x,VICTORY_A").is_none());
        assert!(parse("0,VICTORY_A").is_none());
        assert_eq!(summary(&mut [30.0, 10.0, 20.0]), (20.0, 20.0, 30.0));
        assert_eq!(summary(&mut []), (0.0, 0.0, 0.0));
        assert_eq!(pick("VICTORY_NONE_SUCH"), None);
    }

    #[test]
    fn menu_fits_the_bits_and_names_each_switch_once() {
        assert!(MENU.len() <= 32);
        for (i, (name, ..)) in MENU.iter().enumerate() {
            assert!(name.starts_with("VICTORY_"), "{name}");
            assert!(MENU[..i].iter().all(|(n, ..)| n != name), "{name} twice");
        }
    }
}
