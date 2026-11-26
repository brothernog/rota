//! Decoding of the few instructions the console's processor (Zen 2) runs and an Intel PC may
//! not: AMD-only ones (CLZERO, RDPRU, MONITORX, MWAITX, SSE4a's EXTRQ and INSERTQ) and newer
//! Intel ones older parts lack (CLWB, RDPID). On such a PC they raise
//! STATUS_ILLEGAL_INSTRUCTION; the handler in `console_insn.rs` emulates what is decoded here.
//!
//! Written from the encodings in AMD's "AMD64 Architecture Programmer's Manual" (volumes 3
//! and 4) and Intel's "Software Developer's Manual" (volume 2).

/// An instruction the PC may lack, as found at a faulting address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleOnly {
    /// `clwb m8` (66 0F AE /6, memory operand): a cache write-back; nothing to emulate.
    Clwb,
    /// `clzero` (0F 01 FC): zeroes the 64-byte line holding rAX.
    Clzero,
    /// The usual zeroing loop around `clzero`: `clzero; add rax, 64; cmp rax, <end_reg>;
    /// jb <clzero>`, which zeroes `[rax, end)` line by line.
    ClzeroLoop { end_reg: u8 },
    /// `rdpru` (0F 01 FD): reads the counter ECX names (0 MPERF, 1 APERF) into EDX:EAX.
    Rdpru,
    /// `monitorx` (0F 01 FA) and `mwaitx` (0F 01 FB): a wait hint, skipped.
    Monitorx,
    Mwaitx,
    /// `rdpid r64` (F3 0F C7 /7, register operand): the processor id into the register, by its
    /// number in CONTEXT order (rax, rcx, rdx, rbx, rsp, rbp, rsi, rdi, r8..r15).
    Rdpid(u8),
    /// SSE4a `extrq` (66 0F 78 /0 ib ib, or 66 0F 79 /r): a bit field of `dst`'s low quadword,
    /// shifted down and zero-extended. With `src`, the length and index come from that
    /// register's bits 5:0 and 13:8 at run time. XMM numbers 0..15.
    Extrq { dst: u8, src: Option<u8>, len: u8, idx: u8 },
    /// SSE4a `insertq` (F2 0F 78 /r ib ib, or F2 0F 79 /r): the low bits of `src` placed into
    /// a bit field of `dst`'s low quadword. With `reg`, the length and index come from `src`'s
    /// bits 69:64 and 77:72 at run time.
    Insertq { dst: u8, src: u8, reg: bool, len: u8, idx: u8 },
    /// `ud2` (0F 0B): the title's own trap, left to it.
    Ud2,
}

/// The mask of an SSE4a field of `len` bits, from its 6-bit encoding (0 stands for 64).
fn field_mask(len: u8) -> u64 {
    match len & 63 {
        0 => u64::MAX,
        n => (1u64 << n) - 1,
    }
}

/// EXTRQ on a quadword: the `len`-bit field starting at bit `idx`, at bit 0. AMD leaves a
/// field reaching past bit 63 undefined; this gives what the shift gives.
pub fn extrq(v: u64, len: u8, idx: u8) -> u64 {
    (v >> (idx & 63)) & field_mask(len)
}

/// INSERTQ on a quadword: the low `len` bits of `src` written over bits `idx..` of `dst`.
pub fn insertq(dst: u64, src: u64, len: u8, idx: u8) -> u64 {
    let shift = idx & 63;
    let mask = field_mask(len) << shift;
    (dst & !mask) | ((src << shift) & mask)
}

/// The prefixes in front of an opcode, as the processor reads them.
#[derive(Default)]
struct Prefixes {
    /// The last of F2 / F3, which picks the instruction (0 if neither).
    rep: u8,
    operand_size: bool,
    /// REX, only when it is the last prefix (a REX followed by another prefix is ignored).
    rex: u8,
    len: usize,
}

fn prefixes(code: &[u8]) -> Prefixes {
    let mut p = Prefixes::default();
    while let Some(&b) = code.get(p.len) {
        match b {
            0xF2 | 0xF3 => p.rep = b,
            0x66 => p.operand_size = true,
            0x67 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0xF0 => {}
            0x40..=0x4F => {
                p.rex = b;
                p.len += 1;
                continue;
            }
            _ => break,
        }
        // A legacy prefix after a REX drops it: a REX counts only right before the opcode.
        p.rex = 0;
        p.len += 1;
    }
    p
}

/// The bytes a memory ModRM operand takes (ModRM, SIB, displacement), 64-bit addressing;
/// `None` for a register operand or bytes cut short.
fn memory_operand_len(code: &[u8]) -> Option<usize> {
    let modrm = *code.first()?;
    let (md, rm) = (modrm >> 6, modrm & 7);
    let mut len = 1;
    let mut base = rm;
    if md == 3 {
        return None;
    }
    if rm == 4 {
        base = *code.get(1)? & 7;
        len += 1;
    }
    len += match md {
        0 if rm == 5 || (rm == 4 && base == 5) => 4,
        0 => 0,
        1 => 1,
        _ => 4,
    };
    (code.len() >= len).then_some(len)
}

/// The instruction at the start of `code` if it is one of [`ConsoleOnly`], and its length.
pub fn decode(code: &[u8]) -> Option<(ConsoleOnly, usize)> {
    let p = prefixes(code);
    let op = code.get(p.len..)?;
    let (rex_r, rex_b) = ((p.rex >> 2) & 1, p.rex & 1);
    // A register ModRM: (reg, rm) with REX applied.
    let regs = |m: u8| (((m >> 3) & 7) | rex_r << 3, (m & 7) | rex_b << 3);
    let at = |n: usize| p.len + n;
    match *op {
        [0x0F, 0x0B, ..] => Some((ConsoleOnly::Ud2, at(2))),
        [0x0F, 0x01, 0xFC, ref rest @ ..] => {
            if p.len == 0
                && let [0x48, 0x83, 0xC0, 0x40, rex @ (0x48 | 0x4C), 0x39, m, 0x72, 0xF4, ..] = *rest
                && m >> 6 == 3
                && m & 7 == 0
            {
                // cmp rax, r64: rAX is the r/m operand, the end register the reg one.
                let end_reg = ((m >> 3) & 7) | ((rex >> 2) & 1) << 3;
                return Some((ConsoleOnly::ClzeroLoop { end_reg }, 12));
            }
            Some((ConsoleOnly::Clzero, at(3)))
        }
        [0x0F, 0x01, 0xFD, ..] => Some((ConsoleOnly::Rdpru, at(3))),
        [0x0F, 0x01, 0xFA, ..] => Some((ConsoleOnly::Monitorx, at(3))),
        [0x0F, 0x01, 0xFB, ..] => Some((ConsoleOnly::Mwaitx, at(3))),
        [0x0F, 0xAE, m, ..] if p.operand_size && p.rep == 0 && (m >> 3) & 7 == 6 => {
            memory_operand_len(&op[2..]).map(|n| (ConsoleOnly::Clwb, at(2 + n)))
        }
        [0x0F, 0xC7, m, ..] if p.rep == 0xF3 && m >> 6 == 3 && (m >> 3) & 7 == 7 => {
            Some((ConsoleOnly::Rdpid(regs(m).1), at(3)))
        }
        [0x0F, opcode @ (0x78 | 0x79), m, ..] if m >> 6 == 3 => {
            let (reg, rm) = regs(m);
            match (p.rep, p.operand_size, opcode) {
                (0xF2, _, 0x78) => {
                    let [len, idx] = [*op.get(3)?, *op.get(4)?];
                    Some((ConsoleOnly::Insertq { dst: reg, src: rm, reg: false, len: len & 63, idx: idx & 63 }, at(5)))
                }
                (0xF2, _, _) => Some((ConsoleOnly::Insertq { dst: reg, src: rm, reg: true, len: 0, idx: 0 }, at(3))),
                (0, true, 0x78) if reg & 7 == 0 => {
                    let [len, idx] = [*op.get(3)?, *op.get(4)?];
                    Some((ConsoleOnly::Extrq { dst: rm, src: None, len: len & 63, idx: idx & 63 }, at(5)))
                }
                (0, true, 0x79) => Some((ConsoleOnly::Extrq { dst: reg, src: Some(rm), len: 0, idx: 0 }, at(3))),
                _ => None,
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_helpers_follow_the_manual() {
        // A length of 0 means the whole quadword.
        assert_eq!(field_mask(0), u64::MAX);
        assert_eq!(field_mask(5), 0b1_1111);
        assert_eq!(extrq(0xFF00, 8, 8), 0xFF);
        assert_eq!(extrq(0xDEAD_0000_0000_0000, 16, 48), 0xDEAD);
        assert_eq!(extrq(42, 0, 0), 42);
        assert_eq!(insertq(0, 0b101, 3, 4), 0b101_0000);
        assert_eq!(insertq(u64::MAX, 0, 8, 0), !0xFF);
        assert_eq!(insertq(7, 0xABCD, 0, 0), 0xABCD);
        // Only the low `len` bits of the source go in.
        assert_eq!(insertq(0, 0xFF, 2, 62), 0xC000_0000_0000_0000);
        for (v, len, idx) in [(0xF0F0_1234u64, 9u8, 3u8), (u64::MAX, 1, 63), (0x8000_0001, 32, 0)] {
            assert_eq!(extrq(insertq(u64::MAX, v, len, idx), len, idx), v & field_mask(len));
        }
    }

    #[test]
    fn amd_only_instructions() {
        assert_eq!(decode(&[0x0F, 0x01, 0xFC, 0x90]), Some((ConsoleOnly::Clzero, 3)));
        assert_eq!(decode(&[0x0F, 0x01, 0xFD]), Some((ConsoleOnly::Rdpru, 3)));
        assert_eq!(decode(&[0x0F, 0x01, 0xFA]), Some((ConsoleOnly::Monitorx, 3)));
        assert_eq!(decode(&[0x0F, 0x01, 0xFB]), Some((ConsoleOnly::Mwaitx, 3)));
        assert_eq!(decode(&[0x0F, 0x0B]), Some((ConsoleOnly::Ud2, 2)));
        // 32-bit addressing: still CLZERO, one byte longer.
        assert_eq!(decode(&[0x67, 0x0F, 0x01, 0xFC]), Some((ConsoleOnly::Clzero, 4)));
    }

    #[test]
    fn zeroing_loops() {
        let lp = |rex: u8, m: u8| [0x0F, 0x01, 0xFC, 0x48, 0x83, 0xC0, 0x40, rex, 0x39, m, 0x72, 0xF4];
        // cmp rax, rbx / rsi / r9 / r15
        assert_eq!(decode(&lp(0x48, 0xD8)), Some((ConsoleOnly::ClzeroLoop { end_reg: 3 }, 12)));
        assert_eq!(decode(&lp(0x48, 0xF0)), Some((ConsoleOnly::ClzeroLoop { end_reg: 6 }, 12)));
        assert_eq!(decode(&lp(0x4C, 0xC8)), Some((ConsoleOnly::ClzeroLoop { end_reg: 9 }, 12)));
        assert_eq!(decode(&lp(0x4C, 0xF8)), Some((ConsoleOnly::ClzeroLoop { end_reg: 15 }, 12)));
        // Comparing something other than rax, a different branch, or a prefixed CLZERO: the
        // one instruction only.
        assert_eq!(decode(&lp(0x48, 0xD9)), Some((ConsoleOnly::Clzero, 3)));
        let mut other_jump = lp(0x48, 0xD8);
        other_jump[11] = 0xF0;
        assert_eq!(decode(&other_jump), Some((ConsoleOnly::Clzero, 3)));
        let mut prefixed = vec![0x67];
        prefixed.extend(lp(0x48, 0xD8));
        assert_eq!(decode(&prefixed), Some((ConsoleOnly::Clzero, 4)));
    }

    #[test]
    fn clwb_takes_any_memory_operand() {
        // [rdx], [rbx+0x10], [rsp], [rbp+disp32], [rip+disp32], [r12+rax*8], [r9]
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x32]), Some((ConsoleOnly::Clwb, 4)));
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x73, 0x10]), Some((ConsoleOnly::Clwb, 5)));
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x34, 0x24]), Some((ConsoleOnly::Clwb, 5)));
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0xB5, 0, 1, 0, 0]), Some((ConsoleOnly::Clwb, 8)));
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x35, 0, 0, 0, 0]), Some((ConsoleOnly::Clwb, 8)));
        assert_eq!(decode(&[0x66, 0x41, 0x0F, 0xAE, 0x34, 0xC4]), Some((ConsoleOnly::Clwb, 6)));
        assert_eq!(decode(&[0x66, 0x41, 0x0F, 0xAE, 0x31]), Some((ConsoleOnly::Clwb, 5)));
        // A SIB with no base: disp32.
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x34, 0x25, 0, 0, 0, 0]), Some((ConsoleOnly::Clwb, 9)));
        // Not CLWB: /7 (CLFLUSHOPT), no 66 (XSAVEOPT), a register operand, bytes cut short.
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x3A]), None);
        assert_eq!(decode(&[0x0F, 0xAE, 0x32]), None);
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0xF2]), None);
        assert_eq!(decode(&[0x66, 0x0F, 0xAE, 0x73]), None);
    }

    #[test]
    fn rdpid_names_its_register() {
        assert_eq!(decode(&[0xF3, 0x0F, 0xC7, 0xFB]), Some((ConsoleOnly::Rdpid(3), 4)));
        assert_eq!(decode(&[0xF3, 0x49, 0x0F, 0xC7, 0xFF]), Some((ConsoleOnly::Rdpid(15), 5)));
        // RDRAND (/6) and a memory operand are not RDPID.
        assert_eq!(decode(&[0xF3, 0x0F, 0xC7, 0xF3]), None);
        assert_eq!(decode(&[0xF3, 0x0F, 0xC7, 0x3B]), None);
    }

    #[test]
    fn sse4a_forms() {
        // insertq xmm3, xmm5, 12, 40 / insertq xmm10, xmm4 (lengths in the register)
        assert_eq!(
            decode(&[0xF2, 0x0F, 0x78, 0xDD, 12, 40]),
            Some((ConsoleOnly::Insertq { dst: 3, src: 5, reg: false, len: 12, idx: 40 }, 6))
        );
        assert_eq!(
            decode(&[0xF2, 0x44, 0x0F, 0x79, 0xD4]),
            Some((ConsoleOnly::Insertq { dst: 10, src: 4, reg: true, len: 0, idx: 0 }, 5))
        );
        // extrq xmm6, 7, 3 / extrq xmm2, xmm11
        assert_eq!(decode(&[0x66, 0x0F, 0x78, 0xC6, 7, 3]), Some((ConsoleOnly::Extrq { dst: 6, src: None, len: 7, idx: 3 }, 6)));
        assert_eq!(
            decode(&[0x66, 0x41, 0x0F, 0x79, 0xD3]),
            Some((ConsoleOnly::Extrq { dst: 2, src: Some(11), len: 0, idx: 0 }, 5))
        );
        // Immediates cut short, VMREAD/VMWRITE (no prefix), extrq with a reg field not 0.
        assert_eq!(decode(&[0xF2, 0x0F, 0x78, 0xDD, 12]), None);
        assert_eq!(decode(&[0x0F, 0x78, 0xDD, 12, 40]), None);
        assert_eq!(decode(&[0x66, 0x0F, 0x78, 0xCE, 7, 3]), None);
    }

    #[test]
    fn prefix_order() {
        let insertq = Some((ConsoleOnly::Insertq { dst: 1, src: 2, reg: false, len: 4, idx: 4 }, 7));
        // The last of F2 / F3 decides; 66 beside F2 does not matter; a REX followed by
        // another prefix is dropped.
        for code in [
            [0x66, 0xF2, 0x0F, 0x78, 0xCA, 4, 4],
            [0xF2, 0x66, 0x0F, 0x78, 0xCA, 4, 4],
            [0xF3, 0xF2, 0x0F, 0x78, 0xCA, 4, 4],
            [0x67, 0xF2, 0x0F, 0x78, 0xCA, 4, 4],
            [0x44, 0xF2, 0x0F, 0x78, 0xCA, 4, 4],
        ] {
            assert_eq!(decode(&code), insertq, "{code:02x?}");
        }
        // F3 last: neither SSE4a form.
        for code in [[0xF2, 0xF3, 0x0F, 0x78, 0xCA, 4, 4], [0x66, 0xF3, 0x0F, 0x78, 0xC2, 4, 4]] {
            assert_eq!(decode(&code), None, "{code:02x?}");
        }
        assert_eq!(decode(&[0x48, 0x89, 0xC3]), None);
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[0x0F]), None);
    }
}
