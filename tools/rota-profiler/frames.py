"""Explains every frame of a run from its frame log (VICTORY_FRAME_LOG).

usage:
  python -I frames.py <frames.csv>                  drops, peaks, windows, check
  python -I frames.py <frames.csv> --window 10      window length in seconds
  python -I frames.py --compare <a.csv> <b.csv>     two runs side by side
  python -I frames.py --self-test                   checks on a made-up log

The CSV holds numbers only, and so does everything this prints.
"""
import argparse
import csv
import io
import statistics
import sys

BUCKETS = [
    'present', 'pace', 'preparing', 'submit', 'gpu_wait', 'copy_back', 'guards', 'xmem_title', 'xmem_queue',
    'pipelines', 'recording', 'title_running', 'title_wait', 'fence_wait', 'unexplained',
]
# Time nothing of ours explains: a wait of the title's with nothing of ours going on is no reason.
UNKNOWN = ['unexplained', 'title_wait', 'fence_wait']
# Counters worth naming beside a drop, per frame.
COUNTERS = [
    'xmem_title_calls', 'xmem_title_pages', 'xmem_title_ms', 'xmem_queue_calls', 'xmem_queue_pages', 'xmem_queue_ms',
    'title_waits', 'title_wait_ms', 'list_calls', 'submissions', 'tex_uploads', 'pipelines_asked', 'pipelines_made',
    'pipelines_loaded', 'pipeline_wait_ms', 'dropped', 'dropped_rt_off', 'ds_reads', 'ds_bytes', 'ds_queue_ms',
    'guard_waits', 'guard_wait_ms', 'guard_faults', 'page_faults', 'process_cpu_ms',
]
SLOW, FAST, AROUND, LEAST_MS = 1.5, 0.67, 5.0, 40.0


def read(path_or_file):
    f = open(path_or_file, newline='') if isinstance(path_or_file, str) else path_or_file
    rows = []
    for raw in csv.DictReader(f):
        row = {}
        for k, v in raw.items():
            try:
                row[k] = float(v) if v not in ('', None) else None
            except ValueError:
                row[k] = None
        rows.append(row)
    # The first line covers the time before the first present seen.
    return rows[1:]


def check(rows, tolerance=0.05):
    """Lines whose buckets do not add up to their wall time (ms)."""
    bad = []
    for r in rows:
        total = sum(r[b] or 0 for b in BUCKETS)
        if abs(total - r['wall_ms']) > tolerance + 0.01 * len(BUCKETS):
            bad.append((r['frame'], r['wall_ms'], total))
    return bad


def unknown(r):
    return sum(r[b] or 0 for b in UNKNOWN)


def around(rows, i, seconds=AROUND):
    """The median wall time of the frames within `seconds` of frame i (not i)."""
    t = rows[i]['t_s']
    lo, hi = i, i
    while lo > 0 and t is not None and rows[lo - 1]['t_s'] is not None and t - rows[lo - 1]['t_s'] <= seconds:
        lo -= 1
    while hi + 1 < len(rows) and t is not None and rows[hi + 1]['t_s'] is not None and rows[hi + 1]['t_s'] - t <= seconds:
        hi += 1
    walls = [rows[j]['wall_ms'] for j in range(lo, hi + 1) if j != i]
    return statistics.median(walls) if walls else rows[i]['wall_ms']


def episodes(rows, slow=True):
    """Runs of consecutive slow (or fast) frames: lists of indices."""
    marks = []
    for i, r in enumerate(rows):
        base = around(rows, i)
        if slow:
            marks.append(r['wall_ms'] >= SLOW * base and r['wall_ms'] >= LEAST_MS)
        else:
            marks.append(r['wall_ms'] <= FAST * base)
    runs, run = [], []
    for i, m in enumerate(marks):
        if m:
            run.append(i)
        elif run:
            runs.append(run)
            run = []
    if run:
        runs.append(run)
    return runs


def sums(rows, idx, keys):
    return {k: sum(rows[i].get(k) or 0 for i in idx) for k in keys}


def dominant(total):
    order = sorted(BUCKETS, key=lambda b: -total[b])
    return order[0], order[1]


def per_frame_medians(rows):
    return {c: statistics.median([r.get(c) or 0 for r in rows]) for c in COUNTERS if c in rows[0]}


def describe(rows, idx, medians):
    total = sums(rows, idx, BUCKETS + ['wall_ms'])
    wall = total['wall_ms'] or 1
    first, second = dominant(total)
    counts = sums(rows, idx, COUNTERS)
    n = len(idx)
    # Counters furthest above their run median, per frame.
    ratios = []
    for c in COUNTERS:
        if c not in medians:
            continue
        per = counts[c] / n
        base = medians[c]
        if per > 0 and (base == 0 or per / base >= 2):
            ratios.append((per / base if base else float('inf'), c, per, base))
    ratios.sort(reverse=True)
    t0, t1 = rows[idx[0]]['t_s'], rows[idx[-1]]['t_s']
    text = (f"t {t0:.1f}-{t1:.1f} s, {n} frames, {wall / n:.0f} ms/frame: "
            f"{first} {100 * total[first] / wall:.0f}%, {second} {100 * total[second] / wall:.0f}%, "
            f"unexplained {100 * total['unexplained'] / wall:.1f}%, unknown {100 * sum(total[b] for b in UNKNOWN) / wall:.1f}%")
    if ratios:
        text += '; above the run: ' + ', '.join(f"{c} {per:.1f}/frame (median {base:.1f})" for _, c, per, base in ratios[:4])
    return text


def windows(rows, seconds):
    out = []
    start = rows[0]['t_s'] or 0
    group = []
    for r in rows:
        if r['t_s'] is None:
            continue
        if r['t_s'] - start >= seconds and group:
            out.append(group)
            group, start = [], r['t_s']
        group.append(r)
    if group:
        out.append(group)
    return out


def report(path, seconds):
    rows = read(path)
    if not rows:
        print('no frames')
        return 1
    bad = check(rows)
    walls = [r['wall_ms'] for r in rows]
    print(f"{len(rows)} frames, median {statistics.median(walls):.1f} ms ({1000 / statistics.median(walls):.2f} fps), "
          f"lines that do not add up: {len(bad)}")
    total = sums(rows, range(len(rows)), BUCKETS + ['wall_ms'])
    print('where the time went (share of the run): ' + ', '.join(
        f"{b} {100 * total[b] / total['wall_ms']:.1f}%" for b in sorted(BUCKETS, key=lambda b: -total[b]) if total[b] > 0))
    gaps = [r['tick_max_ms'] for r in rows if r.get('tick_max_ms') is not None]
    if gaps:
        print(f"looking thread: longest gap between looks {max(gaps):.1f} ms, median per frame {statistics.median(gaps):.1f} ms")
    medians = per_frame_medians(rows)
    drops = episodes(rows, slow=True)
    slow = [i for run in drops for i in run]
    if slow:
        good = sum(1 for i in slow if unknown(rows[i]) < 0.10 * rows[i]['wall_ms'])
        print(f"slow frames (>= {SLOW}x the median within {AROUND:.0f} s, >= {LEAST_MS:.0f} ms): {len(slow)}; "
              f"unknown (unexplained + title wait + fence wait) under 10%: {good} ({100 * good / len(slow):.0f}%)")
    print(f"\ndrops ({len(drops)}), longest first:")
    for run in sorted(drops, key=lambda run: -sum(rows[i]['wall_ms'] for i in run))[:20]:
        print('  ' + describe(rows, run, medians))
    peaks = episodes(rows, slow=False)
    print(f"\npeaks ({len(peaks)}: frames <= {FAST}x the median around them), longest first:")
    for run in sorted(peaks, key=len, reverse=True)[:10]:
        print('  ' + describe(rows, run, medians))
    print(f"\nwindows of {seconds:.0f} s (median fps; ms/frame by bucket, the four largest; unknown %):")
    for group in windows(rows, seconds):
        idx = range(len(group))
        tot = sums(group, idx, BUCKETS + ['wall_ms'])
        n = len(group)
        top = sorted(BUCKETS, key=lambda b: -tot[b])[:4]
        fps = 1000 / statistics.median([r['wall_ms'] for r in group])
        print(f"  t {group[0]['t_s']:.0f}-{group[-1]['t_s']:.0f}: {fps:.1f} fps; "
              + ', '.join(f"{b} {tot[b] / n:.0f}" for b in top)
              + f"; unknown {100 * sum(tot[b] for b in UNKNOWN) / tot['wall_ms']:.1f}%")
    return 0


def compare(a, b):
    ra, rb = read(a), read(b)
    if not ra or not rb:
        print('a run has no frames')
        return 1
    print(f"{'':22}{'A':>10}{'B':>10}{'B-A':>10}")
    ma, mb = statistics.median([r['wall_ms'] for r in ra]), statistics.median([r['wall_ms'] for r in rb])
    print(f"{'frames':22}{len(ra):>10}{len(rb):>10}{len(rb) - len(ra):>10}")
    print(f"{'median fps':22}{1000 / ma:>10.2f}{1000 / mb:>10.2f}{1000 / mb - 1000 / ma:>10.2f}")
    print('ms per frame (mean):')
    for k in ['wall_ms'] + BUCKETS + ['pre_list_ms', 'xmem_active_ms']:
        va = sum(r.get(k) or 0 for r in ra) / len(ra)
        vb = sum(r.get(k) or 0 for r in rb) / len(rb)
        print(f"  {k:20}{va:>10.2f}{vb:>10.2f}{vb - va:>10.2f}")
    print('counts per frame (mean):')
    for k in COUNTERS:
        if k not in ra[0] or k not in rb[0]:
            continue
        va = sum(r.get(k) or 0 for r in ra) / len(ra)
        vb = sum(r.get(k) or 0 for r in rb) / len(rb)
        print(f"  {k:20}{va:>10.1f}{vb:>10.1f}{vb - va:>10.1f}")
    return 0


def made_up():
    """A log: 100 frames of 100 ms at 10 fps, a drop of 3 frames of 400 ms
    that xmem explains, one frame of 300 ms nothing explains, a peak."""
    head = ['frame', 't_s', 'wall_ms', 'presents'] + BUCKETS + ['pre_list_ms', 'xmem_active_ms', 'title_cpu_ms'] \
        + COUNTERS + ['ticks', 'tick_max_ms']
    out = io.StringIO()
    w = csv.writer(out)
    w.writerow(head)
    t = 0.0
    for i in range(120):
        row = dict.fromkeys(head, 0)
        wall = 100.0
        b = {'preparing': 30.0, 'gpu_wait': 30.0, 'title_running': 35.0, 'unexplained': 5.0}
        if 50 <= i < 53:
            wall = 400.0
            b = {'preparing': 30.0, 'xmem_title': 340.0, 'title_running': 25.0, 'unexplained': 5.0}
            row['xmem_title_calls'] = 900
        if i == 80:
            wall = 300.0
            b = {'preparing': 30.0, 'gpu_wait': 30.0, 'unexplained': 240.0}
        if i == 100:
            wall = 50.0
            b = {'preparing': 20.0, 'gpu_wait': 20.0, 'title_running': 10.0}
        t += wall / 1000
        row.update(b)
        row.update({'frame': i + 1, 't_s': round(t, 3), 'wall_ms': wall, 'presents': 1, 'xmem_title_calls': row['xmem_title_calls'] or 10})
        w.writerow([row[h] for h in head])
    out.seek(0)
    return out


def self_test():
    rows = read(made_up())
    assert check(rows) == [], check(rows)
    drops = episodes(rows, slow=True)
    assert len(drops) == 2, drops
    first = [d for d in drops if len(d) == 3][0]
    total = sums(rows, first, BUCKETS)
    assert dominant(total)[0] == 'xmem_title', total
    lone = [d for d in drops if len(d) == 1][0]
    assert unknown(rows[lone[0]]) > 0.5 * rows[lone[0]]['wall_ms']
    peaks = episodes(rows, slow=False)
    assert len(peaks) == 1 and rows[peaks[0][0]]['wall_ms'] == 50.0, peaks
    text = describe(rows, first, per_frame_medians(rows))
    assert 'xmem_title' in text and 'xmem_title_calls' in text, text
    # A line that does not add up is found.
    rows[10]['unexplained'] += 5
    assert len(check(rows)) == 1
    print('self-test ok')
    return 0


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument('log', nargs='?')
    p.add_argument('--window', type=float, default=10.0)
    p.add_argument('--compare', nargs=2, metavar=('A', 'B'))
    p.add_argument('--self-test', action='store_true')
    a = p.parse_args()
    if a.self_test:
        return self_test()
    if a.compare:
        return compare(*a.compare)
    if not a.log:
        p.print_help()
        return 2
    return report(a.log, a.window)


if __name__ == '__main__':
    sys.exit(main())
