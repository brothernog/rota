//! Implicit-LOD samples outside pixel shaders (`VICTORY_COMPUTE_LOD=1`; it
//! joins the shader cache key).
//!
//! The console takes the derivatives of such a sample from the quad's
//! neighbouring lanes (lane ^ 1 across x, lane ^ 2 across y). The host has no
//! screen derivatives there, so by default the sample reads level 0. With the
//! switch on, each coordinate's value in those neighbours comes from DXIL's
//! QuadOp, the coarse derivatives from it, and the sample is an explicit
//! gradient one (a bias scales the gradients by 2^bias).
//!
//! Quads are the console's only when the host waves hold whole console
//! quads: the program takes the console lane order (`console_lanes`, which
//! such a sample sets while the switch is on), so a host wave is a console
//! wave or one 32-lane half of it, and lanes 4k..4k+3 stay together.

use super::{Attr, Bin, Cast, Lower, V, Why, flags, pred};

thread_local! {
    #[cfg(test)]
    static FORCED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether `VICTORY_COMPUTE_LOD=1` is set.
pub(super) fn enabled() -> bool {
    #[cfg(test)]
    if FORCED.get() {
        return true;
    }
    std::env::var_os("VICTORY_COMPUTE_LOD").is_some_and(|v| v == "1")
}

/// Tests: runs `f` with the switch on for this thread (no process-wide env change).
#[cfg(test)]
pub(crate) fn force<R>(f: impl FnOnce() -> R) -> R {
    let was = FORCED.replace(true);
    let r = f();
    FORCED.set(was);
    r
}

/// What a quad-derived sample is lowered with.
pub(super) enum Footprint {
    /// `sampleGrad`'s six gradients (x then y).
    Grad([V; 6]),
    /// A cube's level, taken from its face coordinates as for explicit
    /// gradients (`sampleLevel`).
    Level(V),
}

impl Lower<'_> {
    /// `xs` in this lane's quad neighbours across x and y, as the changes
    /// from this lane to them in the direction of growing lane bit 0 / bit 1:
    /// the console's coarse derivatives.
    fn quad_deltas(&mut self, xs: &[V]) -> [Vec<V>; 2] {
        self.s.flags |= flags::WAVE_OPS;
        let (c, i32t) = (self.cur, self.s.t.i32);
        let lane = self.lane_index();
        let mut sign = Vec::new();
        for bit in 0..2 {
            let k = self.s.m.i32(1 << bit);
            let m = self.s.m.bin(c, Bin::And, lane, k);
            let zero = self.s.m.i32(0);
            sign.push(self.s.m.cmp(c, pred::NE, m, zero));
        }
        let mut out = [Vec::new(), Vec::new()];
        for &x in xs {
            let bits = self.as_int(x);
            for (axis, d) in out.iter_mut().enumerate() {
                // QuadReadAcrossX, Y.
                let kind = self.s.m.i8(axis as u8);
                let read = self
                    .s
                    .op(c, "quadOp.i32", Attr::NoUnwind, i32t, 123, &[bits, kind]);
                let near = self.as_float(read);
                let up = self.s.m.bin(c, Bin::Sub, near, x);
                let down = self.s.m.bin(c, Bin::Sub, x, near);
                d.push(self.s.m.select(c, sign[axis], down, up));
            }
        }
        out
    }

    /// The footprint of an implicit-LOD sample outside a pixel shader: the
    /// quad's changes of the coordinates in `cs[..spatial]` (a cube's: of
    /// the face coordinates `st`, before they became a direction) scaled by
    /// 2^`bias`. A texture array's slice is not differentiated.
    pub(super) fn quad_footprint(
        &mut self,
        (kind, spatial): (u32, usize),
        (cs, st): (&[V], [V; 2]),
        bias: Option<V>,
        (h, has_cl, clamp): (V, bool, V),
    ) -> Footprint {
        let c = self.cur;
        let xs: &[V] = if kind == 5 { &st } else { &cs[..spatial] };
        let mut d = self.quad_deltas(xs);
        if let Some(bias) = bias {
            let scale = self.unary(21, bias); // 2^bias
            for x in d.iter_mut().flatten() {
                *x = self.s.m.bin(c, Bin::Mul, *x, scale);
            }
        }
        if kind == 5 {
            let lod = self.cube_footprint_lod(h, has_cl, clamp, |_, axis, k| Ok(d[axis][k]));
            return Footprint::Level(lod.expect("quad deltas are values"));
        }
        let mut g = [self.s.m.undef(self.s.t.f32); 6];
        g[..spatial].copy_from_slice(&d[0][..spatial]);
        g[3..3 + spatial].copy_from_slice(&d[1][..spatial]);
        Footprint::Grad(g)
    }

    /// The level a cube's face-coordinate changes select (`get(axis, k)`:
    /// change `k` of `s`, `t` along x or y; the face is one unit wide): the
    /// longer of the two changes, in texels of level 0. `has_cl`: the
    /// instruction's least level, `clamp`.
    // shortcut: no anisotropic filtering for these samples (the longer axis
    // alone decides the level).
    pub(super) fn cube_footprint_lod(
        &mut self,
        h: V,
        has_cl: bool,
        clamp: V,
        mut get: impl FnMut(&mut Self, usize, usize) -> Result<V, Why>,
    ) -> Result<V, Why> {
        let c = self.cur;
        let mut len = [h; 2];
        for (axis, slot) in len.iter_mut().enumerate() {
            let mut sum = self.s.m.f32(0);
            for k in 0..2 {
                let x = get(self, axis, k)?;
                let sq = self.s.m.bin(c, Bin::Mul, x, x);
                sum = self.s.m.bin(c, Bin::Add, sum, sq);
            }
            *slot = self.unary(24, sum); // square root
        }
        let longer = self.binary(35, len[0], len[1]); // maximum
        let i32t = self.s.t.i32;
        let dims = self.s.m.named_struct("dx.types.Dimensions", vec![i32t; 4]);
        let z = self.s.m.i32(0);
        let d = self
            .s
            .op(c, "getDimensions", Attr::ReadOnly, dims, 72, &[h, z]);
        let width = self.s.m.extract(c, d, 0);
        let width = self.s.m.cast(c, Cast::UiToFp, width, self.s.t.f32);
        let texels = self.s.m.bin(c, Bin::Mul, longer, width);
        // DXIL's logarithm is the one to base 2 (it was taken for the
        // natural one: every level came out 1.44 times too high;
        // DexNom/VIctory 921d8d3b). A length of 0 gives minus infinity,
        // which the sampler clamps to level 0. SampleLevel takes no clamp:
        // the instruction's least level is applied here.
        let lod = self.unary(23, texels);
        Ok(if has_cl { self.binary(35, lod, clamp) } else { lod })
    }
}
