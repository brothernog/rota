//! The settings the README's numbers were measured with (an 8 GB NVIDIA
//! card), taken as the defaults of every command: a variable the user set
//! keeps its value, and `VICTORY_TUNED=0` applies none of them. Applied to
//! `shaders`, `warm` and `run` alike, so the shader cache and the warm-up
//! match the run they are for.

/// Each variable and the value it is given when unset.
pub(crate) const TUNED: &[(&str, &str)] = &[
    // Every ray misses: stable shadows without the ray-tracing structures.
    ("VICTORY_RT_MISS", "1"),
    // Copy-back on a thread of its own, beside the submitting thread.
    ("VICTORY_RELAY", "2"),
    // `VICTORY_THIN=1` more than halves the frame rate on NVIDIA cards.
    ("VICTORY_THIN", "0"),
    // Records the pipelines runs use, for `warm --used`.
    ("VICTORY_PIPELINE_BOOK", "1"),
    ("VICTORY_MEMORY_CARD_MB", "1024"),
    // The title's own upscaler on the Series S profile.
    ("VICTORY_UPSCALER", "6"),
    ("VICTORY_MEMORY_MAP", "0"),
    // Steps done on the card instead of waiting for flights: about +40%,
    // with softer shadows (`VICTORY_CARD_STEPS=0` for the exact ones).
    ("VICTORY_CARD_STEPS", "1"),
    // Small title buffers get a copy on the card too: about +33% more.
    ("VICTORY_CARD_ALL", "1"),
    ("VICTORY_DYNAMIC_SLOTS", "499999"),
    ("VICTORY_PIPELINE_WORKERS", "6"),
];

/// The tuned settings that `is_set` says the user left unset, or none with
/// `VICTORY_TUNED=0`.
fn missing(is_set: impl Fn(&str) -> bool, off: bool) -> Vec<(&'static str, &'static str)> {
    if off {
        return Vec::new();
    }
    TUNED.iter().copied().filter(|(k, _)| !is_set(k)).collect()
}

/// Sets the tuned settings the user left unset. Called first thing in
/// `main`, before any thread starts.
pub(crate) fn apply() {
    let off = std::env::var_os("VICTORY_TUNED").is_some_and(|v| v == "0");
    let set = missing(|k| std::env::var_os(k).is_some(), off);
    for (k, v) in &set {
        // SAFETY: at the start of `main`, before any other thread exists.
        unsafe { std::env::set_var(k, v) };
    }
    if !set.is_empty() {
        let names: Vec<String> = set.iter().map(|(k, v)| format!("{k}={v}")).collect();
        eprintln!("tuned defaults (VICTORY_TUNED=0: none): {}", names.join(" "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_variable_keeps_its_value() {
        let got = missing(|k| k == "VICTORY_CARD_STEPS", false);
        assert!(!got.iter().any(|(k, _)| *k == "VICTORY_CARD_STEPS"));
        assert_eq!(got.len(), TUNED.len() - 1);
    }

    #[test]
    fn tuned_zero_applies_none() {
        assert!(missing(|_| false, true).is_empty());
    }

    #[test]
    fn each_variable_once() {
        let mut names: Vec<&str> = TUNED.iter().map(|(k, _)| *k).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TUNED.len());
        assert!(names.iter().all(|k| k.starts_with("VICTORY_")));
    }
}
