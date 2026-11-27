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
| a wait of the title's | whatever of ours is going on anywhere, in the order submission, title map calls, queue map calls, pipeline waits, tracker locks, list calls on other threads (`recording`), DirectStorage reads (`dstorage`); with nothing of ours, `title_wait` (`fence_wait` for an event one of our fences sets) |
| the title's own code | its processor time (`QueryThreadCycleTime`) is `title_running`; the rest of the tick goes to what of ours is going on, else `unexplained` |

So **`unexplained`** is time when the presenting thread was neither in code of ours, nor in one of the title's waits we wrap, nor running, while nothing of ours was going on anywhere. Examples: a wait through a function we do not wrap, a page fault, or a thread ready to run with no free core. `title_wait` and `fence_wait` say that the title waited, but not why, so `frames.py` counts them as **unknown** together with `unexplained`. The validation target is unknown under 10% on nearly all slow frames.

Two limits to keep in mind:

- Activity on other threads is a correlate. A title wait during a map call elsewhere goes to `xmem_title`, whether or not that particular wait was for the map call. `xmem_active_ms` gives the wall time with any map call going on, whichever bucket won. The same holds for `recording` (another thread is in a list call; in the world one nearly always is) and `dstorage` (a read is running). The waker columns (`frame_wait_*`) are the direct evidence: a waker was matched to the object the presenting thread waited on.
- `pre_list_ms` is the time from the present to the first list call. It is a phase, not a cause, and it overlaps the buckets.

## Other columns

All of these are summed over all threads for the frame, so they can exceed its wall time.

- `xmem_title_*` / `xmem_queue_*`: map calls of the title (`XMemMapPhysicalPages(Scatter)`) and of the queues' `CopyPageMappings`, as calls, pages and ms. Only the outermost call of a thread is counted.
- `title_waits`, `title_wait_ms`: the title's waits. This includes its locks, but only after a try that failed.
- `list_calls`, `list_ms`, `submissions`, `submit_lock_ms`.
- `tex_uploads`, `tex_unchanged`: texture uploads recorded, and those left out as unchanged. `textures_waiting` is a gauge.
- `pipelines_asked` (first use by the title), `pipelines_made`, `pipelines_loaded` (from the library), `pipelines_making` (a gauge), `pipeline_waits`, `pipeline_wait_ms`, `dropped` (draws dropped, any reason), `dropped_rt_off` (dispatches left out because rays are off).
- `ds_reads`, `ds_bytes`, `ds_title_ms` (the title's enqueue to its submit), `ds_queue_ms` (submit to the read's start), `ds_service_ms` (the read).
- `ds_pending_ms`: the frame's wall time with DirectStorage reads submitted and not done (queued or being read). `unknown_ds_pending_ms`: the part of it that went to an unknown bucket (`title_wait`, `fence_wait`, `unexplained`). A correlate, like `xmem_active_ms`: it says the title's unexplained wait overlapped pending reads, not that it waited for them.
- `guard_waits`, `guard_wait_ms`, `guard_faults`.
- `up_wc_bytes`, `up_other_bytes`: bytes copied from title memory up to the card's copies. `back_compared_wc_bytes`, `back_compared_other_bytes`: bytes of the copy-back compared after a submission. `back_wc_bytes`, `back_other_bytes`: bytes put back into title memory. Each pair is split by whether the title allocated that memory write-combined (protection bit 0x400 in `XMemVirtualAlloc`). Memory the title writes and does not read could skip the copy-back. A partial copy of a range is split in proportion to the range's write-combined bytes. Texture uploads are not in these columns.
- `frame_wait_title_ms`, `frame_wait_fence_ms`, `frame_wait_timer_ms`, `frame_wait_other_ms`: the presenting thread's own waits, by what ended them. *Title*: a thread of the title's set, released or woke the object (through the functions the frame log wraps). *Fence*: one of our fences set the event. *Timer*: a sleep. *Other*: a time-out, or a waker we do not see (our own code setting an event directly, a kernel object). A wait on several objects is matched by its first object only.
- `process_cpu_ms`, `page_faults`, `title_cpu_ms` (the presenting thread's processor time).
- `working_set_mb`, `private_mb` (the process), `available_mb` (the system's free physical memory), gauges at the frame's end. `page_reads`, `pages_input`: the system's reads from disk for page faults in the frame, and the pages they brought in (all processes, from PDH). With `page_faults` they tell soft faults from faults that read the disk.
- `menu_bits`: the switches menu's state at the line, a bit per switch (`VICTORY_MENU=1`; empty without the menu). `frames.py` sums up the frames of each state.
- `list_lock_ms`: list calls waiting for their list's state. `list_top{1,2,3}_line`, `_ms`, `_calls`: the three list call sites that took the most time in the frame, each named by its line in `crates/victory-gpu/src/list.rs` (one per method), with their ms and calls.
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

## Example: an automatic run

Measured on an 8 GB card in one automatic run of 330 s: 4958 frames, a median of 90 ms, and every line adds up. The looking thread's longest gap was 3.7 ms. Of 61 slow frames, 57 (93%) have under 10% unknown time. The other 4 are in menus and loading screens at 30 fps. In the world (from the first cutscene on) it is 37 of 37. Counting the correlate buckets `recording` and `dstorage` as unknown too, it is 24 of 37. In the 13 others, `frame_wait_title_ms` is about as large as `recording`, so the presenting thread waited for a thread of the title's while list calls ran elsewhere.

**The first cutscene starts** with one frame of 5839 ms:

| bucket | ms |
|---|---|
| recording | 3636 |
| preparing | 1810 |
| guards | 104 |
| gpu_wait + copy_back | 166 |
| title_running | 41 |
| title_wait + unexplained | 46 |

In that frame, `frame_wait_title_ms` is 3676 (fence 0, timer 0, other 0), so the presenting thread waits for threads of the title's, while threads are in list calls: 125447 list calls, 17128 ms summed over threads, or 136 µs per call against 4.2 µs in the world. Beside them are 1.71 M page faults, of which only 1217 are guard faults, and 11.7 s of process CPU time. DirectStorage is not the cause. Reads were pending for 46 ms of the frame's wall time (`ds_pending_ms`). Its 4126 ms of `ds_queue_ms` is a sum over 618 overlapping reads.

**Arriving in the world** (the next 10 s) runs at 8.5 fps, at 110 ms of `preparing`, 55 of `recording`, 11 of `guards` and 10 of `gpu_wait` per frame. Its drops have `preparing` and `guards` dominant, with DirectStorage bursts (`ds_bytes` 45-210 MB a frame) and queue map calls beside them.

**Later in the world** frames are steady at 9.6-11 fps: `preparing` 42-62 ms, `gpu_wait` 13-45, `copy_back` 4-10, `guards` 3-9. The presenting thread hardly waits for the title (`frame_wait_*` near 0).

The frame log cost nothing measurable (measured before the copy columns `up_*`/`back_*`, which add a lock read per copy). Two runs with the same settings, one with it and one without, gave 10.29 and 10.40 fps in the world (−1%). The same windows vary by ±10% from run to run.
