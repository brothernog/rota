# Rota Profiler

Explains every frame of a run: where its wall time went and what the title asked for in it.

It has two parts:

- **The frame log**, in the runtime (`crates/victory-gpu/src/framelog`). A diagnostics build writes one CSV line per present when `VICTORY_FRAME_LOG=<file>` is set. Nothing is recorded without the variable, and a build without `diagnostics` has none of it.
- **`frames.py`** reads the log: `python -I frames.py <frames.csv>`.

## Recording

```powershell
$env:VICTORY_FRAME_LOG = 'local\logs\frames.csv'   # then run the title as usual (diagnostics build)
```

Write the CSV under `local/logs` (ignored by git). It holds numbers only: no names, no paths, no shader names.

`t_s` uses the same clock as the overlay CSV's `t_s` and the overlay's "Run time" row: seconds since the overlay started. A drop seen live at "t 187 s" is the line with `t_s` near 187. The column is empty until the overlay has started.

## How a frame is split

Every thread keeps one byte that says what of ours it is in: a list call, a submission, a map call, one of the title's waits, a frame wait, the present. A few global words say what the submission under the lock is doing (preparing, waiting for the card, copying back, waiting for flights to land) and what the landing thread is doing (idle, waiting for the card, landing). Setting them is a store to a cache line of the thread's own.

A thread of ours looks at them about every millisecond. It uses a high-resolution waitable timer and does not change the system timer, so the title's own sleeps are not affected. The time since its last look goes to one bucket. The intervals are cut at each present's time, so **a frame's buckets add up to its wall time exactly**. `frames.py` checks this on every line.

The bucket is chosen from the state of the thread that presents:

| that thread is in | bucket |
|---|---|
| our list call | `recording` |
| a pipeline wait / a write tracker's lock / a map call | `pipelines` / `guards` / `xmem_title` or `xmem_queue` |
| a submission, or the present, with the lock holder at a phase | `preparing`, `gpu_wait`, `copy_back` (while flights land: by what the landing thread does) |
| a submission otherwise (waiting for the lock, the sweep) | `submit` |
| the present otherwise / the title's frame wait | `present` / `pace` |
| a wait of the title's | whatever of ours is going on anywhere, in the order submission, title map calls, queue map calls, pipeline waits, tracker locks; with nothing of ours, `title_wait` (`fence_wait` for an event one of our fences sets) |
| the title's own code | its processor time (`QueryThreadCycleTime`) is `title_running`; the rest of the tick goes to what of ours is going on, else `unexplained` |

So **`unexplained`** is time when the presenting thread was neither in code of ours, nor in one of the title's waits we wrap, nor running, while nothing of ours was going on anywhere. Examples: a wait through a function we do not wrap, a page fault, or a thread ready to run with no free core. `title_wait` and `fence_wait` say that the title waited, but not why, so `frames.py` counts them as **unknown** together with `unexplained`. The validation target is unknown under 10% on nearly all slow frames.

Two limits to keep in mind:

- Activity on other threads is a correlate. A title wait during a map call elsewhere goes to `xmem_title`, whether or not that particular wait was for the map call. `xmem_active_ms` gives the wall time with any map call going on, whichever bucket won.
- `pre_list_ms` is the time from the present to the first list call. It is a phase, not a cause, and it overlaps the buckets.

## Other columns

All of these are summed over all threads for the frame, so they can exceed its wall time.

- `xmem_title_*` / `xmem_queue_*`: map calls of the title (`XMemMapPhysicalPages(Scatter)`) and of the queues' `CopyPageMappings`, as calls, pages and ms. Only the outermost call of a thread is counted.
- `title_waits`, `title_wait_ms`: the title's waits. This includes its locks, but only after a try that failed.
- `list_calls`, `list_ms`, `submissions`, `submit_lock_ms`.
- `tex_uploads`, `tex_unchanged`: texture uploads recorded, and those left out as unchanged. `textures_waiting` is a gauge.
- `pipelines_asked` (first use by the title), `pipelines_made`, `pipelines_loaded` (from the library), `pipelines_making` (a gauge), `pipeline_waits`, `pipeline_wait_ms`, `dropped` (draws dropped, any reason), `dropped_rt_off` (dispatches left out because rays are off).
- `ds_reads`, `ds_bytes`, `ds_title_ms` (the title's enqueue to its submit), `ds_queue_ms` (submit to the read's start), `ds_service_ms` (the read).
- `guard_waits`, `guard_wait_ms`, `guard_faults`.
- `process_cpu_ms`, `page_faults`, `title_cpu_ms` (the presenting thread's processor time).
- `ticks`, `tick_max_ms`: the looking thread's looks in the frame and its longest gap. A long gap means the looker was starved and the frame's split is coarse.

## frames.py

```powershell
python -I tools/rota-profiler/frames.py local/logs/frames.csv
python -I tools/rota-profiler/frames.py local/logs/frames.csv --window 10
python -I tools/rota-profiler/frames.py --compare a.csv b.csv
python -I tools/rota-profiler/frames.py --self-test
```

For a single run it prints:

- the run's share per bucket;
- the share of slow frames whose unknown time is under 10%;
- **drops**: runs of frames at least 1.5x the median within 5 s and at least 40 ms long, each with its dominant bucket, unexplained and unknown shares, and the counters furthest above the run's median;
- **peaks**: frames at most 0.67x the median around them;
- a summary per time window.

`--compare` prints two runs side by side: fps, ms per frame by bucket, and counts per frame.
