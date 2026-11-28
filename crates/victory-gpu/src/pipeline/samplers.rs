//! Static samplers made in one order in every process, so that pipelines
//! with them can be kept in the pipeline library (`VICTORY_SAMPLER_ORDER`,
//! on unless `=0`).
//!
//! The card's driver keeps static samplers in a table of its own, puts a
//! sampler there when the first root signature with it is made (alike ones
//! share a place), and compiles the places into the pipelines. A pipeline
//! loaded from the library in another process keeps the places of the
//! process that stored it (`library::with_static_samplers`). So the static
//! samplers seen before are listed beside the library, and each process
//! makes a root signature for each of them, in the list's order, as soon as
//! its device is made and before anything else, and keeps them: the listed
//! ones then have the same places in every process. A pipeline is loaded
//! from or stored in the library only if all its static samplers were made
//! so ([`listed`]); one with a sampler new to this process is made and not
//! kept, and its new samplers join the list for the next one. What comes
//! after the listed ones (new samplers, the layer's own root signatures,
//! other libraries') cannot move them, and a list that was not saved (a
//! crash) only leaves its new pipelines out.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

/// The list, beside the library: each sampler as 13 little-endian words.
const FILE: &str = "static-samplers.bin";
/// The list grows no further: the card's table holds 2,032 distinct static
/// samplers (the next removes the device, measured upstream), and a process
/// makes all the listed ones whether it uses them or not.
/// shortcut: 496 places left for samplers a run meets outside the list (and
/// the layer's own); count them before raising it.
const CAP: usize = 1536;

/// A static sampler's description as words, all of it (register, space and
/// visibility too: alike for the driver or not, the order is kept).
type Key = [u32; 13];

static DIR: OnceLock<PathBuf> = OnceLock::new();
static LIST: Mutex<Option<List>> = Mutex::new(None);
/// Host root signatures for title pipelines, and those with static samplers.
static ROOTS: AtomicU64 = AtomicU64::new(0);
static WITH: AtomicU64 = AtomicU64::new(0);

/// Keeps the list in `dir` (the library's folder) from now on.
pub(crate) fn use_dir(dir: PathBuf) {
    let _ = DIR.set(dir);
}

/// `VICTORY_SAMPLER_ORDER=0`: no list; pipelines with static samplers are
/// never loaded nor stored.
fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SAMPLER_ORDER").is_none_or(|v| v != "0"))
}

struct List {
    /// Made at the start of this process, in the list's order.
    start: HashSet<Key>,
    /// The list: those, then the new ones of this process.
    all: Vec<Key>,
    known: HashSet<Key>,
    /// How many of `all` the file holds.
    written: usize,
    /// The root signatures that hold the listed samplers' places.
    _kept: Vec<ID3D12RootSignature>,
}

fn key(s: &D3D12_STATIC_SAMPLER_DESC) -> Key {
    [
        s.Filter.0 as u32,
        s.AddressU.0 as u32,
        s.AddressV.0 as u32,
        s.AddressW.0 as u32,
        s.MipLODBias.to_bits(),
        s.MaxAnisotropy,
        s.ComparisonFunc.0 as u32,
        s.BorderColor.0 as u32,
        s.MinLOD.to_bits(),
        s.MaxLOD.to_bits(),
        s.ShaderRegister,
        s.RegisterSpace,
        s.ShaderVisibility.0 as u32,
    ]
}

fn desc(k: &Key) -> D3D12_STATIC_SAMPLER_DESC {
    D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER(k[0] as i32),
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(k[1] as i32),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(k[2] as i32),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(k[3] as i32),
        MipLODBias: f32::from_bits(k[4]),
        MaxAnisotropy: k[5],
        ComparisonFunc: D3D12_COMPARISON_FUNC(k[6] as i32),
        BorderColor: D3D12_STATIC_BORDER_COLOR(k[7] as i32),
        MinLOD: f32::from_bits(k[8]),
        MaxLOD: f32::from_bits(k[9]),
        ShaderRegister: k[10],
        RegisterSpace: k[11],
        ShaderVisibility: D3D12_SHADER_VISIBILITY(k[12] as i32),
    }
}

/// The list in `bytes`: a torn end is not read, one listed twice counts once,
/// and no more than [`CAP`].
fn parse(bytes: &[u8]) -> Vec<Key> {
    let mut seen = HashSet::new();
    bytes
        .chunks_exact(size_of::<Key>())
        .map(|c| std::array::from_fn(|i| u32::from_le_bytes([c[4 * i], c[4 * i + 1], c[4 * i + 2], c[4 * i + 3]])))
        .filter(|k| seen.insert(*k))
        .take(CAP)
        .collect()
}

fn bytes(keys: &[Key]) -> Vec<u8> {
    keys.iter().flatten().flat_map(|w| w.to_le_bytes()).collect()
}

/// A root signature with the one sampler `k`.
fn root(device: &ID3D12Device, k: &Key) -> windows::core::Result<ID3D12RootSignature> {
    let sampler = desc(k);
    let d = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 { NumStaticSamplers: 1, pStaticSamplers: &sampler, ..Default::default() },
        },
    };
    // SAFETY: the description points at a live sampler; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        D3D12SerializeVersionedRootSignature(&d, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let code = std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, code)
    }
}

/// Makes the listed static samplers, in the list's order: right after the
/// device is made, before any other root signature.
pub(crate) fn prepare(device: &ID3D12Device) {
    let Some(dir) = DIR.get().filter(|_| on()) else { return };
    let all = parse(&std::fs::read(dir.join(FILE)).unwrap_or_default());
    let began = std::time::Instant::now();
    let mut kept = Vec::with_capacity(all.len());
    let mut start = HashSet::new();
    for k in &all {
        match root(device, k) {
            Ok(r) => {
                kept.push(r);
                start.insert(*k);
            }
            Err(e) => {
                // Those after it would not have their places: not listed here.
                tracing::warn!(error = %e, made = start.len(), "a listed static sampler was refused");
                break;
            }
        }
    }
    tracing::info!(
        listed = all.len(),
        made = start.len(),
        ms = began.elapsed().as_millis() as u64,
        "static samplers made first, in the list's order"
    );
    let known = all.iter().copied().collect();
    let written = all.len();
    *lock() = Some(List { start, all, known, written, _kept: kept });
}

fn lock() -> std::sync::MutexGuard<'static, Option<List>> {
    LIST.lock().unwrap_or_else(|e| e.into_inner())
}

/// The static samplers of a title pipeline's root signature, about to be
/// made: whether a pipeline with them may be loaded from and stored in the
/// library (none, or all made first in this process). New ones join the list.
pub(crate) fn listed(samplers: &[D3D12_STATIC_SAMPLER_DESC]) -> bool {
    ROOTS.fetch_add(1, Ordering::Relaxed);
    if samplers.is_empty() {
        return true;
    }
    WITH.fetch_add(1, Ordering::Relaxed);
    lock().as_mut().is_some_and(|list| admit(list, samplers.iter().map(key)))
}

/// [`listed`] on `list`.
fn admit(list: &mut List, keys: impl Iterator<Item = Key>) -> bool {
    let mut all_made = true;
    for k in keys {
        if list.start.contains(&k) {
            continue;
        }
        all_made = false;
        if list.known.len() < CAP && list.known.insert(k) {
            list.all.push(k);
        }
    }
    all_made
}

/// Writes the list if it grew (with the library's saves), and says how
/// things stand.
pub(crate) fn save() {
    let Some(dir) = DIR.get() else { return };
    let mut guard = lock();
    let Some(list) = guard.as_mut() else { return };
    if list.all.len() == list.written {
        return;
    }
    match write(dir, &bytes(&list.all)) {
        Ok(()) => list.written = list.all.len(),
        Err(e) => tracing::warn!(error = %e, "static sampler list not saved"),
    }
    tracing::info!(
        made_first = list.start.len(),
        listed = list.all.len(),
        new = list.all.len() - list.start.len(),
        roots = ROOTS.load(Ordering::Relaxed),
        with_static_samplers = WITH.load(Ordering::Relaxed),
        out_of_library = super::library::UNSTORED.load(Ordering::Relaxed),
        "static samplers (the layer's own 4 kinds come after the list and are not counted)"
    );
}

fn write(dir: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(FILE).with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, dir.join(FILE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sampler(bias: u32) -> Key {
        let mut k = key(&D3D12_STATIC_SAMPLER_DESC::default());
        k[4] = bias;
        k
    }

    fn list(start: &[Key]) -> List {
        List {
            start: start.iter().copied().collect(),
            all: start.to_vec(),
            known: start.iter().copied().collect(),
            written: start.len(),
            _kept: Vec::new(),
        }
    }

    /// Only a pipeline whose samplers were all made first may use the
    /// library; a new sampler joins the list, and is not made first by that.
    #[test]
    fn only_samplers_made_first_open_the_library() {
        let (a, b, c) = (sampler(1), sampler(2), sampler(3));
        let mut l = list(&[a, b]);
        assert!(admit(&mut l, [a].into_iter()));
        assert!(admit(&mut l, [b, a].into_iter()));
        assert!(!admit(&mut l, [a, c].into_iter()), "a new sampler let the pipeline in");
        assert_eq!(l.all, [a, b, c]);
        assert!(!admit(&mut l, [c].into_iter()), "listed in this process is not made first");
        assert_eq!(l.all, [a, b, c], "listed twice");
    }

    /// Past the cap the list keeps its length; those pipelines stay out.
    #[test]
    fn the_list_stops_at_the_cap() {
        let full: Vec<Key> = (0..CAP as u32).map(sampler).collect();
        let mut l = list(&full);
        assert!(!admit(&mut l, [sampler(CAP as u32)].into_iter()));
        assert_eq!(l.all.len(), CAP);
    }

    /// The file keeps the order; a torn end and a repeat are dropped.
    #[test]
    fn the_list_comes_back_in_its_order() {
        let keys = [sampler(5), sampler(3), sampler(5), sampler(9)];
        let mut b = bytes(&keys);
        b.extend([1, 2, 3]);
        assert_eq!(parse(&b), [sampler(5), sampler(3), sampler(9)]);
        let k = key(&D3D12_STATIC_SAMPLER_DESC { MaxLOD: 12.5, ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL, ..Default::default() });
        assert_eq!(key(&desc(&k)), k);
    }
}
