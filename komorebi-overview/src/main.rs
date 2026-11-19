//! komorebi-overview: a Hyprland-style workspace overview.
//!
//! Run it (e.g. from a hotkey) to zoom the current monitor out into a grid of
//! all its workspaces. Pick one with the mouse, hjkl/arrows + Enter, or 1-0.
//! Escape, a click outside the grid, or running it again cancels.

#![windows_subsystem = "windows"]

mod capture;
mod gfx;
mod model;

use capture::WindowShot;
use gfx::Gfx;
use gfx::RectF;
use gfx::rgba;
use model::Monitor;
use model::Rect;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;
use std::time::Instant;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::ID2D1Bitmap;
use windows::Win32::Graphics::Dwm::DWMWA_CLOAK;
use windows::Win32::Graphics::Dwm::DwmSetWindowAttribute;
use windows::Win32::Graphics::Gdi::ValidateRect;
use windows::Win32::System::Com::COINIT_APARTMENTTHREADED;
use windows::Win32::System::Com::CoInitializeEx;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2;
use windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::BOOL;
use windows::core::w;

const CLASS: windows::core::PCWSTR = w!("komorebi-overview");
const WM_SHOTS: u32 = WM_APP + 1;
const WM_TOGGLE: u32 = WM_APP + 2;

const OPEN: Duration = Duration::from_millis(240);
const CLOSE: Duration = Duration::from_millis(210);

const ACCENT: u32 = 0x33ccff;

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

fn ease_in_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
    }
}

#[derive(Clone, Copy)]
enum Phase {
    Opening(Instant),
    Open,
    Closing { start: Instant, target: usize },
    Done,
}

struct Shot {
    frame: Rect,
    title: String,
    bitmap: Option<ID2D1Bitmap>,
}

struct App {
    hwnd: HWND,
    gfx: Gfx,
    /// Overlay currently shown
    visible: bool,
    /// Direct2D device lost; recreate on next open
    lost: bool,
    wallpaper_path: Option<String>,
    monitor: Monitor,
    size: (f32, f32),
    cells: Vec<RectF>,
    cols: usize,
    screen: Option<ID2D1Bitmap>,
    wallpaper: Option<ID2D1Bitmap>,
    shots: HashMap<isize, Shot>,
    rx: mpsc::Receiver<WindowShot>,
    phase: Phase,
    hover: Option<usize>,
    selected: usize,
    /// Smoothed 0..1 highlight per cell
    glow: Vec<f32>,
    last_frame: Instant,
    dirty: bool,
}

/// Grid of `n` cells with the monitor's aspect ratio, as large as fits
fn layout(n: usize, w: f32, h: f32) -> (Vec<RectF>, usize) {
    let aspect = w / h;
    let margin = 0.06 * w.min(h);
    let gap = 0.025 * w.min(h);
    let (aw, ah) = (w - 2.0 * margin, h - 2.0 * margin);

    let mut best = (0.0, 1, 0.0, 0.0);
    for cols in 1..=n.max(1) {
        let rows = n.div_ceil(cols);
        let mut cw = (aw - gap * (cols - 1) as f32) / cols as f32;
        let mut ch = cw / aspect;
        if ch * rows as f32 + gap * (rows - 1) as f32 > ah {
            ch = (ah - gap * (rows - 1) as f32) / rows as f32;
            cw = ch * aspect;
        }
        if cw * ch > best.0 {
            best = (cw * ch, cols, cw, ch);
        }
    }

    let (_, cols, cw, ch) = best;
    let rows = n.div_ceil(cols);
    let grid_h = ch * rows as f32 + gap * (rows - 1) as f32;
    let top = (h - grid_h) / 2.0;

    let cells = (0..n)
        .map(|i| {
            let (row, col) = (i / cols, i % cols);
            // Centre each row, including a shorter last row
            let in_row = (n - row * cols).min(cols);
            let row_w = cw * in_row as f32 + gap * (in_row - 1) as f32;
            let left = (w - row_w) / 2.0;
            RectF::new(
                left + col as f32 * (cw + gap),
                top + row as f32 * (ch + gap),
                cw,
                ch,
            )
        })
        .collect();

    (cells, cols)
}

impl App {
    fn full(&self) -> RectF {
        RectF::new(0.0, 0.0, self.size.0, self.size.1)
    }

    /// 0 = zoomed into one workspace, 1 = full overview
    fn openness(&self, now: Instant) -> f32 {
        match self.phase {
            Phase::Opening(start) => ease_out((now - start).as_secs_f32() / OPEN.as_secs_f32()),
            Phase::Open => 1.0,
            Phase::Closing { start, .. } => {
                1.0 - ease_in_out((now - start).as_secs_f32() / CLOSE.as_secs_f32())
            }
            Phase::Done => 0.0,
        }
    }

    /// The workspace that fills the screen when openness is 0
    fn zoom_cell(&self) -> usize {
        match self.phase {
            Phase::Closing { target, .. } => target,
            _ => self.monitor.focused_workspace,
        }
    }

    fn interactive(&self) -> bool {
        matches!(self.phase, Phase::Opening(_) | Phase::Open)
    }

    fn choose(&mut self, target: usize) {
        if !self.interactive() || target >= self.cells.len() {
            return;
        }
        if target != self.monitor.focused_workspace {
            // Switch underneath the overlay while the zoom-in animation runs
            model::focus(self.monitor.index, target);
        }
        self.selected = target;
        self.phase = Phase::Closing {
            start: Instant::now(),
            target,
        };
    }

    fn cancel(&mut self) {
        let focused = self.monitor.focused_workspace;
        self.choose(focused);
    }

    fn cell_at(&self, x: f32, y: f32) -> Option<usize> {
        self.cells.iter().position(|c| c.contains(x, y))
    }

    fn key(&mut self, vk: u16) {
        if !self.interactive() {
            return;
        }
        let n = self.cells.len();
        let cols = self.cols;
        let s = self.selected;
        let vk = VIRTUAL_KEY(vk);

        match vk {
            VK_ESCAPE => self.cancel(),
            VK_RETURN | VK_SPACE => self.choose(s),
            VK_LEFT | VK_H if s % cols > 0 => self.selected = s - 1,
            VK_RIGHT | VK_L if s % cols + 1 < cols && s + 1 < n => self.selected = s + 1,
            VK_UP | VK_K if s >= cols => self.selected = s - cols,
            VK_DOWN | VK_J if s + cols < n => self.selected = s + cols,
            VK_TAB => self.selected = (s + 1) % n,
            VK_Q => self.cancel(),
            _ => {
                let digit = match vk.0 {
                    k @ 0x31..=0x39 => Some((k - 0x31) as usize),
                    0x30 => Some(9),
                    k @ 0x61..=0x69 => Some((k - 0x61) as usize),
                    0x60 => Some(9),
                    _ => None,
                };
                if let Some(d) = digit {
                    self.choose(d);
                }
            }
        }
        self.hover = None;
        self.dirty = true;
    }

    fn mouse_move(&mut self, x: f32, y: f32) {
        if !self.interactive() {
            return;
        }
        let hover = self.cell_at(x, y);
        if hover != self.hover {
            self.hover = hover;
            if let Some(h) = hover {
                self.selected = h;
            }
            self.dirty = true;
        }
    }

    fn click(&mut self, x: f32, y: f32) {
        match self.cell_at(x, y) {
            Some(i) => self.choose(i),
            None => self.cancel(),
        }
    }

    fn receive_shots(&mut self) {
        while let Ok(shot) = self.rx.try_recv() {
            let bitmap = shot.pixels.as_ref().and_then(|p| self.gfx.bitmap(p));
            self.shots.insert(
                shot.hwnd,
                Shot {
                    frame: shot.frame,
                    title: shot.title,
                    bitmap,
                },
            );
            self.dirty = true;
        }
    }

    /// Advance animations and draw if anything changed. Returns true while
    /// more frames are needed.
    fn tick(&mut self) -> bool {
        if !self.visible {
            return false;
        }
        self.receive_shots();

        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().min(0.05);
        self.last_frame = now;

        // Smooth hover/selection highlight
        let mut glowing = false;
        for i in 0..self.glow.len() {
            let target = if self.interactive() && i == self.selected {
                1.0
            } else {
                0.0
            };
            let g = &mut self.glow[i];
            let next = *g + (target - *g) * (1.0 - (-dt * 18.0).exp());
            *g = if (next - target).abs() < 0.002 {
                target
            } else {
                next
            };
            glowing |= *g != target;
        }

        let animating = match self.phase {
            Phase::Opening(start) if now - start >= OPEN => {
                self.phase = Phase::Open;
                true
            }
            Phase::Closing { start, .. } if now - start >= CLOSE => {
                self.phase = Phase::Done;
                false
            }
            Phase::Opening(_) | Phase::Closing { .. } => true,
            Phase::Open | Phase::Done => false,
        };

        if matches!(self.phase, Phase::Done) {
            self.hide();
            return false;
        }

        if animating || glowing || self.dirty {
            self.render(now);
            self.dirty = false;
        }

        animating || glowing
    }

    fn new(hwnd: HWND, gfx: Gfx) -> Self {
        Self {
            hwnd,
            gfx,
            visible: false,
            lost: false,
            wallpaper: None,
            wallpaper_path: None,
            monitor: Monitor::default(),
            size: (1.0, 1.0),
            cells: vec![],
            cols: 1,
            screen: None,
            shots: HashMap::new(),
            rx: mpsc::channel().1,
            phase: Phase::Done,
            hover: None,
            selected: 0,
            glow: vec![],
            last_frame: Instant::now(),
            dirty: false,
        }
    }

    /// Zoom out of the workspace on the monitor under the cursor
    fn open(&mut self) -> Result<(), String> {
        if self.visible {
            return Ok(());
        }

        let t0 = Instant::now();
        let mark = |what: &str| {
            if debugging() {
                log(&format!(
                    "{:>6.1}ms {what}",
                    t0.elapsed().as_secs_f64() * 1000.0
                ));
            }
        };

        let mut cursor = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut cursor);
        }
        let monitor = model::monitor_at(cursor.x, cursor.y)?;
        if monitor.workspaces.is_empty() {
            return Err("monitor has no workspaces".into());
        }
        mark("state queried");

        // Grab the screen before the overlay covers it
        let screen = capture::screen(monitor.rect);
        mark("screen captured");

        let r = monitor.rect;
        if self.lost {
            self.gfx = Gfx::new(self.hwnd, r.w() as u32, r.h() as u32)
                .map_err(|e| format!("Direct2D init: {e}"))?;
            self.wallpaper = None;
            self.wallpaper_path = None;
            self.lost = false;
        }
        self.gfx.resize(r.w() as u32, r.h() as u32);

        let wallpaper_path = gfx::wallpaper_path();
        if wallpaper_path != self.wallpaper_path {
            self.wallpaper = wallpaper_path
                .as_deref()
                .and_then(|p| self.gfx.image_file(p));
            self.wallpaper_path = wallpaper_path;
        }

        let (w, h) = (r.w() as f32, r.h() as f32);
        let (cells, cols) = layout(monitor.workspaces.len(), w, h);
        let (tx, rx) = mpsc::channel();
        spawn_captures(&monitor, self.hwnd, tx);

        self.screen = screen.as_ref().and_then(|p| self.gfx.bitmap(p));
        self.selected = monitor.focused_workspace;
        self.glow = vec![0.0; cells.len()];
        self.monitor = monitor;
        self.size = (w, h);
        self.cells = cells;
        self.cols = cols;
        self.shots.clear();
        self.rx = rx;
        self.hover = None;
        self.dirty = true;
        self.visible = true;
        self.gfx.reset_blur();
        mark("prepared");

        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                r.l,
                r.t,
                r.w(),
                r.h(),
                SWP_NOACTIVATE,
            );

            // Show cloaked, draw the first frame (identical to the screen),
            // then uncloak: no flash of an empty window
            cloak(self.hwnd, true);
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            self.phase = Phase::Opening(Instant::now());
            self.render(Instant::now());
            cloak(self.hwnd, false);
            take_foreground(self.hwnd);
        }
        self.phase = Phase::Opening(Instant::now());
        self.last_frame = Instant::now();
        mark("first frame shown");
        Ok(())
    }

    fn hide(&mut self) {
        if debugging() {
            log("hide");
        }
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.visible = false;
        self.phase = Phase::Done;
        // Free this session's GPU bitmaps; the device and wallpaper stay warm
        self.screen = None;
        self.shots.clear();
        self.rx = mpsc::channel().1;
    }

    fn toggle(&mut self) {
        if self.visible {
            self.cancel();
        } else if let Err(error) = self.open() {
            log(&format!("komorebi-overview: {error}"));
            self.hide();
        }
    }

    fn render(&mut self, now: Instant) {
        let o = self.openness(now);
        let zoom = self.zoom_cell();
        let full = self.full();

        self.gfx.begin();

        // Backdrop: the current screen, blurred and dimmed
        if let Some(screen) = self.screen.clone() {
            self.gfx.blurred(&screen, 36.0);
        }
        self.gfx.fill(full, 0.0, rgba(0x05070c, 0.55 * o));

        for i in 0..self.cells.len() {
            if i == zoom {
                continue;
            }
            let rect = self.cells[i].scaled(0.92 + 0.08 * o);
            self.draw_cell(i, rect, o, o);
        }

        // The zooming workspace morphs between full screen and its grid cell
        let rect = RectF::lerp(full, self.cells[zoom], o);
        self.draw_cell(zoom, rect, 1.0, o);

        if !self.gfx.end() {
            self.lost = true;
        }
    }

    fn draw_cell(&self, i: usize, rect: RectF, opacity: f32, o: f32) {
        let gfx = &self.gfx;
        let cell = self.cells[i];
        let scale = rect.w() / cell.w();
        let radius = 14.0 * o.min(1.0) * scale.min(1.5);
        let glow = self.glow[i];
        let rect = rect.scaled(1.0 + 0.025 * glow * o);
        let workspace = &self.monitor.workspaces[i];
        let focused = i == self.monitor.focused_workspace;

        // Soft shadow
        if o > 0.01 {
            let shadow = RectF {
                l: rect.l - 6.0,
                t: rect.t - 2.0,
                r: rect.r + 6.0,
                b: rect.b + 12.0,
            };
            gfx.fill(shadow, radius + 6.0, rgba(0x000000, 0.28 * opacity * o));
        }

        if focused && let Some(screen) = &self.screen {
            gfx.image_rounded(screen, None, rect, radius, opacity);
        } else {
            self.draw_desktop(rect, radius, opacity);
            self.draw_windows(workspace, rect, opacity, o);
        }

        // Borders: current workspace in the accent colour, selection in white
        if o > 0.01 {
            if focused {
                gfx.stroke(rect, radius, 3.0, rgba(ACCENT, opacity * o));
            }
            if glow > 0.0 {
                let inset = if focused { 4.0 } else { 0.0 };
                let r = RectF {
                    l: rect.l - 3.0 - inset,
                    t: rect.t - 3.0 - inset,
                    r: rect.r + 3.0 + inset,
                    b: rect.b + 3.0 + inset,
                };
                gfx.stroke(
                    r,
                    radius + 3.0,
                    2.5,
                    rgba(0xffffff, 0.85 * glow * opacity * o),
                );
            }
            self.draw_label(&workspace.name, rect, focused, opacity * o, glow);
        }
    }

    fn draw_desktop(&self, rect: RectF, radius: f32, opacity: f32) {
        let gfx = &self.gfx;
        match &self.wallpaper {
            Some(wp) => {
                // "Fill": crop the wallpaper to the monitor's aspect ratio
                let size = unsafe { wp.GetSize() };
                let target = self.size.0 / self.size.1;
                let src = if size.width / size.height > target {
                    let w = size.height * target;
                    RectF::new((size.width - w) / 2.0, 0.0, w, size.height)
                } else {
                    let h = size.width / target;
                    RectF::new(0.0, (size.height - h) / 2.0, size.width, h)
                };
                gfx.image_rounded(wp, Some(src), rect, radius, opacity);
                gfx.fill(rect, radius, rgba(0x000000, 0.18 * opacity));
            }
            None => gfx.fill(rect, radius, rgba(0x1b2030, opacity)),
        }
    }

    fn draw_windows(&self, workspace: &model::Workspace, rect: RectF, opacity: f32, o: f32) {
        let gfx = &self.gfx;
        let mon = self.monitor.rect;
        let (sx, sy) = (rect.w() / mon.w() as f32, rect.h() / mon.h() as f32);

        if workspace.windows.is_empty() {
            let size = rect.h() * 0.28;
            gfx.text(
                &workspace.name,
                rect,
                size,
                true,
                rgba(0xffffff, 0.22 * opacity * o.max(0.3)),
            );
            return;
        }

        gfx.clip(rect);
        for hwnd in &workspace.windows {
            let Some(shot) = self.shots.get(hwnd) else {
                continue;
            };
            let f = shot.frame;
            let dest = RectF::new(
                rect.l + (f.l - mon.l) as f32 * sx,
                rect.t + (f.t - mon.t) as f32 * sy,
                f.w() as f32 * sx,
                f.h() as f32 * sy,
            );
            match &shot.bitmap {
                Some(bitmap) => gfx.image(bitmap, dest, opacity),
                None => {
                    // Uncapturable (e.g. a game): a labelled placeholder
                    gfx.fill(dest, 6.0, rgba(0x232a3a, 0.95 * opacity));
                    gfx.stroke(dest, 6.0, 1.0, rgba(0xffffff, 0.12 * opacity));
                    let size = (dest.h() * 0.09).clamp(9.0, 22.0);
                    gfx.text(
                        &shot.title,
                        dest,
                        size,
                        false,
                        rgba(0xffffff, 0.75 * opacity),
                    );
                }
            }
        }
        gfx.unclip();
    }

    fn draw_label(&self, name: &str, rect: RectF, focused: bool, alpha: f32, glow: f32) {
        let gfx = &self.gfx;
        let size = (self.cells[0].h() * 0.075).clamp(13.0, 26.0);
        let pad = size * 0.55;
        let w = gfx.measure(name, size, true) + pad * 2.0;
        let h = size * 1.6;
        let pill = RectF::new(rect.l + size * 0.6, rect.t + size * 0.6, w.max(h), h);
        let bg = if focused { ACCENT } else { 0x10131a };
        let bg_alpha = if focused { 0.95 } else { 0.72 + 0.2 * glow };
        gfx.fill(pill, h / 2.0, rgba(bg, bg_alpha * alpha));
        let fg = if focused { 0x06121a } else { 0xffffff };
        gfx.text(name, pill, size, true, rgba(fg, alpha));
    }
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|cell| {
        let mut borrow = cell.try_borrow_mut().ok()?;
        borrow.as_mut().map(f)
    })
}

fn lparam_point(l: LPARAM) -> (f32, f32) {
    let x = (l.0 & 0xffff) as i16 as f32;
    let y = ((l.0 >> 16) & 0xffff) as i16 as f32;
    (x, y)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_MOUSEMOVE => {
            let (x, y) = lparam_point(lparam);
            with_app(|a| a.mouse_move(x, y));
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_point(lparam);
            with_app(|a| a.click(x, y));
            LRESULT(0)
        }
        WM_TOGGLE => {
            if debugging() {
                log("toggle");
            }
            with_app(App::toggle);
            LRESULT(0)
        }
        WM_RBUTTONUP | WM_MBUTTONUP | WM_CLOSE => {
            with_app(App::cancel);
            LRESULT(0)
        }
        WM_KEYDOWN | WM_SYSKEYDOWN => {
            let vk = wparam.0 as u16;
            // Alt+F4 still closes, via WM_CLOSE -> cancel
            if msg == WM_SYSKEYDOWN && vk == VK_F4.0 {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
            if debugging() {
                log(&format!("key {vk:#x}"));
            }
            with_app(|a| a.key(vk));
            LRESULT(0)
        }
        // Releasing Alt (from the Alt+Q hotkey) must not open the system menu
        WM_SYSCOMMAND if (wparam.0 & 0xfff0) as u32 == SC_KEYMENU => LRESULT(0),
        WM_SYSKEYUP | WM_SYSCHAR => LRESULT(0),
        WM_ACTIVATE => {
            if (wparam.0 & 0xffff) as u32 == WA_INACTIVE {
                if debugging() {
                    let other = lparam.0;
                    let fg = unsafe { GetForegroundWindow() }.0 as isize;
                    log(&format!(
                        "deactivated: lparam hwnd {other} ({:?}), foreground {fg} ({:?})",
                        capture::title(other),
                        capture::title(fg)
                    ));
                }
                with_app(App::cancel);
            }
            LRESULT(0)
        }
        WM_PAINT => {
            unsafe {
                let _ = ValidateRect(Some(hwnd), None);
            }
            with_app(|a| a.dirty = true);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            unsafe {
                if let Ok(cursor) = LoadCursorW(None, IDC_ARROW) {
                    SetCursor(Some(cursor));
                }
            }
            LRESULT(1)
        }
        WM_ERASEBKGND | WM_SHOTS => LRESULT(1),
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

fn cloak(hwnd: HWND, on: bool) {
    let value = BOOL::from(on);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK,
            &value as *const BOOL as *const _,
            size_of::<BOOL>() as u32,
        );
    }
}

/// Capture every window of the hidden workspaces on a few worker threads
fn spawn_captures(monitor: &Monitor, hwnd: HWND, tx: mpsc::Sender<WindowShot>) {
    let mut jobs: Vec<isize> = monitor
        .workspaces
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != monitor.focused_workspace)
        .flat_map(|(_, w)| w.windows.iter().copied())
        .collect();
    jobs.dedup();

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(2, 8);
    let overlay = hwnd.0 as isize;

    for chunk in jobs.chunks(jobs.len().div_ceil(threads).max(1)) {
        let chunk = chunk.to_vec();
        let tx = tx.clone();
        std::thread::spawn(move || {
            for h in chunk {
                if let Some(shot) = capture::window(h) {
                    if tx.send(shot).is_err() {
                        return;
                    }
                    unsafe {
                        let _ = PostMessageW(
                            Some(HWND(overlay as *mut _)),
                            WM_SHOTS,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
            }
        });
    }
}

/// Make `hwnd` the foreground window with keyboard focus. Windows refuses this to a
/// process that did not receive the last input (the hotkey went to whkd), so when the
/// plain request does not stick, the input queue of the current foreground window is
/// borrowed for the switch.
fn take_foreground(hwnd: HWND) {
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        if GetForegroundWindow() == hwnd {
            let _ = SetFocus(Some(hwnd));
            return;
        }

        let foreground = GetForegroundWindow();
        let theirs = GetWindowThreadProcessId(foreground, None);
        let ours = windows::Win32::System::Threading::GetCurrentThreadId();
        let attached = theirs != 0
            && theirs != ours
            && windows::Win32::System::Threading::AttachThreadInput(ours, theirs, true).as_bool();
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        if attached {
            let _ = windows::Win32::System::Threading::AttachThreadInput(ours, theirs, false);
        }

        if debugging() {
            log(&format!(
                "foreground was refused; after borrowing the input of thread {theirs}: ours = {}",
                GetForegroundWindow() == hwnd
            ));
        }
    }
}

/// Diagnostics are written when KOMOREBI_OVERVIEW_DEBUG is set or
/// %TEMP%\komorebi-overview.debug exists (the resident instance does not see
/// the environment of the hotkey that toggles it)
fn debugging() -> bool {
    std::env::var_os("KOMOREBI_OVERVIEW_DEBUG").is_some()
        || std::env::temp_dir()
            .join("komorebi-overview.debug")
            .exists()
}

/// Errors go to %TEMP%\komorebi-overview.log (there is no console)
fn log(message: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("komorebi-overview.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{message}");
    }
}

fn main() {
    if let Err(error) = run() {
        log(&format!("komorebi-overview: {error}"));
    }
}

fn run() -> Result<(), String> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);

        // A resident instance is already warm: ask it to open (or close) and exit
        if let Ok(existing) = FindWindowW(CLASS, None)
            && !existing.is_invalid()
        {
            if std::env::args().all(|a| a != "--daemon") {
                // This instance was started by the hotkey and may take the foreground;
                // pass that on, so that the overview gets the keyboard when it opens
                let mut pid = 0;
                GetWindowThreadProcessId(existing, Some(&mut pid));
                if pid != 0 {
                    let _ = AllowSetForegroundWindow(pid);
                }
                let _ = PostMessageW(Some(existing), WM_TOGGLE, WPARAM(0), LPARAM(0));
            }
            return Ok(());
        }

        // Become the resident instance: window, Direct2D device and wallpaper
        // are created once so every later open takes ~50 ms instead of ~350 ms
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let module = GetModuleHandleW(None).map_err(|e| e.to_string())?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wndproc),
            hInstance: module.into(),
            lpszClassName: CLASS,
            ..Default::default()
        };
        RegisterClassExW(&class);

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            CLASS,
            w!("komorebi overview"),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(module.into()),
            None,
        )
        .map_err(|e| format!("CreateWindowExW: {e}"))?;

        let gfx = Gfx::new(hwnd, 1, 1).map_err(|e| format!("Direct2D init: {e}"))?;
        let mut app = App::new(hwnd, gfx);
        app.wallpaper_path = gfx::wallpaper_path();
        app.wallpaper = app
            .wallpaper_path
            .as_deref()
            .and_then(|p| app.gfx.image_file(p));
        APP.with(|cell| *cell.borrow_mut() = Some(app));

        if std::env::args().all(|a| a != "--daemon") {
            with_app(App::toggle);
        }

        let mut msg = MSG::default();
        loop {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            let busy = with_app(App::tick).unwrap_or(false);
            if !busy {
                let _ = WaitMessage();
            }
        }
    }
}
