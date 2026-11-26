# Notes for automated agents

Read [CONTRIBUTING.md](CONTRIBUTING.md) first. The short version of the rules:

- **Nothing from any game enters this repository**: no game files, executables, shaders (in any
  form, their names included), textures, audio, screenshots or logs that quote them. Shader
  hashes and plain numbers (fps, milliseconds, megabytes, counts, error codes) are fine.
- Do not name games, publishers or their staff in code, comments, commits or pull requests;
  say "the title".
- Do not loosen `crates/victory-hle/src/modules/net.rs`: the title may only reach loopback.
- No GPL-2.0-only code, nothing from repositories without a licence.
- Commits in English, no AI attribution lines. A pull request body ends with `AI-assisted.`

## Check a change (no game needed)

```powershell
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked                  # no new warnings in what you touched
cargo clippy --workspace --all-targets --all-features --locked   # the diagnostics build
cargo xtask policy                                               # the asset policy (also a test)
```

All of it runs without the title and without a GPU-heavy setup; tests that need a Direct3D 12
device skip themselves when there is none.

## Measure a change (needs your own copy)

1. Build: `cargo build --locked --profile diag -p victory --features diagnostics`.
2. Warm the driver's pipeline cache **after** every build, with the same environment as the run:
   `target\diag\victory.exe warm --used`. A new build loses the warm-up; a run on a cold cache
   spends its frames making pipelines and its numbers do not compare.
3. Run with the overlay's CSV on, and keep `VICTORY_CACHE_DIR` / `VICTORY_DATA_DIR` outside the
   source tree (the layer refuses to write title-derived data below it):

   ```powershell
   $env:VICTORY_OVERLAY_CSV = "$env:TEMP\run.csv"
   target\diag\victory.exe run <your copy's executable> --console series-s --execute
   ```

   The settings the README's numbers use are listed there ("Build and run").
4. Summarize: `scripts\overlay-summary.ps1 $env:TEMP\run.csv`. It prints medians and peaks,
   numbers only, and is what goes into a pull request or an issue.

One run is not a result: the same settings have varied by a factor of two between runs. Compare
at least two runs per setting, the same route each time, and change one setting at a time.

What to look at: `preparing_ms` (CPU time building each frame), `gpu_wait_ms` (the card), and
the memory peaks. `shared_used_gb` above a few hundred MB means video memory spilled into PC
memory, which is the main limit on 8 GB cards.
