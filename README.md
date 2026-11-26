# Redemption of Them All

**An Xbox-to-PC compatibility layer focused on cheaper graphics cards.** It runs the
**Xbox Series X|S** build (GDK, *Scarlett*) of a title natively on Windows, from your own copy,
and is tuned for low-end and older cards: the RTX 3050, 3060 and 2070 class, 8 GB of video
memory or less, 16 to 32 GB of system memory.

The console's x86-64 code runs unchanged inside the process. Under it sit reimplemented console
system libraries (HLE) and a Direct3D 12 back end that recompiles the console's GPU programs
(GFX10/RDNA2) to DXIL.

> **No games here.** This project is not affiliated with, endorsed by or connected to Microsoft
> or any game, developer or publisher. It does not provide, and will not help you get, game
> files, executables, keys, decryption, cracks or any other way around buying a game. It
> contains no game code or assets. You need your own legally obtained copy. All trademarks
> belong to their owners.

## Goal

Make the title run as well as possible on cards with little memory and no headroom. That means:

- **Less memory, first.** Video memory, and the PC memory it spills into, is the wall on an
  8 GB card. Work goes into texture residency, upload budgets and what the layer keeps around.
- **Less work per frame.** Skip passes nobody sees, copy less between the PC and the card,
  keep the CPU from preparing the same thing twice.
- **No new-card features.** Nothing here needs the newest GPUs or drivers to run, and nothing
  that only helps high-end cards is a priority.

## Where it is

**Not playable yet.** Measured on an RTX 3050 8 GB with 32 GB of system memory, 1080p, the
Series S profile and the `diag` build: about **3.4 fps** (median) in the first playable area,
10% of the time below 1.3 fps. Most of each frame goes to the CPU preparing work for the card
(copies and texture uploads), not to the card itself.

## What this fork adds

- **A performance overlay** (`crates/victory-gpu/src/overlay.rs`): frame rate, where the frame
  time goes (preparing, copies, textures, waiting for the GPU, copying back), draws, pipelines
  being made, textures waiting, CPU and GPU load, system and process memory, video memory and
  how much of it spilled into PC memory. It is a separate click-through window: no GPU work,
  the swap chain untouched. `F10` hides it, `VICTORY_OVERLAY=0` turns it off, and
  `VICTORY_OVERLAY_CSV=<file>` writes the same numbers once a second (numbers only).

Inherited from the forks it starts from, and kept on:

- `VICTORY_RT_MISS=1`: every ray misses. Without the ray-tracing structures built, local light
  shadows read garbage; with every ray a miss they are stable, and the programs that only feed
  those structures are skipped.
- A new texture shows in its slot only after its upload (`VICTORY_SHOW_AFTER`, on by default).
- The title's own upscaler on the Series S profile (`VICTORY_UPSCALER=6`).
- Pipeline creation tuned for NVIDIA drivers, and wave64 programs in their wide form on
  32-lane cards.

## Build and run

You need Windows 10 or 11, a Direct3D 12 card, and Rust (the version is pinned in
`rust-toolchain.toml`).

```powershell
copy victory.example.toml victory.toml      # then point game_dir at your own copy
cargo build --locked --profile diag -p victory --features diagnostics
target\diag\victory.exe shaders <your copy's shader files>
target\diag\victory.exe run <your copy's executable> --console series-s --execute
```

The settings the numbers above were measured with:

```powershell
$env:VICTORY_RT_MISS = '1'; $env:VICTORY_RELAY = '2'; $env:VICTORY_THIN = '0'
$env:VICTORY_DISPLAY_MODE = '1920x1080'; $env:VICTORY_PIPELINE_BOOK = '1'
$env:VICTORY_MEMORY_CARD_MB = '1024'; $env:VICTORY_UPSCALER = '6'
$env:VICTORY_MEMORY_MAP = '0'
```

Keep `VICTORY_CACHE_DIR` and `VICTORY_DATA_DIR` outside this source tree: the layer refuses to
write anything derived from the title below it. Do not set `VICTORY_THIN=1` on NVIDIA cards
(it more than halves the frame rate there).

## Documentation

- [docs/architecture.md](docs/architecture.md): how the layer is put together.
- [docs/asset-policy.md](docs/asset-policy.md): what may never enter the repository.
- [docs/replay.md](docs/replay.md): recording a frame and running it again.
- [CONTRIBUTING.md](CONTRIBUTING.md): rules for code, commits and pull requests.
- [AGENTS.md](AGENTS.md): build, test and measuring steps for automated agents.

## License

GPL-3.0-or-later; see [LICENSE](LICENSE). The authors of the imported code are listed in
[AUTHORS.md](AUTHORS.md). The BCPack format tables in `crates/victory-formats` are not covered
by this license; see [docs/asset-policy.md](docs/asset-policy.md).
