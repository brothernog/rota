//! Cube samples with gradients select their level by the footprint (after
//! DexNom/VIctory 921d8d3b).

use super::host::{Dispatch, Host, Root, View};
use super::intersect_tests::build_compute_shader;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use windows::Win32::Graphics::Direct3D12::*;

/// The level is the red channel on all six faces so every sample returns
/// its selected mip directly (including the fraction with trilinear filtering).
#[test]
fn guest_cube_gradient_selects_mip_by_footprint() {
    const SIZE: usize = 64;
    const MIPS: u16 = 7;
    let mut words = vec![
        0xBE81_0380,
        0xBE83_0380, // s1, s3 = 0 (upper halves of the tables)
        0xF40C_0400,
        0xFA00_0000, // cube SRV to s[16:23] from s[0:1]
        0xF408_0300,
        0xFA00_0020, // raw output UAV to s[12:15] from s[0:1]+32
        0xF408_0601,
        0xFA00_0000, // sampler to s[24:27] from s[2:3]
        0x7E1E_0280, // v15 = 0 for output byte offsets
    ];
    let cases = [
        // image_sample_d: successive XY footprints (in texels).
        (0x22, 1.0, 0.0, 0.0),
        (0x22, 2.0, 0.0, 1.0),
        (0x22, 16.0, 0.0, 4.0),
        (0x22, 0.000001, 0.0, 0.0),
        (0x22, 0.0, 0.0, 0.0),
        // image_sample_d_cl: an instruction-specified minimum level.
        (0x23, 1.0, 3.0, 3.0),
        (0x23, 16.0, 2.0, 4.0),
        (0x23, 0.0, 2.0, 2.0),
        // image_sample_l and image_sample_lz: the level is given directly.
        (0x24, 0.0, 2.0, 2.0),
        (0x27, 0.0, 0.0, 0.0),
    ];
    for (i, &(op, footprint, lod, _)) in cases.iter().enumerate() {
        let delta = footprint / SIZE as f32;
        let mut addr = Vec::new();
        if op == 0x22 || op == 0x23 {
            addr.extend([delta, 0.0, 0.0, delta]); // derivatives along the face
        }
        addr.extend([1.5f32, 1.5, 0.0]); // positive X face, center
        if op == 0x23 || op == 0x24 {
            addr.push(lod);
        }
        for (reg, value) in addr.iter().enumerate() {
            words.extend([0x7E00_02FF | ((reg as u32) << 17), value.to_bits()]);
        }
        // MIMG, CUBE, dmask:R, v0... inputs, SRV s16, sampler s24, result v10.
        words.extend([0xF000_0118 | (op << 18), 0x00C4_0A00]);
        // buffer_store_dword v10, v15, s[12:15], 0 offen offset:i*4
        words.extend([0xE070_1000 | (i as u32 * 4), 0x8003_0A0F]);
    }
    words.push(0xBF81_0000); // s_endpgm
    let table = |slot, user_data| RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot,
    };
    let range = |kind, at, words, slot, user_data| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 0,
        dwords: words,
        user_data,
        slot,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: vec![table(0, 0), table(1, 2)],
        used: vec![
            range(ResourceType::Srv, 0, 8, 0, 0),
            range(ResourceType::Uav, 8, 4, 0, 0),
            range(ResourceType::Sampler, 0, 4, 1, 2),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 3 << 1,
            threads: [1, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    };
    let (shader, _) = build_compute_shader(&native).unwrap();
    let texels: Vec<_> = (0..6)
        .flat_map(|_| {
            (0..MIPS).flat_map(|mip| std::iter::repeat_n([f32::from(mip); 4], (SIZE >> mip).pow(2)))
        })
        .collect();
    let sampler = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxLOD: f32::MAX,
        MaxAnisotropy: 1,
        ..Default::default()
    };
    for (name, warp) in [("WARP", true), ("hardware", false)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {name} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).unwrap();
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(
                        0,
                        Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                    )],
                    views: &[
                        View::Cube(SIZE as u32, MIPS, &texels),
                        View::RawUav(cases.len() as u32),
                    ],
                    samplers: &[sampler],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        let levels: Vec<_> = out[0].iter().copied().map(f32::from_bits).collect();
        let expected = cases.map(|(_, _, _, mip)| mip);
        assert_eq!(levels.as_slice(), expected, "{name}");
    }
}

/// A compute program of `words` that reads a cube SRV, a raw output UAV and
/// a sampler (the tables of the test above), in groups of `threads`.
fn cube_program(words: Vec<u32>, threads: [u32; 3]) -> NativeProgram {
    let table = |slot, user_data| RootParam {
        root: RootType::Table,
        table_offset_dwords: 0,
        sgprs: 1,
        base_register: 0,
        user_data,
        slot,
    };
    let range = |kind, at, words, slot, user_data| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords: at,
        lower_bound: 0,
        stride: 0,
        dwords: words,
        user_data,
        slot,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: vec![table(0, 0), table(1, 2)],
        used: vec![
            range(ResourceType::Srv, 0, 8, 0, 0),
            range(ResourceType::Uav, 8, 4, 0, 0),
            range(ResourceType::Sampler, 0, 4, 1, 2),
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 3 << 1,
            threads,
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    }
}

/// The descriptor loads of [`cube_program`]'s tables.
const CUBE_PROLOGUE: [u32; 8] = [
    0xBE81_0380,
    0xBE83_0380, // s1, s3 = 0 (upper halves of the tables)
    0xF40C_0400,
    0xFA00_0000, // cube SRV to s[16:23] from s[0:1]
    0xF408_0300,
    0xFA00_0020, // raw output UAV to s[12:15] from s[0:1]+32
    0xF408_0601,
    0xFA00_0000, // sampler to s[24:27] from s[2:3]
];

/// Runs `native` on WARP and on the hardware device (where there is one)
/// with `cube` and a raw UAV of `out` dwords; each device's UAV as floats.
fn run_cube(native: &NativeProgram, cube: View, out: u32) -> Vec<(&'static str, Vec<f32>)> {
    let (shader, _) = build_compute_shader(native).unwrap();
    let sampler = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxLOD: f32::MAX,
        MaxAnisotropy: 1,
        ..Default::default()
    };
    let mut results = Vec::new();
    for (name, warp) in [("WARP", true), ("hardware", false)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {name} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).unwrap();
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(
                        0,
                        Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                    )],
                    views: &[cube, View::RawUav(out)],
                    samplers: &[sampler],
                    groups: [1, 1, 1],
                },
            )
            .unwrap();
        results.push((name, out[0].iter().copied().map(f32::from_bits).collect()));
    }
    results
}

/// A cube array's face operand is 8 × slice + face.
#[test]
#[ignore = "issue #5: a face operand past 5 reads +X (lower.rs, cube_direction)"]
fn cube_face_operand_of_a_later_slice_reads_its_face() {
    const SIZE: usize = 4;
    let mut words = CUBE_PROLOGUE.to_vec();
    words.push(0x7E1E_0280); // v15 = 0 for output byte offsets
    let faces = [0.0f32, 1.0, 4.0, 5.0, 8.0, 9.0, 12.0, 13.0];
    for (i, &face) in faces.iter().enumerate() {
        for (reg, value) in [1.5f32, 1.5, face, 0.0].iter().enumerate() {
            words.extend([0x7E00_02FF | ((reg as u32) << 17), value.to_bits()]);
        }
        // image_sample_l, CUBE, dmask:R, address v[0:3], result v10.
        words.extend([0xF000_0118 | (0x24 << 18), 0x00C4_0A00]);
        // buffer_store_dword v10, v15, s[12:15], 0 offen offset:i*4
        words.extend([0xE070_1000 | (i as u32 * 4), 0x8003_0A0F]);
    }
    words.push(0xBF81_0000); // s_endpgm
    // Face f of cube c holds 10c + f + 1.
    let texels: Vec<_> = (0..12u8)
        .flat_map(|f| std::iter::repeat_n([f32::from(f / 6 * 10 + f % 6 + 1); 4], SIZE * SIZE))
        .collect();
    let native = cube_program(words, [1, 1, 1]);
    let cubes = View::CubeArray(SIZE as u32, 1, 2, &texels);
    for (name, got) in run_cube(&native, cubes, faces.len() as u32) {
        let read: Vec<_> = got.iter().map(|v| v - 1.0).collect();
        assert_eq!(read, [0.0, 1.0, 4.0, 5.0, 10.0, 11.0, 14.0, 15.0], "{name}");
    }
}

/// An implicit-LOD sample in a compute program takes its derivatives from
/// the quad's lanes, as in a pixel program (lane ^ 1 across x, ^ 2 across
/// y). Four lanes of a 64² cube, 8 texels apart (so 8 across x, 16 across
/// y): level 4.
#[test]
#[ignore = "issue #5: implicit-LOD samples in compute programs read level 0 (lower.rs)"]
fn cube_implicit_lod_in_compute_follows_the_quad() {
    const SIZE: usize = 64;
    const MIPS: u16 = 7;
    let delta = 8.0f32 / SIZE as f32;
    let mut words = CUBE_PROLOGUE.to_vec();
    words.extend([
        0x7E02_0D00, // v_cvt_f32_u32 v1, v0 (thread id x)
        0x1002_02FF,
        delta.to_bits(), // v_mul_f32 v1, delta, v1
        0x0602_02FF,
        1.5f32.to_bits(), // v_add_f32 v1, 1.5, v1
        0x7E04_02FF,
        1.5f32.to_bits(), // v2 = 1.5
        0x7E06_0280,      // v3 = 0 (+X face)
        0x341E_0082,      // v_lshlrev_b32 v15, 2, v0
        // image_sample, CUBE, dmask:R, address v[1:3], result v10.
        0xF000_0118 | (0x20 << 18),
        0x00C4_0A01,
        0xE070_1000,
        0x8003_0A0F, // buffer_store_dword v10, v15 offen
        0xE070_1010,
        0x8003_010F, // buffer_store_dword v1, v15 offen offset:16
        0xBF81_0000, // s_endpgm
    ]);
    let texels: Vec<_> = (0..6)
        .flat_map(|_| {
            (0..MIPS).flat_map(|mip| std::iter::repeat_n([f32::from(mip); 4], (SIZE >> mip).pow(2)))
        })
        .collect();
    let native = cube_program(words, [4, 1, 1]);
    for (name, got) in run_cube(&native, View::Cube(SIZE as u32, MIPS, &texels), 8) {
        // The lanes' coordinates differ: the probe itself is right.
        assert_eq!(got[4..], [1.5, 1.625, 1.75, 1.875], "{name}: coordinates");
        assert_eq!(got[..4], [4.0; 4], "{name}: levels");
    }
}
