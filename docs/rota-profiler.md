# Rota Profiler (frame log)

`VICTORY_FRAME_LOG=<file>` (diagnostics builds) writes one CSV line per present. Each line splits the frame's wall time into named buckets that add up to it exactly, ending with an explicit `unexplained` bucket. It also counts what the title asked for in that frame: map calls and pages, waits, textures, pipelines, DirectStorage reads. `t_s` uses the overlay CSV's clock.

The design is cheap counters that are always on, plus one thread that looks at per-thread state bytes about every millisecond. There is no stack sampling.

`tools/rota-profiler/frames.py` lists a run's drops and peaks, each with its dominant bucket and residual, summarizes by time window, and compares two runs.

See `tools/rota-profiler/README.md` for the bucket rules and the columns.
