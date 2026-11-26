<div align="center">

# Redemption of Them All

**An Xbox-to-PC compatibility layer focused on cheaper graphics cards.**

[![CI](https://github.com/brothernog/rota/actions/workflows/ci.yml/badge.svg)](https://github.com/brothernog/rota/actions/workflows/ci.yml)
![License](https://img.shields.io/badge/license-GPL--3.0--or--later-blue)
![Platform](https://img.shields.io/badge/platform-Windows%2010%20%7C%2011-0078d4)
![Target](https://img.shields.io/badge/target-8%20GB%20cards-76b900)
![Status](https://img.shields.io/badge/status-playable%2C%20slow-orange)

</div>

It runs the **Xbox Series X|S** build (GDK, *Scarlett*) of a title natively on Windows, from your
own copy, and is tuned for low-end and older cards. The console's x86-64 code runs unchanged
inside the process. Under it sit reimplemented console system libraries (HLE) and a Direct3D 12
back end that recompiles the console's GPU programs (GFX10/RDNA2) to DXIL.

> [!IMPORTANT]
> **No games here.** This project is not affiliated with, endorsed by or connected to Microsoft
> or any game, developer or publisher. It does not provide, and will not help you get, game
> files, executables, keys, decryption, cracks or any other way around buying a game. It
> contains no game code or assets. You need your own legally obtained copy. All trademarks
> belong to their owners.

## Who it is for

| | Cards | System memory | Status |
|---|---|---|---|
| **Tested** | RTX 3050 8 GB | 32 GB | playable, ~4.5 fps |
| **Target** | RTX 3060, RTX 2070, other 6–8 GB cards | 16–32 GB | not measured yet |
| **Not a goal** | features that only the newest cards have | | |

Work goes, in order, into **less memory** (video memory and the PC memory it spills into are the
wall on an 8 GB card), **less work per frame** (skip passes nobody sees, copy less between the PC
and the card) and **no new-card features**: nothing here needs the newest GPUs or drivers.

## Where it is

**Playable, slowly.** On the RTX 3050 the title boots, takes input and runs through the first
mission without crashing so far, at about **4.5 fps** in the first playable area (1080p, Series S
profile, `diag` build; the best measured run had a median of 4.6 fps, and 10% of its time was
under 2.8 fps).

The first round of measurements, one setting changed at a time:

```mermaid
xychart-beta
    title "Median fps in the first playable area (RTX 3050 8 GB)"
    x-axis ["Texture cap 3072 MB", "No texture cap (now)", "Texture levels on demand"]
    y-axis "fps" 0 --> 6
    bar [3.35, 4.6, 2.6]
```

Where the time of one frame goes in the best run (218 ms a frame). The card is now the larger
share, and memory is full: 31.4 of 32 GB of system memory in use, and up to 6.75 GB of video
memory spilled into PC memory.

```mermaid
xychart-beta
    title "One frame, best run (ms)"
    x-axis ["CPU preparing", "Waiting for the GPU", "Copying back"]
    y-axis "ms" 0 --> 120
    bar [77, 110, 61]
```

## What this fork adds

- **A performance overlay** (`crates/victory-gpu/src/overlay.rs`): frame rate, where the frame
  time goes (preparing, copies, textures, waiting for the GPU, copying back), draws, pipelines
  being made, textures waiting, CPU and GPU load, system and process memory, video memory and
  how much of it spilled into PC memory. A separate click-through window: no GPU work, the swap
  chain untouched. `F10` hides it, `VICTORY_OVERLAY=0` turns it off.
- **Numbers you can share.** `VICTORY_OVERLAY_CSV=<file>` writes the overlay's numbers once a
  second, and [`scripts/overlay-summary.ps1`](scripts/overlay-summary.ps1) turns a run into one
  line of medians and peaks, ready for an issue.
- **Measured defaults for 8 GB cards**: no texture budget cap (`VICTORY_TEXTURE_BUDGET_MB`
  unset) and `VICTORY_MEMORY_MAP=0`.

Inherited from the forks it starts from, and kept on:

- `VICTORY_RT_MISS=1`: every ray misses. Without the ray-tracing structures built, local light
  shadows read garbage; with every ray a miss they are stable, and the programs that only feed
  those structures are skipped.
- A new texture shows in its slot only after its upload (`VICTORY_SHOW_AFTER`, on by default).
- The title's own upscaler on the Series S profile (`VICTORY_UPSCALER=6`).
- Pipeline creation tuned for NVIDIA drivers, and wave64 programs in their wide form on 32-lane
  cards.

## Build and run

You need Windows 10 or 11, a Direct3D 12 card, and Rust (the version is pinned in
`rust-toolchain.toml`).

```powershell
copy victory.example.toml victory.toml      # then point game_dir at your own copy
cargo build --locked --profile diag -p victory --features diagnostics
target\diag\victory.exe shaders <your copy's shader files>
target\diag\victory.exe run <your copy's executable> --console series-s --execute   # with VICTORY_PIPELINE_BOOK=1
target\diag\victory.exe warm --used
target\diag\victory.exe run <your copy's executable> --console series-s --execute
```

On a clean setup `warm --used` knows no pipelines yet ("0 pipelines known"): the list is written
by a run with `VICTORY_PIPELINE_BOOK=1`. So the first run is cold (dropped draws, a mostly black
picture), then warm, then run again. The warm-up's bookkeeping is kept per card, so after
switching cards `warm --used` makes that card's pipelines on its own.

The settings the numbers above were measured with:

```powershell
$env:VICTORY_RT_MISS = '1'; $env:VICTORY_RELAY = '2'; $env:VICTORY_THIN = '0'
$env:VICTORY_DISPLAY_MODE = '1920x1080'; $env:VICTORY_PIPELINE_BOOK = '1'
$env:VICTORY_MEMORY_CARD_MB = '1024'; $env:VICTORY_UPSCALER = '6'
$env:VICTORY_MEMORY_MAP = '0'
```

> [!TIP]
> Warm with the same settings as the run. A rebuild that does not change the shader translator
> keeps the warm-up (pipelines load from the layer's pipeline library); one that does needs
> `warm --used` again, or the driver makes pipelines while you play.

Keep `VICTORY_CACHE_DIR` and `VICTORY_DATA_DIR` outside this source tree: the layer refuses to
write anything derived from the title below it. Do not set `VICTORY_THIN=1` on NVIDIA cards
(it more than halves the frame rate there).

## Documentation

| | |
|---|---|
| [docs/architecture.md](docs/architecture.md) | how the layer is put together |
| [docs/asset-policy.md](docs/asset-policy.md) | what may never enter the repository |
| [docs/replay.md](docs/replay.md) | recording a frame and running it again |
| [CONTRIBUTING.md](CONTRIBUTING.md) | rules for code, commits and pull requests |
| [AGENTS.md](AGENTS.md) | build, test and measuring steps for automated agents |

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). The authors of the imported code are listed in
[AUTHORS.md](AUTHORS.md). The BCPack format tables in `crates/victory-formats` are not covered
by this license; see [docs/asset-policy.md](docs/asset-policy.md).
