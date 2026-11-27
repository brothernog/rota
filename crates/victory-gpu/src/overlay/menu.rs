//! Diagnostics: the switches menu (`VICTORY_MENU=1`), a panel in the overlay's style at the game
//! window's top-right corner that takes clicks. A click turns a switch of `crate::ab::MENU` over at
//! the next present; the panel shows each switch, the clicks not applied yet, the frame times of
//! the phase under way and of the one before, and some switches only a new run changes. F9 shows
//! and hides it. The window never takes the focus, so the title keeps its keyboard and pad.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use windows_sys::Win32::Foundation::{HWND, POINT};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use super::{BG, DIM, DIMMER, Gdi, ROW, SMALL, VALUE, WHITE, present, scale, wide};
use crate::ab;
use crate::pixels::Bitmap;

const VK_F9: i32 = 0x78;
const GREEN: [u8; 3] = [96, 220, 130];
const YELLOW: [u8; 3] = [250, 204, 70];

/// Switches a click cannot change: the run's value, for the record.
const RESTART: &[&str] = &[
    "VICTORY_SECTION_MB",
    "VICTORY_RELAY_COPY",
    "VICTORY_CARD_ALL",
    "VICTORY_CARD_BUFFERS_MIN_KB",
    "VICTORY_COMPUTE_LOD",
    "VICTORY_WAVE_LDS_LOOSE",
    "VICTORY_LIBRARY_TRIM",
];

static VISIBLE: AtomicBool = AtomicBool::new(true);
/// A click came: draw again now.
static DIRTY: AtomicBool = AtomicBool::new(false);
/// The switch rows of the picture shown: top, bottom (window pixels) and the switch's number.
static ROWS: Mutex<Vec<(i32, i32, u32)>> = Mutex::new(Vec::new());

/// The switch under window row `y`.
fn hit(rows: &[(i32, i32, u32)], y: i32) -> Option<u32> {
    rows.iter().find(|&&(top, bottom, _)| (top..bottom).contains(&y)).map(|&(.., bit)| bit)
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> isize {
    match msg {
        WM_MOUSEACTIVATE => MA_NOACTIVATE as isize,
        WM_LBUTTONDOWN => {
            let y = i32::from((lp >> 16) as i16);
            if let Some(bit) = hit(&ROWS.lock().unwrap_or_else(|e| e.into_inner()), y) {
                ab::request(bit);
                DIRTY.store(true, Ordering::Relaxed);
            }
            0
        }
        // SAFETY: the standard handler.
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

pub(super) struct Menu {
    window: HWND,
    f9: std::cell::Cell<bool>,
}

impl Menu {
    /// The menu's window, when the menu is on.
    ///
    /// # Safety
    /// Called on the overlay's thread, which runs the messages of the window.
    pub(super) unsafe fn new(instance: windows_sys::Win32::Foundation::HINSTANCE) -> Option<Menu> {
        if !ab::menu() {
            return None;
        }
        let class = wide("VictoryMenu");
        // SAFETY: a window class and window of this thread.
        let window = unsafe {
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
            CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
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
            )
        };
        if window.is_null() {
            tracing::warn!("switches menu window not created");
            return None;
        }
        tracing::info!("switches menu on (F9 hides it; a click turns a switch over at the next present)");
        Some(Menu { window, f9: std::cell::Cell::new(false) })
    }

    /// F9 pressed, or a click to show: whether to draw again now.
    pub(super) fn keys(&self, ours_in_front: bool) -> bool {
        // SAFETY: a plain query.
        let down = unsafe { GetAsyncKeyState(VK_F9) } < 0;
        let held = self.f9.replace(down);
        let pressed = ours_in_front && down && !held;
        if pressed {
            VISIBLE.fetch_xor(true, Ordering::Relaxed);
        }
        pressed | DIRTY.swap(false, Ordering::Relaxed)
    }

    /// Shows the menu at the top-right corner of the game window's client area (`origin`, `width`
    /// by `frame_h`), or hides it when `show` is false.
    ///
    /// # Safety
    /// Called on the overlay's thread.
    pub(super) unsafe fn refresh(&self, show: bool, origin: POINT, width: i32, frame_h: usize) {
        let drawn = (show && VISIBLE.load(Ordering::Relaxed)).then(|| render(frame_h)).flatten();
        // SAFETY: our window, on its thread.
        unsafe {
            let Some((bm, rows)) = drawn else {
                ShowWindow(self.window, SW_HIDE);
                return;
            };
            *ROWS.lock().unwrap_or_else(|e| e.into_inner()) = rows;
            let m = (8.0 * scale(frame_h)).round() as i32;
            let at = POINT { x: origin.x + width - bm.w as i32 - m, y: origin.y + m };
            present(self.window, &bm, at);
            SetWindowPos(self.window, HWND_TOPMOST, at.x, at.y, bm.w as i32, bm.h as i32, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        }
    }
}

fn short(name: &str) -> &str {
    name.strip_prefix("VICTORY_").unwrap_or(name)
}

/// The panel and its switch rows (top, bottom, switch).
fn render(frame_h: usize) -> Option<(Bitmap, Vec<(i32, i32, u32)>)> {
    let s = scale(frame_h);
    let px = |v: f32| (v * s).round() as i32;
    let g = Gdi::new(s)?;
    let (pad, gap) = (px(12.0), px(10.0));
    let (row_h, small_h) = (g.measure(ROW, "Ag").1 + px(1.0), g.measure(SMALL, "Ag").1 + px(3.0));
    let sep = px(9.0);
    let pending = ab::pending();
    let [now, last] = ab::phases();
    let phase = |(frames, median): (usize, f32)| {
        if frames == 0 { "...".to_string() } else { format!("{frames} fr \u{b7} {median:.0} ms \u{b7} {:.1} fps", 1000.0 / median.max(1.0)) }
    };
    let restart: Vec<String> = RESTART
        .iter()
        .map(|n| format!("{}={}", short(n), std::env::var(n).unwrap_or_else(|_| "unset".into())))
        .collect();
    let lines = [("This phase", phase(now)), ("Last phase", phase(last))];
    let foot = "* run's own value \u{b7} click to flip";

    let tag = |bit: u32| {
        let (on, own) = ab::menu_state(bit);
        let text = format!("{}{}", if on { "ON" } else { "off" }, if own { "*" } else { "" });
        (text, if pending >> bit & 1 != 0 { YELLOW } else if on { GREEN } else { DIM })
    };
    let tag_w = g.measure(VALUE, "off*").0.max(g.measure(VALUE, "ON*").0) + gap;
    let name_w = ab::MENU.iter().map(|(n, ..)| g.measure(ROW, short(n)).0).max().unwrap_or(0) + gap;
    let widths = ab::MENU
        .iter()
        .map(|(_, _, what)| tag_w + name_w + g.measure(SMALL, what).0)
        .chain(lines.iter().map(|(l, v)| g.measure(ROW, l).0 + gap * 2 + g.measure(ROW, v).0))
        .chain(restart.iter().map(|t| g.measure(SMALL, t).0))
        .chain([g.measure(SMALL, foot).0, g.measure(VALUE, "Live switches").0 + gap * 2 + g.measure(SMALL, "F9").0]);
    let w = widths.max().unwrap_or(0).max(px(220.0)) + 2 * pad;
    let h = pad
        + row_h
        + sep
        + ab::MENU.len() as i32 * row_h
        + sep
        + lines.len() as i32 * row_h
        + sep
        + small_h
        + restart.len() as i32 * small_h
        + sep
        + small_h
        + pad;
    let mut bm = Bitmap::new(w as usize, h as usize);
    bm.fill_round((0, 0, w as usize, h as usize), 10.0 * s, BG, 218);
    let line = |bm: &mut Bitmap, y: i32| {
        for x in pad..w - pad {
            bm.over(x as usize, y as usize, [255, 255, 255], 24);
        }
    };

    let mut y = pad;
    g.draw(&mut bm, VALUE, "Live switches", pad, y, WHITE);
    g.draw(&mut bm, SMALL, "F9", w - pad - g.measure(SMALL, "F9").0, y + g.ascent[VALUE] - g.ascent[SMALL], DIM);
    y += row_h + sep / 2;
    line(&mut bm, y);
    y += sep - sep / 2;
    let mut rows = Vec::with_capacity(ab::MENU.len());
    for (bit, (name, _, what)) in ab::MENU.iter().enumerate() {
        let bit = bit as u32;
        let (text, colour) = tag(bit);
        let top = y;
        g.draw(&mut bm, VALUE, &text, pad, y, colour);
        g.draw(&mut bm, ROW, short(name), pad + tag_w, y, WHITE);
        g.draw(&mut bm, SMALL, what, pad + tag_w + name_w, y + g.ascent[ROW] - g.ascent[SMALL], DIMMER);
        y += row_h;
        rows.push((top, y, bit));
    }
    y += sep / 2;
    line(&mut bm, y);
    y += sep - sep / 2;
    for (label, value) in &lines {
        g.draw(&mut bm, ROW, label, pad, y, DIM);
        g.draw(&mut bm, ROW, value, w - pad - g.measure(ROW, value).0, y, WHITE);
        y += row_h;
    }
    y += sep / 2;
    line(&mut bm, y);
    y += sep - sep / 2;
    g.draw(&mut bm, SMALL, "New run only", pad, y, DIM);
    y += small_h;
    for t in &restart {
        g.draw(&mut bm, SMALL, t, pad, y, DIMMER);
        y += small_h;
    }
    y += sep / 2;
    line(&mut bm, y);
    y += sep - sep / 2;
    g.draw(&mut bm, SMALL, foot, pad, y, DIMMER);
    Some((bm, rows))
}

#[cfg(test)]
mod tests {
    use super::{hit, render};

    #[test]
    fn the_panel_holds_every_switch_row() {
        for frame_h in [720, 1080, 2160] {
            let (bm, rows) = render(frame_h).expect("a panel");
            assert_eq!(rows.len(), crate::ab::MENU.len());
            assert!(rows.windows(2).all(|r| r[0].1 <= r[1].0), "rows overlap at {frame_h}");
            assert!(rows.last().unwrap().1 < bm.h as i32, "last row outside the panel at {frame_h}");
            assert!(bm.h + 16 * frame_h / 1080 <= frame_h, "panel taller than the picture at {frame_h}");
        }
    }

    #[test]
    fn a_click_finds_the_row_under_it() {
        let rows = [(10, 30, 0), (30, 50, 1)];
        assert_eq!(hit(&rows, 9), None);
        assert_eq!(hit(&rows, 10), Some(0));
        assert_eq!(hit(&rows, 29), Some(0));
        assert_eq!(hit(&rows, 30), Some(1));
        assert_eq!(hit(&rows, 50), None);
    }
}
