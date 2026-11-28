"""Rota panel: live dashboard that follows the newest run csv and its log.

Run: python -I tools/rota-profiler/panel.py [--file <csv>]
Check: python -I tools/rota-profiler/panel.py --selftest <csv>
Prepare: python -I tools/rota-profiler/panel.py --prepare <shaders or warm log>  (check: --selftest-prepare)
"""
import argparse
import csv
import glob
import io
import os
import re
import sys
import time

LOGS = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", "local", "logs")
WINDOW_S = 180
MAX_LINES = 500


def rgb(r, g, b):
    return "#%02x%02x%02x" % (r, g, b)


BG, DIM, DIMMER = rgb(12, 13, 16), rgb(140, 146, 158), rgb(104, 110, 122)
WHITE, RED, GREEN, YELLOW = rgb(240, 242, 246), rgb(255, 98, 88), rgb(96, 220, 130), rgb(250, 204, 70)
CYAN, BLUE = rgb(80, 200, 230), rgb(120, 150, 255)
CARD = rgb(20, 22, 27)

KEEP_WHITE = ("all made", "library", "cutscene", "named by the submissions")
PREFIX = re.compile(r"^\W*\d{4}-\d\d-\d\dT[\d:.]+Z\s+")


def newest_csv(after=0.0):
    files = [f for f in glob.glob(os.path.join(LOGS, "run-*.csv")) if not f.endswith(".frames.csv")]
    files = [f for f in files if os.path.getmtime(f) >= after]
    return max(files, key=os.path.getmtime) if files else None


class Tail:
    """Reads new bytes from an offset; keeps a partial last line for the next call."""

    def __init__(self, path):
        self.path, self.off, self.rest = path, 0, b""

    def read(self):
        try:
            with open(self.path, "rb") as f:
                f.seek(self.off)
                data = f.read()
        except OSError:
            return []
        self.off += len(data)
        data = self.rest + data
        *lines, self.rest = data.split(b"\n")
        return [l.decode("utf-8", errors="replace").rstrip("\r") for l in lines]


def num(s):
    try:
        return float(s)
    except (TypeError, ValueError):
        return float("nan")  # empty or garbage cell: missing


class Csv:
    def __init__(self, path):
        self.tail, self.header, self.cols = Tail(path), None, {}

    def update(self):
        for line in self.tail.read():
            if not line.strip():
                continue
            row = next(csv.reader(io.StringIO(line)))
            if self.header is None or row[0] == "t_s":
                self.header = row
                self.cols = {k: [] for k in row}
                continue
            if len(row) < len(self.header):
                row += [""] * (len(self.header) - len(row))
            for k, v in zip(self.header, row):
                self.cols[k].append(num(v))

    def last(self, key):
        v = self.cols.get(key) or []
        return v[-1] if v else float("nan")


LEVEL_MODULE = re.compile(r"^\s*(?:INFO|WARN|ERROR|DEBUG)\s+[\w:]+:\s*")
ANSI = re.compile(r"\x1b\[[0-9;]*m")  # an interactive run logs in color


def short_line(line):
    # the level shows as the color, the module path only eats width
    return LEVEL_MODULE.sub("", PREFIX.sub("", ANSI.sub("", line)))


def top(vals, floor):
    ok = [v for v in vals if v == v]
    return max([floor] + [v * 1.1 for v in ok])


def line_color(line):
    low = ANSI.sub("", line).lower()
    if any(k in low for k in KEEP_WHITE):
        return "white"
    if " error " in low or low.startswith("error"):
        return "red"
    if " warn " in low or low.startswith("warn"):
        return "yellow"
    return "dim"


def fmt(v, spec="%.1f", unit=""):
    return "--" if v != v else (spec % v) + unit


def fps_color(v):
    if v != v:
        return DIM
    return RED if v < 5 else YELLOW if v < 10 else GREEN


def selftest(path):
    c = Csv(path)
    c.update()
    log = Tail(os.path.splitext(path)[0] + ".log")
    n_log = len(log.read())
    print("rows=%d last_fps=%s log_lines=%d" % (len(c.cols.get("t_s", [])), fmt(c.last("fps"), "%.2f"), n_log))


def run_ui(path_arg):
    import tkinter as tk
    from tkinter import font as tkfont
    import matplotlib
    matplotlib.use("TkAgg")
    from matplotlib.figure import Figure
    from matplotlib.backends.backend_tkagg import FigureCanvasTkAgg

    started = time.time()
    root = tk.Tk()
    root.geometry("980x560")
    root.configure(bg=BG)
    # the run's name is in the title from the start: a launcher closes its own panel by it
    root.title("Rota panel — " + (os.path.basename(path_arg)[:57] if path_arg else "waiting for a run"))
    st = {"csv": None, "log": None, "path": None}

    head = tk.Frame(root, bg=BG)
    head.place(x=0, y=0, relwidth=1, height=26)
    tk.Label(head, text="ROTA", bg=BG, fg=WHITE, font=tkfont.Font(family="Consolas", size=11, weight="bold")).pack(side="left", padx=(8, 6))
    run_name = tk.Label(head, text="waiting for a run...", bg=BG, fg=DIMMER, font=tkfont.Font(family="Consolas", size=8))
    run_name.pack(side="left")
    status = tk.Label(head, text="● idle", bg=BG, fg=DIMMER, font=tkfont.Font(family="Consolas", size=8))
    status.pack(side="right", padx=8)

    left = tk.Frame(root, bg=CARD)
    left.place(relx=0, y=26, relwidth=0.4, relheight=1, height=-26)
    mono = tkfont.Font(family="Consolas", size=8)
    txt = tk.Text(left, bg=CARD, fg=DIM, font=mono, wrap="none", borderwidth=0, highlightthickness=0, state="disabled")
    txt.pack(fill="both", expand=True, padx=6, pady=4)
    for name, col in (("dim", DIMMER), ("white", WHITE), ("red", RED), ("yellow", YELLOW)):
        txt.tag_configure(name, foreground=col)

    right = tk.Frame(root, bg=BG)
    right.place(relx=0.4, y=26, relwidth=0.6, relheight=1, height=-26)
    strip = tk.Frame(right, bg=BG)
    strip.pack(fill="x", padx=4, pady=(0, 2))
    big = tkfont.Font(family="Consolas", size=14, weight="bold")
    small = tkfont.Font(family="Consolas", size=7)
    tiles = {}
    for key, label in (("fps", "fps"), ("ms", "frame ms"), ("pipelines_making", "pipelines"),
                       ("textures_waiting", "textures"), ("vram", "VRAM GB"), ("ram_process_gb", "RAM GB"),
                       ("gpu_pct", "GPU %")):
        f = tk.Frame(strip, bg=CARD)
        f.pack(side="left", expand=True, fill="x", padx=2)
        v = tk.Label(f, text="--", bg=CARD, fg=WHITE, font=big)
        v.pack(pady=(3, 0))
        tk.Label(f, text=label, bg=CARD, fg=DIMMER, font=small).pack(pady=(0, 3))
        tiles[key] = v

    fig = Figure(facecolor=BG)
    axes = fig.subplots(4, 1, sharex=True)
    ax3b = axes[2].twinx()
    for ax in list(axes) + [ax3b]:
        ax.set_facecolor(BG)
        ax.tick_params(colors=DIM, labelsize=6)
        for sp in ax.spines.values():
            sp.set_visible(False)
    for ax in axes:
        ax.grid(True, color=DIMMER, alpha=0.25, linewidth=0.5)
    FigureCanvasTkAgg(fig, master=right).get_tk_widget().pack(fill="both", expand=True)
    canvas = fig.canvas

    def add_text(lines):
        if not lines:
            return
        txt.configure(state="normal")
        for l in lines[-MAX_LINES:]:
            txt.insert("end", short_line(l) + "\n", line_color(l))
        extra = int(txt.index("end-1c").split(".")[0]) - MAX_LINES
        if extra > 0:
            txt.delete("1.0", "%d.0" % (extra + 1))
        txt.configure(state="disabled")
        txt.see("end")

    def switch(path):
        st["path"], st["csv"] = path, Csv(path)
        st["log"] = Tail(os.path.splitext(path)[0] + ".log")
        txt.configure(state="normal")
        txt.delete("1.0", "end")
        txt.configure(state="disabled")
        base = os.path.basename(path)
        root.title("Rota panel — " + (base if len(base) < 60 else base[:57] + "..."))
        run_name.configure(text=base[4:-4][:70])

    def draw():
        c = st["csv"]
        t = c.cols.get("t_s", [])
        if not t:
            return
        lo = t[-1] - WINDOW_S
        i = next((k for k, x in enumerate(t) if x >= lo), 0)
        xs = t[i:]

        def y(k):
            return c.cols.get(k, [])[i:]

        for ax in list(axes) + [ax3b]:
            ax.clear()
            ax.set_facecolor(BG)
            ax.tick_params(colors=DIM, labelsize=6)
            for sp in ax.spines.values():
                sp.set_visible(False)
        for ax in axes:
            ax.grid(True, color=DIMMER, alpha=0.25, linewidth=0.5)
        a1, a2, a3, a4 = axes
        a1.plot(xs, y("fps"), color=GREEN, lw=1.2)
        a1.fill_between(xs, y("fps"), color=GREEN, alpha=0.12, linewidth=0)
        a1.set_ylim(0, top(y("fps"), 32))
        a1.axhline(10, color=YELLOW, ls="--", lw=0.8, alpha=0.7)
        a1.axhline(30, color=DIM, ls="--", lw=0.8, alpha=0.7)
        a1.set_ylabel("fps", color=DIM, fontsize=7)
        for k, col in (("preparing_ms", CYAN), ("copy_ms", YELLOW), ("gpu_wait_ms", RED)):
            a2.plot(xs, y(k), color=col, lw=1, label=k)
        a2.legend(loc="upper left", fontsize=6, facecolor=BG, edgecolor="none", labelcolor=DIM)
        a2.set_ylabel("ms", color=DIM, fontsize=7)
        a3.plot(xs, y("pipelines_making"), color=CYAN, lw=1.2, label="pipelines")
        ax3b.plot(xs, y("textures_waiting"), color=BLUE, lw=1, label="textures")
        ax3b.tick_params(colors=DIM, labelsize=6)
        a3.set_ylim(0, top(y("pipelines_making"), 10))
        ax3b.set_ylim(0, top(y("textures_waiting"), 10))
        a3.set_ylabel("pipelines", color=CYAN, fontsize=7)
        ax3b.set_ylabel("textures", color=BLUE, fontsize=7)
        ax3b.yaxis.set_label_position("right")  # clear() puts the twin's label back on the left
        a4.plot(xs, y("vram_used_gb"), color=CYAN, lw=1.2, label="vram used")
        a4.plot(xs, y("vram_budget_gb"), color=DIM, ls="--", lw=0.9, label="vram budget")
        a4.set_ylim(0, top(y("vram_budget_gb") + y("vram_used_gb"), 1))  # RAM stays in its tile: it would flatten VRAM
        a4.legend(loc="lower left", fontsize=6, facecolor=BG, edgecolor="none", labelcolor=DIM, ncol=2)
        a4.set_ylabel("GB", color=DIM, fontsize=7)
        a4.set_xlabel("t_s", color=DIM, fontsize=7)
        fig.subplots_adjust(left=0.08, right=0.92, top=0.99, bottom=0.08, hspace=0.1)
        canvas.draw_idle()

    def tick():
        try:
            if not path_arg:  # follow the newest run, switch when a newer one shows up
                newest = newest_csv(started)
                if newest and newest != st["path"]:
                    switch(newest)
            elif st["path"] is None and os.path.exists(path_arg):
                switch(path_arg)
            if st["csv"]:
                st["csv"].update()
                add_text(st["log"].read())
                c = st["csv"]
                live = time.time() - os.path.getmtime(st["path"]) < 3  # the game writes a row each second
                status.configure(text="● live" if live else "● idle", fg=GREEN if live else DIMMER)
                fps = c.last("fps")
                tiles["fps"].configure(text=fmt(fps), fg=fps_color(fps))
                tiles["ms"].configure(text=fmt(c.last("ms")))
                tiles["pipelines_making"].configure(text=fmt(c.last("pipelines_making"), "%.0f"))
                tiles["textures_waiting"].configure(text=fmt(c.last("textures_waiting"), "%.0f"))
                tiles["vram"].configure(text="%s/%s" % (fmt(c.last("vram_used_gb")), fmt(c.last("vram_budget_gb"))))
                tiles["ram_process_gb"].configure(text=fmt(c.last("ram_process_gb")))
                tiles["gpu_pct"].configure(text=fmt(c.last("gpu_pct"), "%.0f"))
                draw()
        except Exception as e:  # keep the panel alive on a bad row or a file in flux
            print("panel: %s" % e, file=sys.stderr)
        root.after(1000, tick)

    tick()
    root.mainloop()


STEPS = {"shaders": "Translating shaders", "make": "Making pipelines (driver)", "library": "Pipelines into the library"}
FIRST_TIME = ("First time: no pipeline list yet. The first run will look rough "
              "(black screen, pipelines being made); close it and preparing runs again.")
# Formats come from apps/victory/src/warm.rs and translate.rs (precompile).
RE_KNOWN = re.compile(r"^(\d+) pipelines known, .*; (\d+) to make")
RE_OF = re.compile(r"^(\d+) of (\d+), (\d+) s$")
RE_MADE = re.compile(r"^(\d+) made in (\d+) s")
RE_LIB = re.compile(r"^(\d+) pipelines into the runs' pipeline library")
RE_LIB_END = re.compile(r"^(\d+) through the library in (\d+) s")
RE_SH = re.compile(r"^(\d+) shaders in the cache, (\d+) s$")
RE_SH_END = re.compile(r"^(\d+) shaders in the cache, \d+ do not translate, (\d+) s")


class Prep:
    """Progress of a shaders or warm log. total is None when the log has none (shaders)."""

    def __init__(self):
        self.step, self.done, self.total, self.secs = None, 0, None, 0.0
        self.stamp = time.time()  # when the last progress line was read
        self.step_done, self.finished, self.first_time = False, False, False
        self.lines = []

    def feed(self, raw):
        line = short_line(raw).strip()
        if not line:
            return
        self.lines.append(line)
        del self.lines[:-6]
        m = RE_KNOWN.match(line)
        if m:
            self._start("make", int(m[2]))
            self.first_time = int(m[1]) == 0
            return
        m = RE_LIB.match(line)
        if m:
            self._start("library", int(m[1]))
            return
        for rx, step, end, fin in ((RE_OF, None, False, False), (RE_MADE, "make", True, False),
                                   (RE_LIB_END, "library", True, True), (RE_SH, "shaders", False, False),
                                   (RE_SH_END, "shaders", True, True)):
            m = rx.match(line)
            if not m:
                continue
            if step and self.step != step:
                self._start(step, None)
            if rx is RE_OF:
                self.done, self.total, self.secs = int(m[1]), int(m[2]), float(m[3])
            elif rx is RE_SH:
                self.done, self.secs = int(m[1]), float(m[2])
            else:
                self.done, self.secs = int(m[1]), float(m[m.lastindex])
                self.total = self.done if self.total is None else self.total
                self.step_done, self.finished = True, fin
            self.stamp = time.time()
            return

    def _start(self, step, total):
        self.step, self.done, self.total, self.secs = step, 0, total, 0.0
        self.step_done, self.stamp = False, time.time()

    def eta(self, now):
        if not self.total or not self.done or not self.secs or self.step_done:
            return None
        return max(0.0, (self.total - self.done) / (self.done / self.secs) - (now - self.stamp))


def hms(s):
    s = int(s)
    return "%d:%02d:%02d" % (s // 3600, s % 3600 // 60, s % 60) if s >= 3600 else "%d:%02d" % (s // 60, s % 60)


def selftest_prepare():
    p = Prep()
    for l in ("88579 pipelines known, 0 made before (abc, card), 0 bad; 5064 to make, 10 processes",
              "\x1b[32m1000 of 5064, 8 s\x1b[0m"):
        p.feed(l)
    assert (p.step, p.done, p.total, p.finished, p.first_time) == ("make", 1000, 5064, False, False)
    assert p.eta(p.stamp) is not None and abs(p.eta(p.stamp) - 4064 / (1000 / 8)) < 1e-6
    p.feed("5064 made in 802 s (0 have no host pipeline, 0 ended their process); ms each: median 1.0")
    assert (p.done, p.total, p.step_done, p.finished) == (5064, 5064, True, False)
    p.feed("5064 pipelines into the runs' pipeline library, one process at a time")
    assert (p.step, p.done, p.total, p.step_done) == ("library", 0, 5064, False)
    p.feed("1000 of 5064, 8 s")
    assert (p.step, p.done, p.total) == ("library", 1000, 5064)
    p.feed("5064 through the library in 52 s: 1 loaded, 2 made and stored, 3 with static samplers made only; 0 ended their process")
    assert (p.done, p.total, p.step_done, p.finished) == (5064, 5064, True, True)
    p = Prep()
    p.feed("0 pipelines known, 0 made before (x, y), 0 bad; 0 to make, 4 processes")
    assert p.first_time and p.total == 0
    p = Prep()
    p.feed("2000 shaders in the cache, 31 s")
    assert (p.step, p.done, p.total, p.finished) == ("shaders", 2000, None, False)
    p.feed("2100 shaders in the cache, 3 do not translate, 40 s")
    assert (p.done, p.step_done, p.finished) == (2100, True, True)
    print("selftest-prepare ok")


def run_prepare(path):
    import tkinter as tk
    from tkinter import font as tkfont

    root = tk.Tk()
    root.geometry("720x300")
    root.configure(bg=BG)
    # the log's name is in the title: a launcher closes this window by it
    root.title("Rota — preparing — " + os.path.basename(path)[:50])
    mono = lambda size, bold=False: tkfont.Font(family="Consolas", size=size, weight="bold" if bold else "normal")
    tk.Label(root, text="ROTA", bg=BG, fg=WHITE, font=mono(11, True)).place(x=12, y=6)
    step_l = tk.Label(root, text="waiting for the log...", bg=BG, fg=WHITE, font=mono(14, True), anchor="w")
    step_l.place(x=12, y=34, width=696)
    bar = tk.Canvas(root, bg=CARD, height=22, highlightthickness=0)
    bar.place(x=12, y=72, width=696)
    info = tk.Label(root, text="", bg=BG, fg=DIM, font=mono(10), anchor="w")
    info.place(x=12, y=102, width=696)
    note = tk.Label(root, text="", bg=BG, fg=YELLOW, font=mono(8), anchor="w", justify="left", wraplength=696)
    note.place(x=12, y=126, width=696, height=30)
    logbox = tk.Label(root, text="", bg=CARD, fg=DIMMER, font=mono(8), anchor="nw", justify="left")
    logbox.place(x=12, y=160, width=696, height=128)

    p, tail, st = Prep(), Tail(path), {"quiet": time.time()}

    def tick():
        try:
            new = tail.read()
            for l in new:
                p.feed(l)
            if new:
                st["quiet"] = time.time()
            now = time.time()
            ok = p.step_done
            col = GREEN if ok else CYAN
            if p.step:
                step_l.configure(text=STEPS[p.step] + (" - done" if ok else ""), fg=GREEN if ok else WHITE)
            frac = 1.0 if ok else (p.done / p.total if p.total else 0.0)
            w = bar.winfo_width()
            bar.delete("all")
            if p.step and not ok and not p.total:  # no total (shaders): a sliding block
                x = int(now * 120) % (w + 80) - 80
                bar.create_rectangle(max(0, x), 0, min(w, x + 80), 22, fill=col, width=0)
            else:
                bar.create_rectangle(0, 0, int(w * frac), 22, fill=col, width=0)
            eta = p.eta(now)
            elapsed = p.secs if ok else p.secs + (now - p.stamp if p.step else 0)
            count = "%d of %d" % (p.done, p.total) if p.total else "%d" % p.done
            info.configure(text="%s    elapsed %s    ETA %s" % (count, hms(elapsed), hms(eta) if eta is not None else "--") if p.step else "")
            note.configure(text=FIRST_TIME if p.first_time else "")
            logbox.configure(text="\n".join(p.lines))
            if p.finished and now - st["quiet"] > 20:  # final line seen and the log went quiet
                root.destroy()
                return
        except Exception as e:  # keep the window alive on a file in flux
            print("panel: %s" % e, file=sys.stderr)
        root.after(250, tick)

    tick()
    root.mainloop()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--file")
    ap.add_argument("--selftest")
    ap.add_argument("--prepare", help="follow a shaders or warm log")
    ap.add_argument("--selftest-prepare", action="store_true")
    a = ap.parse_args()
    if a.selftest_prepare:
        selftest_prepare()
    elif a.prepare:
        run_prepare(a.prepare)
    elif a.selftest:
        selftest(a.selftest)
    else:
        run_ui(a.file)


if __name__ == "__main__":
    main()
