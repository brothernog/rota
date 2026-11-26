//! Performance overlay: a small translucent panel (per-pixel alpha through `UpdateLayeredWindow`)
//! in a click-through, topmost tool window at the game window's top-left corner, drawn on a thread
//! of its own twice a second. It does no GPU work and never touches the swap chain.
//!
//! Adapted from the Neptune overlay (dudegio/Neptune-GDeflate) as carried by
//! soldsawn/victory-nvidia-8gb (commit 7c55a8c1), both GPL-3.0; this version adds the present
//! meter's breakdown (preparing, copies, textures, copy-back), the pipeline and texture backlogs,
//! the PC memory shared with the card, and a numbers-only CSV.
//!
//! `VICTORY_OVERLAY=0`: off. F10 shows and hides the panel while a window of this process is in
//! front. `VICTORY_OVERLAY_CSV=<file>`: one row per meter tick (about a second), numbers only,
//! written whether the panel shows or not. Not for exclusive fullscreen (the swap chain is
//! windowed, flip model, so a topmost window above it is composed as usual).

use std::collections::VecDeque;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, Once};

use windows::Win32::Graphics::Dxgi::{
    DXGI_MEMORY_SEGMENT_GROUP_LOCAL, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, DXGI_QUERY_VIDEO_MEMORY_INFO, IDXGIAdapter3,
};
use windows_sys::Win32::Foundation::{FILETIME, HWND, POINT, RECT, SIZE};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetSystemTimes};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::pixels::{Bitmap, short_cpu, short_gpu};

/// One tick of the present path's meter, every value per frame over about a second.
#[derive(Clone, Copy, Default)]
pub(crate) struct Frame {
    pub fps: f64,
    pub ms: f64,
    pub draws: f64,
    /// Preparing submissions on the CPU, ms, and of it copies and textures (ms, MB).
    pub preparing: f64,
    pub copy_ms: f64,
    pub copy_mb: f64,
    pub texture_ms: f64,
    pub texture_mb: f64,
    /// Waiting for the GPU, ms.
    pub gpu_wait: f64,
    /// Copying back to title memory (ms, MB).
    pub back_ms: f64,
    pub back_mb: f64,
    /// Draws dropped for want of a host pipeline, and the pipelines being made.
    pub dropped: f64,
    pub making: u64,
    /// Textures picked by index: waiting, and MB uploaded so far.
    pub textures_waiting: u64,
    pub textures_mb: u64,
}

static FRAME: Mutex<Option<Frame>> = Mutex::new(None);
/// Meter ticks so far (the CSV writes a row per tick).
static TICKS: AtomicU64 = AtomicU64::new(0);
/// The meter's last thirty frame rates (one a tick).
static RATES: Mutex<VecDeque<f64>> = Mutex::new(VecDeque::new());
static VISIBLE: AtomicBool = AtomicBool::new(true);

const VK_F10: i32 = 0x79;
const REFRESH: usize = 1;
const KEYS: usize = 2;

/// Called once a second by the present path's meter.
pub(crate) fn set_frame(frame: Frame) {
    *FRAME.lock().unwrap_or_else(|e| e.into_inner()) = Some(frame);
    let mut rates = RATES.lock().unwrap_or_else(|e| e.into_inner());
    rates.push_back(frame.fps);
    while rates.len() > 30 {
        rates.pop_front();
    }
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// The lowest, mean and highest of `rates` (none before there are two).
fn spread(rates: &VecDeque<f64>) -> Option<(f64, f64, f64)> {
    (rates.len() >= 2).then(|| {
        let (low, high) = rates.iter().fold((f64::MAX, f64::MIN), |(l, h), &r| (l.min(r), h.max(r)));
        (low, rates.iter().sum::<f64>() / rates.len() as f64, high)
    })
}

/// What the panel and the CSV show.
#[derive(Clone, Default)]
struct Stats {
    frame: Frame,
    half_minute: Option<(f64, f64, f64)>,
    cpu: f64,
    gpu: Option<f64>,
    /// This process's working set and commit, system memory used and total, in GB.
    ram: [f64; 4],
    /// The card's memory budget and use, then the PC's memory shared with the card: budget and use, in GB.
    vram: Option<[f64; 4]>,
    cpu_name: String,
    gpu_name: String,
}

/// Starts the overlay thread for the game window `hwnd` (the first call only).
pub(crate) fn start(hwnd: windows::Win32::Foundation::HWND, adapter: Option<IDXGIAdapter3>) {
    static ONCE: Once = Once::new();
    if std::env::var_os("VICTORY_OVERLAY").is_some_and(|v| v == "0") {
        return;
    }
    let game = hwnd.0 as usize;
    ONCE.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("victory-overlay".into())
            .spawn(move || run(game as HWND, adapter));
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "overlay thread not started");
        }
    });
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}

fn ft(t: FILETIME) -> u64 {
    u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)
}

/// System CPU load since the last call, in percent.
struct Cpu(u64, u64);
impl Cpu {
    fn read() -> (u64, u64) {
        let z = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
        let (mut idle, mut kernel, mut user) = (z, z, z);
        // SAFETY: plain out-parameters.
        unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) };
        // Kernel time includes idle.
        (ft(idle), ft(kernel) + ft(user))
    }
    fn percent(&mut self) -> f64 {
        let (idle, total) = Cpu::read();
        let (di, dt) = (idle.saturating_sub(self.0), total.saturating_sub(self.1));
        *self = Cpu(idle, total);
        if dt == 0 { 0.0 } else { (dt - di.min(dt)) as f64 * 100.0 / dt as f64 }
    }
}

/// The 3D engines' load summed over the instances (PDH, English names so any Windows language works).
struct Gpu3d(*mut std::ffi::c_void, *mut std::ffi::c_void);
impl Gpu3d {
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Performance::*;
        let (mut query, mut counter) = (std::ptr::null_mut(), std::ptr::null_mut());
        let path = wide(r"\GPU Engine(*engtype_3D)\Utilization Percentage");
        // SAFETY: out handles and a NUL-terminated path.
        unsafe {
            if PdhOpenQueryW(std::ptr::null(), 0, &mut query) != 0 {
                return None;
            }
            if PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) != 0 {
                PdhCloseQuery(query);
                return None;
            }
            PdhCollectQueryData(query);
        }
        Some(Gpu3d(query, counter))
    }
    fn percent(&self) -> Option<f64> {
        use windows_sys::Win32::System::Performance::*;
        // SAFETY: the sizing call, then a buffer of the size it named (8-aligned).
        unsafe {
            if PdhCollectQueryData(self.0) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            PdhGetFormattedCounterArrayW(self.1, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut());
            if size == 0 {
                return None;
            }
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>();
            if PdhGetFormattedCounterArrayW(self.1, PDH_FMT_DOUBLE, &mut size, &mut count, items) != 0 {
                return None;
            }
            let sum = (0..count as usize)
                .map(|i| &*items.add(i))
                .filter(|item| item.FmtValue.CStatus == 0 || item.FmtValue.CStatus == 1)
                .map(|item| item.FmtValue.Anonymous.doubleValue)
                .sum::<f64>();
            Some(sum.clamp(0.0, 100.0))
        }
    }
}

fn cpu_name() -> String {
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    let key = wide(r"HARDWARE\DESCRIPTION\System\CentralProcessor\0");
    let value = wide("ProcessorNameString");
    let mut buf = [0u16; 128];
    let mut bytes = std::mem::size_of_val(&buf) as u32;
    // SAFETY: a buffer of the size passed.
    let rc = unsafe {
        RegGetValueW(HKEY_LOCAL_MACHINE, key.as_ptr(), value.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
    };
    if rc != 0 {
        return "CPU".into();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len]).trim().to_string()
}

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

fn sample(cpu: &mut Cpu, gpu: Option<&Gpu3d>, adapter: Option<&IDXGIAdapter3>, names: &(String, String)) -> Stats {
    // SAFETY: plain structs filled by the calls.
    let ram = unsafe {
        let mut pm = std::mem::zeroed::<PROCESS_MEMORY_COUNTERS_EX>();
        pm.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        GetProcessMemoryInfo(GetCurrentProcess(), std::ptr::from_mut(&mut pm).cast(), pm.cb);
        let mut ms = std::mem::zeroed::<MEMORYSTATUSEX>();
        ms.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        GlobalMemoryStatusEx(&mut ms);
        [pm.WorkingSetSize as f64, pm.PrivateUsage as f64, (ms.ullTotalPhys - ms.ullAvailPhys) as f64, ms.ullTotalPhys as f64]
            .map(|b| b / GB)
    };
    let vram = adapter.and_then(|a| {
        let (mut own, mut shared) = (DXGI_QUERY_VIDEO_MEMORY_INFO::default(), DXGI_QUERY_VIDEO_MEMORY_INFO::default());
        // SAFETY: plain queries filling our structs.
        unsafe {
            a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own).ok()?;
            a.QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared).ok()?;
        }
        Some([own.Budget, own.CurrentUsage, shared.Budget, shared.CurrentUsage].map(|b| b as f64 / GB))
    });
    Stats {
        frame: FRAME.lock().unwrap_or_else(|e| e.into_inner()).unwrap_or_default(),
        half_minute: spread(&RATES.lock().unwrap_or_else(|e| e.into_inner())),
        cpu: cpu.percent(),
        gpu: gpu.and_then(Gpu3d::percent),
        ram,
        vram,
        cpu_name: names.0.clone(),
        gpu_name: names.1.clone(),
    }
}

const CSV_HEADER: &str = "t_s,fps,ms,preparing_ms,copy_ms,copy_mb,texture_ms,texture_mb,gpu_wait_ms,back_ms,back_mb,\
draws,dropped,pipelines_making,textures_waiting,textures_up_mb,cpu_pct,gpu_pct,ram_process_gb,commit_gb,\
ram_used_gb,ram_total_gb,vram_budget_gb,vram_used_gb,shared_budget_gb,shared_used_gb";

/// One CSV row: numbers only (empty where the system gave none).
fn csv_row(t: f64, s: &Stats) -> String {
    let f = &s.frame;
    let v = s.vram.map_or([String::new(), String::new(), String::new(), String::new()], |v| v.map(|x| format!("{x:.3}")));
    format!(
        "{t:.1},{:.2},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.2},{:.0},{:.0},{},{},{},{:.0},{},{:.3},{:.3},{:.3},{:.3},{},{},{},{}",
        f.fps,
        f.ms,
        f.preparing,
        f.copy_ms,
        f.copy_mb,
        f.texture_ms,
        f.texture_mb,
        f.gpu_wait,
        f.back_ms,
        f.back_mb,
        f.draws,
        f.dropped,
        f.making,
        f.textures_waiting,
        f.textures_mb,
        s.cpu,
        s.gpu.map_or(String::new(), |g| format!("{g:.0}")),
        s.ram[0],
        s.ram[1],
        s.ram[2],
        s.ram[3],
        v[0],
        v[1],
        v[2],
        v[3]
    )
}

/// Whether a window of this process is in front.
fn ours_in_front() -> bool {
    let mut pid = 0u32;
    // SAFETY: plain queries.
    unsafe {
        let front = GetForegroundWindow();
        GetWindowThreadProcessId(front, &mut pid);
        !front.is_null() && pid == GetCurrentProcessId()
    }
}

/// The panel's scale for a frame `h` pixels tall (the design is made for 1080).
fn scale(h: usize) -> f32 {
    (h as f32 / 1080.0).clamp(0.75, 4.0)
}

const FACE: &str = "Segoe UI";
const BG: [u8; 3] = [12, 13, 16];
const DIM: [u8; 3] = [140, 146, 158];
const DIMMER: [u8; 3] = [104, 110, 122];
const WHITE: [u8; 3] = [240, 242, 246];
const RED: [u8; 3] = [255, 98, 88];
/// Font slots: big number, semibold value, regular row, small hardware line.
const BIG: usize = 0;
const VALUE: usize = 1;
const ROW: usize = 2;
const SMALL: usize = 3;

/// GDI as a glyph rasteriser only: each run is drawn white on black into a scratch DIB, and its
/// grey level becomes the coverage the canvas blends in the run's colour.
struct Gdi {
    dc: HDC,
    fonts: [HFONT; 4],
    /// Ascent of each font, in pixels.
    ascent: [i32; 4],
}

impl Gdi {
    fn new(s: f32) -> Option<Gdi> {
        let face = wide(FACE);
        let make = |px: f32, weight: i32| {
            // SAFETY: a plain font creation.
            unsafe { CreateFontW(-(px * s).round() as i32, 0, 0, 0, weight, 0, 0, 0, 1, 0, 0, ANTIALIASED_QUALITY as u32, 0, face.as_ptr()) }
        };
        // SAFETY: a memory DC compatible with the screen.
        let dc = unsafe { CreateCompatibleDC(std::ptr::null_mut()) };
        let fonts = [make(34.0, 600), make(15.0, 600), make(14.0, 400), make(12.5, 400)];
        let mut g = Gdi { dc, fonts, ascent: [0; 4] };
        if dc.is_null() || fonts.iter().any(|f| f.is_null()) {
            return None; // Drop frees what exists.
        }
        for i in 0..4 {
            // SAFETY: live DC and font; a plain struct filled by the call.
            unsafe {
                SelectObject(dc, g.fonts[i]);
                let mut tm = std::mem::zeroed::<TEXTMETRICW>();
                GetTextMetricsW(dc, &mut tm);
                g.ascent[i] = tm.tmAscent;
            }
        }
        Some(g)
    }

    fn measure(&self, font: usize, text: &str) -> (i32, i32) {
        let t: Vec<u16> = text.encode_utf16().collect();
        let mut size = SIZE { cx: 0, cy: 0 };
        // SAFETY: live DC and font, a slice and its length.
        unsafe {
            SelectObject(self.dc, self.fonts[font]);
            GetTextExtentPoint32W(self.dc, t.as_ptr(), t.len() as i32, &mut size);
        }
        (size.cx, size.cy)
    }

    /// Draws `text` with its top-left at (x, y) in `rgb`.
    fn draw(&self, canvas: &mut Bitmap, font: usize, text: &str, x: i32, y: i32, rgb: [u8; 3]) {
        let t: Vec<u16> = text.encode_utf16().collect();
        let (w, h) = self.measure(font, text);
        let w = w + 2;
        if w <= 2 || h <= 0 {
            return;
        }
        let info = dib_info(w, h);
        let mut bits = std::ptr::null_mut();
        // SAFETY: a DIB section of w x h x 4 bytes (zeroed: black) selected for the text, read after
        // GdiFlush, and freed here.
        unsafe {
            let dib = CreateDIBSection(self.dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
            if dib.is_null() || bits.is_null() {
                return;
            }
            let old = SelectObject(self.dc, dib);
            SelectObject(self.dc, self.fonts[font]);
            SetBkMode(self.dc, TRANSPARENT as i32);
            SetTextColor(self.dc, 0x00ff_ffff);
            TextOutW(self.dc, 0, 0, t.as_ptr(), t.len() as i32);
            GdiFlush();
            let grey = std::slice::from_raw_parts(bits.cast::<u8>(), (w * h) as usize * 4);
            for row in 0..h {
                for col in 0..w {
                    let (px, py) = (x + col, y + row);
                    if px >= 0 && py >= 0 {
                        // Green channel: grey-scale anti-aliasing leaves all three equal.
                        canvas.over(px as usize, py as usize, rgb, u32::from(grey[((row * w + col) * 4 + 1) as usize]));
                    }
                }
            }
            SelectObject(self.dc, old);
            DeleteObject(dib);
        }
    }
}

impl Drop for Gdi {
    fn drop(&mut self) {
        // SAFETY: objects this value made.
        unsafe {
            for f in self.fonts {
                if !f.is_null() {
                    DeleteObject(f);
                }
            }
            if !self.dc.is_null() {
                DeleteDC(self.dc);
            }
        }
    }
}

/// A 32-bit top-down DIB header, `w` x `h`.
fn dib_info(w: i32, h: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: 0,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD { rgbBlue: 0, rgbGreen: 0, rgbRed: 0, rgbReserved: 0 }],
    }
}

fn fps_colour(fps: f64) -> [u8; 3] {
    if fps >= 55.0 {
        [96, 220, 130]
    } else if fps >= 30.0 {
        [250, 204, 70]
    } else {
        RED
    }
}

fn rate(r: f64) -> String {
    if r < 10.0 { format!("{r:.1}") } else { format!("{r:.0}") }
}

/// The panel's rows: label, value and the value's colour; `None` is a separator.
fn rows(s: &Stats) -> Vec<Option<(&'static str, String, [u8; 3])>> {
    let f = &s.frame;
    let mut rows = vec![
        Some(("30 s low / avg / high", s.half_minute.map_or("...".into(), |(l, m, h)| format!("{} / {} / {}", rate(l), rate(m), rate(h))), WHITE)),
        Some(("Preparing (CPU)", format!("{:.0} ms", f.preparing), WHITE)),
        Some(("  copies", format!("{:.0} ms \u{b7} {:.0} MB", f.copy_ms, f.copy_mb), WHITE)),
        Some(("  textures", format!("{:.0} ms \u{b7} {:.0} MB", f.texture_ms, f.texture_mb), WHITE)),
        Some(("Waiting for GPU", format!("{:.0} ms", f.gpu_wait), WHITE)),
        Some(("Copy back", format!("{:.0} ms \u{b7} {:.1} MB", f.back_ms, f.back_mb), WHITE)),
        Some(("Draws", format!("{:.0} / frame", f.draws), WHITE)),
        Some(("No pipeline", format!("{:.0} dropped \u{b7} {} making", f.dropped, f.making), if f.dropped >= 1.0 { RED } else { WHITE })),
        Some(("Textures", format!("{} waiting \u{b7} {} MB up", f.textures_waiting, f.textures_mb), WHITE)),
        None,
        Some(("CPU / GPU", format!("{:.0}% / {}", s.cpu, s.gpu.map_or("n/a".into(), |g| format!("{g:.0}%"))), WHITE)),
        Some(("RAM (process)", format!("{:.1} GB \u{b7} commit {:.1} GB", s.ram[0], s.ram[1]), WHITE)),
        Some(("RAM (system)", format!("{:.1} / {:.0} GB", s.ram[2], s.ram[3]), if s.ram[2] > s.ram[3] * 0.92 { RED } else { WHITE })),
    ];
    match s.vram {
        Some([budget, used, _, shared]) => {
            rows.push(Some(("VRAM", format!("{used:.2} / {budget:.2} GB"), if used > budget { RED } else { WHITE })));
            // Over a quarter GB in the PC's memory: the card is spilling.
            rows.push(Some(("Shared (PC)", format!("{shared:.2} GB"), if shared > 0.25 { RED } else { WHITE })));
        }
        None => rows.push(Some(("VRAM", "n/a".into(), WHITE))),
    }
    rows
}

/// Draws the panel for a frame `frame_h` pixels tall: a premultiplied RGBA bitmap, sized to its content.
fn render(stats: &Stats, frame_h: usize) -> Option<Bitmap> {
    let s = scale(frame_h);
    let px = |v: f32| (v * s).round() as i32;
    let g = Gdi::new(s)?;
    let (pad, gap) = (px(12.0), px(10.0));
    let rows = rows(stats);
    // The project's name and address, so a screenshot of the panel says where it came from.
    let hardware = [short_cpu(&stats.cpu_name), short_gpu(&stats.gpu_name), "Redemption of Them All".into(), "github.com/brothernog/rota".into()];
    let fps_text = rate(stats.frame.fps);
    let ms_text = format!("{:.1} ms", stats.frame.ms);
    // Width: the widest line, with at least a comfortable minimum.
    let top = g.measure(BIG, &fps_text).0 + px(6.0) + g.measure(SMALL, "FPS").0 + gap * 2 + g.measure(VALUE, &ms_text).0;
    let body = rows.iter().flatten().map(|(l, v, _)| g.measure(ROW, l).0 + gap * 2 + g.measure(ROW, v).0);
    let foot = hardware.iter().map(|t| g.measure(SMALL, t).0);
    let inner = body.chain(foot).chain([top]).max().unwrap_or(0).max(px(190.0));
    let (w, row_h) = (inner + 2 * pad, g.measure(ROW, "Ag").1 + px(3.0));
    let big_h = g.measure(BIG, "0").1;
    let small_h = g.measure(SMALL, "Ag").1 + px(1.0);
    let sep = px(9.0);
    let lines = rows.iter().filter(|r| r.is_some()).count() as i32;
    let seps = rows.len() as i32 - lines;
    let h = pad + big_h + sep + lines * row_h + seps * sep + sep + hardware.len() as i32 * small_h + pad - px(1.0);
    let mut bm = Bitmap::new(w as usize, h as usize);
    bm.fill_round((0, 0, w as usize, h as usize), 10.0 * s, BG, 218);

    let right = |font, text: &str| w - pad - g.measure(font, text).0;
    let line = |bm: &mut Bitmap, y: i32| {
        for x in pad..w - pad {
            bm.over(x as usize, y as usize, [255, 255, 255], 24);
        }
    };
    let mut y = pad;
    // The big number with FPS beside it on the same baseline; the frame time at the right edge.
    g.draw(&mut bm, BIG, &fps_text, pad, y, fps_colour(stats.frame.fps));
    let nx = pad + g.measure(BIG, &fps_text).0 + px(6.0);
    g.draw(&mut bm, SMALL, "FPS", nx, y + g.ascent[BIG] - g.ascent[SMALL], DIM);
    g.draw(&mut bm, VALUE, &ms_text, right(VALUE, &ms_text), y + g.ascent[BIG] - g.ascent[VALUE], WHITE);
    y += big_h + sep / 2;
    line(&mut bm, y);
    y += sep - sep / 2;
    for row in &rows {
        match row {
            Some((label, value, colour)) => {
                g.draw(&mut bm, ROW, label, pad, y, DIM);
                g.draw(&mut bm, ROW, value, right(ROW, value), y, *colour);
                y += row_h;
            }
            None => {
                line(&mut bm, y + sep / 2);
                y += sep;
            }
        }
    }
    y += sep / 2 - 1;
    line(&mut bm, y);
    y += sep - sep / 2 + 1;
    for (i, t) in hardware.iter().enumerate() {
        g.draw(&mut bm, SMALL, t, pad, y, [DIM, DIM, WHITE, DIMMER][i]);
        y += small_h;
    }
    Some(bm)
}

/// Sets the layered window's picture and position in one go.
///
/// # Safety
/// `window` is a live layered window of this thread.
unsafe fn present(window: HWND, bm: &Bitmap, at: POINT) {
    // SAFETY: a 32-bit top-down DIB the size of the bitmap, filled with its premultiplied pixels
    // as BGRA, handed to UpdateLayeredWindow, then freed.
    unsafe {
        let screen = GetDC(std::ptr::null_mut());
        let dc = CreateCompatibleDC(screen);
        let info = dib_info(bm.w as i32, bm.h as i32);
        let mut bits = std::ptr::null_mut();
        let dib = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        if !dib.is_null() && !bits.is_null() {
            let dst = std::slice::from_raw_parts_mut(bits.cast::<u8>(), bm.px.len());
            for (d, s) in dst.chunks_exact_mut(4).zip(bm.px.chunks_exact(4)) {
                d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
            }
            let old = SelectObject(dc, dib);
            let size = SIZE { cx: bm.w as i32, cy: bm.h as i32 };
            let from = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 255, AlphaFormat: AC_SRC_ALPHA as u8 };
            UpdateLayeredWindow(window, screen, &at, &size, dc, &from, 0, &blend, ULW_ALPHA);
            SelectObject(dc, old);
            DeleteObject(dib);
        }
        DeleteDC(dc);
        ReleaseDC(std::ptr::null_mut(), screen);
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    // SAFETY: the standard handlers; the picture is set by `present`, not by painting.
    unsafe {
        match msg {
            WM_NCHITTEST => HTTRANSPARENT as isize,
            WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// The CSV named by `VICTORY_OVERLAY_CSV`, header written (none when unset or not writable).
fn open_csv() -> Option<std::fs::File> {
    let path = std::env::var_os("VICTORY_OVERLAY_CSV").filter(|p| !p.is_empty())?;
    match std::fs::File::create(&path).and_then(|mut f| writeln!(f, "{CSV_HEADER}").map(|()| f)) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(error = %e, "overlay CSV not opened");
            None
        }
    }
}

fn run(game: HWND, adapter: Option<IDXGIAdapter3>) {
    let gpu_name = adapter
        .as_ref()
        // SAFETY: a plain query.
        .and_then(|a| unsafe { a.GetDesc().ok() })
        .map(|d| String::from_utf16_lossy(&d.Description).trim_end_matches('\0').trim().to_string())
        .unwrap_or_else(|| "GPU".into());
    let names = (cpu_name(), gpu_name);
    let mut csv = open_csv();
    let started = std::time::Instant::now();
    let mut written = 0u64;
    let class = wide("VictoryOverlay");
    // SAFETY: window class and window made and used on this thread only.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSW {
            style: 0,
            lpfnWndProc: Some(proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
        };
        RegisterClassW(&wc);
        let window = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            class.as_ptr(),
            class.as_ptr(),
            WS_POPUP,
            0,
            0,
            10,
            10,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if window.is_null() {
            tracing::warn!("overlay window not created");
            return;
        }
        tracing::info!(csv = csv.is_some(), "overlay on (VICTORY_OVERLAY=0 turns it off, F10 hides it)");
        let mut cpu = Cpu(0, 0);
        cpu.percent();
        let gpu = Gpu3d::new();
        let mut f10 = false;
        SetTimer(window, REFRESH, 500, None);
        SetTimer(window, KEYS, 40, None);
        let mut refresh = |cpu: &mut Cpu| {
            let stats = sample(cpu, gpu.as_ref(), adapter.as_ref(), &names);
            let ticks = TICKS.load(Ordering::Relaxed);
            if ticks != written {
                written = ticks;
                if let Some(f) = csv.as_mut()
                    && writeln!(f, "{}", csv_row(started.elapsed().as_secs_f64(), &stats)).is_err()
                {
                    csv = None;
                }
            }
            let mut client = std::mem::zeroed::<RECT>();
            let mut origin = POINT { x: 0, y: 0 };
            let show = VISIBLE.load(Ordering::Relaxed)
                && IsWindow(game) != 0
                && IsIconic(game) == 0
                && IsWindowVisible(game) != 0
                && ours_in_front()
                && GetClientRect(game, &mut client) != 0
                && ClientToScreen(game, &mut origin) != 0;
            let frame_h = client.bottom.max(1) as usize;
            let Some(bm) = show.then(|| render(&stats, frame_h)).flatten() else {
                ShowWindow(window, SW_HIDE);
                return;
            };
            let m = (8.0 * scale(frame_h)).round() as i32;
            let at = POINT { x: origin.x + m, y: origin.y + m };
            present(window, &bm, at);
            SetWindowPos(window, HWND_TOPMOST, at.x, at.y, bm.w as i32, bm.h as i32, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        };
        refresh(&mut cpu);
        let mut msg = std::mem::zeroed::<MSG>();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            if msg.message == WM_TIMER && msg.hwnd == window {
                if msg.wParam == REFRESH {
                    refresh(&mut cpu);
                } else {
                    // Edge-triggered: acts on the press, not the hold.
                    let d10 = GetAsyncKeyState(VK_F10) < 0;
                    if ours_in_front() && d10 && !f10 {
                        VISIBLE.fetch_xor(true, Ordering::Relaxed);
                        refresh(&mut cpu);
                    }
                    f10 = d10;
                }
                continue;
            }
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spread_needs_two_rates() {
        assert!(spread(&VecDeque::from([1.0])).is_none());
        assert_eq!(spread(&VecDeque::from([1.0, 3.0, 2.0])), Some((1.0, 2.0, 3.0)));
    }

    #[test]
    fn csv_row_matches_the_header() {
        let s = Stats { frame: Frame { fps: 1.5, making: 7, ..Frame::default() }, ..Stats::default() };
        let row = csv_row(2.0, &s);
        assert_eq!(row.split(',').count(), CSV_HEADER.split(',').count());
        assert!(row.starts_with("2.0,1.50,"), "{row}");
        // No VRAM figures: empty cells, still numbers-only.
        assert!(row.ends_with(",,,"), "{row}");
    }
}
