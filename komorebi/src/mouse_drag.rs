//! Hyprland-style modifier + mouse drag to move and resize windows.
//!
//! A low-level mouse hook watches for `modifier + button` presses. The press
//! and release are swallowed so the click never reaches the application, and a
//! worker thread moves/resizes the window under the cursor as the mouse moves.
//!
//! Drags are reported to the window manager as `MoveResizeStart` and
//! `MoveResizeEnd` events, so the existing logic that handles native title-bar
//! drags (container swaps, cross-monitor moves, resize ratio updates) is reused.
//!
//! Tiled windows are resized live: while dragging, the layout is periodically
//! updated so neighbouring tiles follow the cursor, like Hyprland. Edges with
//! no neighbouring tile are never picked, so a resize always does something.

use std::cell::Cell;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
use parking_lot::Mutex;
use serde::Deserialize;
use serde::Serialize;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::LPARAM;
use windows::Win32::Foundation::LRESULT;
use windows::Win32::Foundation::POINT;
use windows::Win32::Foundation::RECT;
use windows::Win32::Foundation::WPARAM;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT_0;
use windows::Win32::UI::Input::KeyboardAndMouse::INPUT_KEYBOARD;
use windows::Win32::UI::Input::KeyboardAndMouse::KEYBDINPUT;
use windows::Win32::UI::Input::KeyboardAndMouse::KEYEVENTF_KEYUP;
use windows::Win32::UI::Input::KeyboardAndMouse::SendInput;
use windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_LWIN;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_MENU;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RWIN;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_SHIFT;
use windows::Win32::UI::WindowsAndMessaging::CallNextHookEx;
use windows::Win32::UI::WindowsAndMessaging::GA_ROOT;
use windows::Win32::UI::WindowsAndMessaging::GWL_STYLE;
use windows::Win32::UI::WindowsAndMessaging::GetAncestor;
use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
use windows::Win32::UI::WindowsAndMessaging::GetMessageW;
use windows::Win32::UI::WindowsAndMessaging::GetShellWindow;
use windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows::Win32::UI::WindowsAndMessaging::IsZoomed;
use windows::Win32::UI::WindowsAndMessaging::LLMHF_INJECTED;
use windows::Win32::UI::WindowsAndMessaging::MSG;
use windows::Win32::UI::WindowsAndMessaging::MSLLHOOKSTRUCT;
use windows::Win32::UI::WindowsAndMessaging::SWP_ASYNCWINDOWPOS;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOACTIVATE;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOOWNERZORDER;
use windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER;
use windows::Win32::UI::WindowsAndMessaging::SetWindowPos;
use windows::Win32::UI::WindowsAndMessaging::SetWindowsHookExW;
use windows::Win32::UI::WindowsAndMessaging::WH_MOUSE_LL;
use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_MBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_MBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONDOWN;
use windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP;
use windows::Win32::UI::WindowsAndMessaging::WS_CAPTION;
use windows::Win32::UI::WindowsAndMessaging::WS_CHILD;
use windows::Win32::UI::WindowsAndMessaging::WS_THICKFRAME;

use crate::Window;
use crate::animation::ANIMATION_ENABLED_GLOBAL;
use crate::core::Layout;
use crate::core::Rect;
use crate::core::Sizing;
use crate::core::OperationDirection;
use crate::window_manager::WindowManager;
use crate::window_manager_event::WindowManagerEvent;
use crate::winevent::WinEvent;
use crate::winevent_listener;
use crate::workspace::WorkspaceLayer;

/// Smallest width/height a window can be resized down to with the mouse
const MIN_SIZE: i32 = 100;

/// How often the tiling layout is recomputed during a live resize
const LIVE_RESIZE_INTERVAL: Duration = Duration::from_millis(33);

/// An unassigned virtual key, tapped while Win is held so that releasing Win
/// after a drag does not open the Start menu
const VK_DUMMY: u16 = 0xE8;

/// Processes whose windows must never be dragged
const IGNORED_EXES: &[&str] = &["komorebi-bar.exe", "komorebi.exe"];

pub static MOUSE_DRAG_ENABLED: AtomicBool = AtomicBool::new(false);
static LIVE_RESIZE: AtomicBool = AtomicBool::new(true);

/// Set while a mouse_drag move is handed to MoveResizeEnd, so it is never
/// misclassified as a resize by komorebi's size-change heuristic
static MOVE_HINT: AtomicBool = AtomicBool::new(false);

/// Consume the move hint (called by the MoveResizeEnd handler)
pub fn take_move_hint() -> bool {
    MOVE_HINT.swap(false, Ordering::SeqCst)
}
static MODIFIER: AtomicU8 = AtomicU8::new(MouseDragModifier::Win as u8);
static MOVE_BUTTON: AtomicU8 = AtomicU8::new(MouseDragButton::Left as u8);
static RESIZE_BUTTON: AtomicU8 = AtomicU8::new(MouseDragButton::Right as u8);

/// Testing aid: also react to synthetic (injected) mouse input when
/// `KOMOREBI_MOUSE_DRAG_ALLOW_INJECTED=1` is set in the environment
static ALLOW_INJECTED: OnceLock<bool> = OnceLock::new();

static WM: OnceLock<Arc<Mutex<WindowManager>>> = OnceLock::new();
static CHANNEL: OnceLock<(Sender<DragMessage>, Receiver<DragMessage>)> = OnceLock::new();
static HOOK_THREAD: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();
static WORKER_THREAD: OnceLock<std::thread::JoinHandle<()>> = OnceLock::new();

#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Modifier key that must be held for mouse drag operations
pub enum MouseDragModifier {
    /// Either Windows key
    #[default]
    Win = 1,
    /// Either Alt key
    Alt = 2,
    /// Either Ctrl key
    Ctrl = 3,
    /// Either Shift key
    Shift = 4,
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Mouse button used for a mouse drag operation
pub enum MouseDragButton {
    /// Left mouse button
    Left = 1,
    /// Right mouse button
    Right = 2,
    /// Middle mouse button
    Middle = 3,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schemars", derive(schemars::JsonSchema))]
/// Hyprland-style modifier + mouse drag to move and resize windows
pub struct MouseDragConfig {
    /// Enable modifier + mouse drag (default: true when this section is present)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Modifier key to hold (default: Win)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modifier: Option<MouseDragModifier>,
    /// Mouse button that moves the window under the cursor (default: Left)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub move_button: Option<MouseDragButton>,
    /// Mouse button that resizes the window under the cursor (default: Right)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resize_button: Option<MouseDragButton>,
    /// Re-tile neighbouring windows while resizing a tiled window (default: true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_resize: Option<bool>,
}

/// Give the drag worker direct access to the window manager, so drags are
/// processed synchronously instead of racing other queued events
pub fn set_window_manager(wm: Arc<Mutex<WindowManager>>) {
    let _ = WM.set(wm);
}

/// Apply (or re-apply on config reload) the mouse drag configuration.
/// The hook is only installed the first time mouse drag is enabled.
pub fn apply(config: Option<&MouseDragConfig>) {
    let enabled = config.is_some_and(|c| c.enabled.unwrap_or(true));

    if let Some(config) = config {
        MODIFIER.store(config.modifier.unwrap_or_default() as u8, Ordering::SeqCst);
        MOVE_BUTTON.store(
            config.move_button.unwrap_or(MouseDragButton::Left) as u8,
            Ordering::SeqCst,
        );
        RESIZE_BUTTON.store(
            config.resize_button.unwrap_or(MouseDragButton::Right) as u8,
            Ordering::SeqCst,
        );
        LIVE_RESIZE.store(config.live_resize.unwrap_or(true), Ordering::SeqCst);
    }

    MOUSE_DRAG_ENABLED.store(enabled, Ordering::SeqCst);

    if enabled {
        start();
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum DragOp {
    Move,
    Resize,
}

#[derive(Copy, Clone, Debug)]
enum DragMessage {
    Begin { hwnd: isize, op: DragOp, x: i32, y: i32 },
    Motion { x: i32, y: i32 },
    End { x: i32, y: i32 },
}

fn channel() -> &'static (Sender<DragMessage>, Receiver<DragMessage>) {
    CHANNEL.get_or_init(crossbeam_channel::unbounded)
}

fn start() {
    ALLOW_INJECTED.get_or_init(|| {
        std::env::var("KOMOREBI_MOUSE_DRAG_ALLOW_INJECTED").is_ok_and(|v| v == "1")
    });

    WORKER_THREAD.get_or_init(|| std::thread::spawn(worker));

    HOOK_THREAD.get_or_init(|| {
        std::thread::spawn(|| unsafe {
            if let Err(error) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
                tracing::error!("failed to install mouse drag hook: {error}");
                return;
            }

            tracing::info!("mouse drag hook installed");

            // A low-level hook is serviced by this thread's message loop.
            // Do not sleep here: any delay adds latency to every mouse event.
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {}
        })
    });
}

// ---------------------------------------------------------------------------
// Hook thread: must stay fast and must never lock the window manager. It only
// decides whether to swallow input and forwards points to the worker.
// ---------------------------------------------------------------------------

thread_local! {
    /// Button (as `MouseDragButton as u8`) driving the active drag, 0 if none
    static ACTIVE_BUTTON: Cell<u8> = const { Cell::new(0) };
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(i32::from(vk.0)) as u16 & 0x8000) != 0 }
}

fn modifier_down() -> bool {
    match MODIFIER.load(Ordering::Relaxed) {
        1 => key_down(VK_LWIN) || key_down(VK_RWIN),
        2 => key_down(VK_MENU),
        3 => key_down(VK_CONTROL),
        4 => key_down(VK_SHIFT),
        _ => false,
    }
}

/// Returns the top-level window under the point if it can be dragged
fn draggable_window_at(pt: POINT) -> Option<HWND> {
    unsafe {
        let hwnd = windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(pt);
        if hwnd.is_invalid() {
            return None;
        }

        let root = GetAncestor(hwnd, GA_ROOT);
        if root.is_invalid() || root == GetDesktopWindow() || root == GetShellWindow() {
            return None;
        }

        // Only real application windows: skips the taskbar, desktop icon
        // host, tooltips and other caption-less surfaces
        let style = GetWindowLongPtrW(root, GWL_STYLE) as u32;
        if style & WS_CHILD.0 != 0 || style & (WS_CAPTION.0 | WS_THICKFRAME.0) == 0 {
            return None;
        }

        if IsZoomed(root).as_bool() {
            return None;
        }

        if let Ok(exe) = Window::from(root.0 as isize).exe()
            && IGNORED_EXES.iter().any(|i| i.eq_ignore_ascii_case(&exe))
        {
            return None;
        }

        Some(root)
    }
}

fn button_for(msg: u32) -> Option<(u8, bool)> {
    match msg {
        WM_LBUTTONDOWN => Some((MouseDragButton::Left as u8, true)),
        WM_LBUTTONUP => Some((MouseDragButton::Left as u8, false)),
        WM_RBUTTONDOWN => Some((MouseDragButton::Right as u8, true)),
        WM_RBUTTONUP => Some((MouseDragButton::Right as u8, false)),
        WM_MBUTTONDOWN => Some((MouseDragButton::Middle as u8, true)),
        WM_MBUTTONUP => Some((MouseDragButton::Middle as u8, false)),
        _ => None,
    }
}

/// Returns true if the event should be swallowed
fn handle_mouse(msg: u32, pt: POINT) -> bool {
    let tx = &channel().0;
    let active = ACTIVE_BUTTON.get();

    if active != 0 {
        if msg == WM_MOUSEMOVE {
            let _ = tx.send(DragMessage::Motion { x: pt.x, y: pt.y });
            // Never swallow motion, or the cursor would freeze
            return false;
        }

        match button_for(msg) {
            Some((button, false)) if button == active => {
                ACTIVE_BUTTON.set(0);
                let _ = tx.send(DragMessage::End { x: pt.x, y: pt.y });
                return true;
            }
            // A press while a drag is active means the release was lost (e.g.
            // the hook was skipped, or a secure desktop appeared). End the
            // stale drag and treat this press as a fresh one below.
            Some((_, true)) => {
                tracing::warn!("mouse drag: release was missed, ending stale drag");
                ACTIVE_BUTTON.set(0);
                let _ = tx.send(DragMessage::End { x: pt.x, y: pt.y });
            }
            _ => return false,
        }
    }

    if !MOUSE_DRAG_ENABLED.load(Ordering::Relaxed) {
        return false;
    }

    let Some((button, true)) = button_for(msg) else {
        return false;
    };

    let op = if button == MOVE_BUTTON.load(Ordering::Relaxed) {
        DragOp::Move
    } else if button == RESIZE_BUTTON.load(Ordering::Relaxed) {
        DragOp::Resize
    } else {
        return false;
    };

    if !modifier_down() {
        return false;
    }

    let Some(hwnd) = draggable_window_at(pt) else {
        let under = unsafe { windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(pt) };
        let window = Window::from(under.0 as isize);
        tracing::info!(
            "mouse drag: nothing draggable at {},{} (hwnd {}, class {}, exe {})",
            pt.x,
            pt.y,
            window.hwnd,
            window.class().unwrap_or_default(),
            window.exe().unwrap_or_default()
        );
        return false;
    };

    ACTIVE_BUTTON.set(button);
    let _ = tx.send(DragMessage::Begin {
        hwnd: hwnd.0 as isize,
        op,
        x: pt.x,
        y: pt.y,
    });

    true
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        // Ignore synthetic input (including komorebi's own focus nudges)
        let injected = info.flags & LLMHF_INJECTED != 0;
        let allow_injected = *ALLOW_INJECTED.get().unwrap_or(&false);
        if (!injected || allow_injected) && handle_mouse(wparam.0 as u32, info.pt) {
            return LRESULT(1);
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Worker thread: focuses, moves and resizes windows, and talks to komorebi.
// ---------------------------------------------------------------------------

/// Which edges of a tiled container have a neighbour that can absorb a resize
#[derive(Copy, Clone, Debug)]
struct ResizableEdges {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
}

/// A tiled container being dragged, as found in komorebi's state
#[derive(Copy, Clone, Debug)]
struct Tiled {
    monitor_idx: usize,
    edges: ResizableEdges,
    /// The container's layout rect when the drag started
    layout: Rect,
}

struct Drag {
    hwnd: isize,
    op: DragOp,
    origin: POINT,
    rect: RECT,
    /// Which horizontal edge a resize drags: -1 left, 1 right, 0 locked
    edge_x: i32,
    /// Which vertical edge a resize drags: -1 top, 1 bottom, 0 locked
    edge_y: i32,
    tiled: Option<Tiled>,
    /// Learned ratio between a resize request and how far the layout edge
    /// actually moved, per axis (it differs between layouts and positions)
    gain_x: f32,
    gain_y: f32,
    last_live_update: Instant,
    /// Global animation setting to restore once the drag is over
    restore_animation: Option<bool>,
}

impl Drag {
    /// Window rect following the cursor, for moves and floating resizes
    fn target_rect(&self, x: i32, y: i32) -> RECT {
        let dx = x - self.origin.x;
        let dy = y - self.origin.y;
        let mut r = self.rect;

        match self.op {
            DragOp::Move => {
                r.left += dx;
                r.right += dx;
                r.top += dy;
                r.bottom += dy;
            }
            DragOp::Resize => {
                match self.edge_x {
                    -1 => r.left = (r.left + dx).min(r.right - MIN_SIZE),
                    1 => r.right = (r.right + dx).max(r.left + MIN_SIZE),
                    _ => {}
                }

                match self.edge_y {
                    -1 => r.top = (r.top + dy).min(r.bottom - MIN_SIZE),
                    1 => r.bottom = (r.bottom + dy).max(r.top + MIN_SIZE),
                    _ => {}
                }
            }
        }

        r
    }

    fn apply_window(&self, x: i32, y: i32, asynchronous: bool) {
        let r = self.target_rect(x, y);
        let mut flags = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER;
        if asynchronous {
            // Never block the drag on a slow or hung application
            flags |= SWP_ASYNCWINDOWPOS;
        }

        let _ = unsafe {
            SetWindowPos(
                HWND(self.hwnd as *mut core::ffi::c_void),
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                flags,
            )
        };
    }

    fn is_tiled_resize(&self) -> bool {
        self.op == DragOp::Resize && self.tiled.is_some()
    }

    /// Drive the tiling layout so the dragged edges sit under the cursor.
    ///
    /// This only ever reads komorebi's own layout rects, never the window's
    /// real size: apps clamp or snap their size (minimum widths, terminal
    /// character grids), which made measuring the window oscillate.
    fn resize_layout(&mut self, x: i32, y: i32) {
        let (Some(tiled), Some(wm)) = (self.tiled, WM.get()) else {
            return;
        };

        let mut wm = wm.lock();
        let dx = x - self.origin.x;
        let dy = y - self.origin.y;

        let horizontal = match self.edge_x {
            -1 => Some((OperationDirection::Left, tiled.layout.left + dx)),
            1 => Some((
                OperationDirection::Right,
                tiled.layout.left + tiled.layout.right + dx,
            )),
            _ => None,
        };

        let vertical = match self.edge_y {
            -1 => Some((OperationDirection::Up, tiled.layout.top + dy)),
            1 => Some((
                OperationDirection::Down,
                tiled.layout.top + tiled.layout.bottom + dy,
            )),
            _ => None,
        };

        if let Some((direction, target)) = horizontal {
            drive_edge(
                &mut wm,
                self.hwnd,
                tiled.monitor_idx,
                direction,
                target,
                &mut self.gain_x,
            );
        }

        if let Some((direction, target)) = vertical {
            drive_edge(
                &mut wm,
                self.hwnd,
                tiled.monitor_idx,
                direction,
                target,
                &mut self.gain_y,
            );
        }
    }

    fn motion(&mut self, x: i32, y: i32) {
        if !self.is_tiled_resize() {
            self.apply_window(x, y, true);
            return;
        }

        if LIVE_RESIZE.load(Ordering::Relaxed)
            && self.last_live_update.elapsed() >= LIVE_RESIZE_INTERVAL
        {
            self.last_live_update = Instant::now();
            self.resize_layout(x, y);
        }
    }
}

/// Current position of one edge of the container holding `hwnd`, in layout
/// coordinates. Also (re)focuses that container, since resize_window acts on
/// the focused container and focus events may arrive mid-drag.
fn layout_edge(
    wm: &mut WindowManager,
    hwnd: isize,
    monitor_idx: usize,
    direction: OperationDirection,
) -> Option<i32> {
    wm.focus_monitor(monitor_idx).ok()?;
    let workspace = wm.focused_workspace_mut().ok()?;
    let idx = workspace.container_idx_for_window(hwnd)?;
    workspace.focus_container(idx);
    let r = workspace.latest_layout.get(idx)?;

    Some(match direction {
        OperationDirection::Left => r.left,
        OperationDirection::Right => r.left + r.right,
        OperationDirection::Up => r.top,
        OperationDirection::Down => r.top + r.bottom,
    })
}

/// Move one edge of a tiled container to `target` (layout coordinates).
///
/// How far a layout edge moves per unit of resize depends on the layout and
/// the container's position in it, so the ratio is measured after each step
/// and used for the next (secant method). This converges in one or two steps
/// instead of overshooting and oscillating.
fn drive_edge(
    wm: &mut WindowManager,
    hwnd: isize,
    monitor_idx: usize,
    direction: OperationDirection,
    target: i32,
    gain: &mut f32,
) {
    // Set once a step had to be undone; from then on steps are bisected
    let mut limit: Option<i32> = None;

    for _ in 0..8 {
        let Some(current) = layout_edge(wm, hwnd, monitor_idx, direction) else {
            return;
        };

        let error = target - current;
        if error.abs() <= 2 {
            return;
        }

        let grows = match direction {
            OperationDirection::Left | OperationDirection::Up => error < 0,
            OperationDirection::Right | OperationDirection::Down => error > 0,
        };
        let sizing = if grows {
            Sizing::Increase
        } else {
            Sizing::Decrease
        };

        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let mut amount = ((error.abs() as f32) / *gain).round().max(1.0) as i32;
        if let Some(limit) = limit {
            amount = amount.min(limit);
        }
        if amount < 2 {
            return;
        }

        let saved = wm
            .focused_workspace()
            .map(|w| w.resize_dimensions.clone())
            .unwrap_or_default();

        if let Err(error) = wm.resize_window(direction, sizing, amount, true) {
            tracing::debug!("mouse drag resize step failed: {error}");
            return;
        }

        // The layout's own bound can't see how adjacent tiles are affected, so
        // check every tile; if any got too small, undo and try a smaller step
        if !layout_is_sane(wm) {
            if let Ok(workspace) = wm.focused_workspace_mut() {
                workspace.resize_dimensions = saved;
            }
            let _ = wm.update_focused_workspace(false, false);
            limit = Some(amount / 2);
            continue;
        }

        let Some(after) = layout_edge(wm, hwnd, monitor_idx, direction) else {
            return;
        };

        let moved = (after - current).abs();
        if moved == 0 {
            // Hit a layout bound; pushing further does nothing
            return;
        }

        #[allow(clippy::cast_precision_loss)]
        {
            *gain = (moved as f32 / amount as f32).clamp(0.05, 8.0);
        }
    }
}

/// Every tile on the focused workspace is at least MIN_SIZE and inside the work area
fn layout_is_sane(wm: &WindowManager) -> bool {
    let Ok(workspace) = wm.focused_workspace() else {
        return true;
    };

    let area = workspace.globals.work_area;
    workspace.latest_layout.iter().all(|r| {
        r.right >= MIN_SIZE
            && r.bottom >= MIN_SIZE
            && r.left >= area.left - 2
            && r.top >= area.top - 2
            && r.left + r.right <= area.left + area.right + 2
            && r.top + r.bottom <= area.top + area.bottom + 2
    })
}

/// Pick which edge of an axis to drag: the one nearest the cursor, unless it
/// has no neighbour to push against, in which case the opposite edge is used
const fn pick_edge(near_negative: bool, negative_ok: bool, positive_ok: bool) -> i32 {
    match (near_negative, negative_ok, positive_ok) {
        (true, true, _) | (false, true, false) => -1,
        (false, _, true) | (true, false, true) => 1,
        _ => 0,
    }
}

/// If `hwnd` is a tiled container on the focused workspace of its monitor,
/// focus that monitor and container in komorebi's state (so the drag acts on
/// the right container, without waiting for the OS focus event) and return
/// its layout rect and which of its edges can be resized.
fn prepare_tiled(wm: &mut WindowManager, hwnd: isize) -> Option<Tiled> {
    let (monitor_idx, container_idx) = wm.monitors().iter().enumerate().find_map(|(m, monitor)| {
        let workspace = monitor.focused_workspace()?;
        if !workspace.tile
            || workspace.monocle_container.is_some()
            || workspace.maximized_window.is_some()
        {
            return None;
        }

        workspace.container_idx_for_window(hwnd).map(|c| (m, c))
    })?;

    wm.focus_monitor(monitor_idx).ok()?;
    let workspace = wm.focused_workspace_mut().ok()?;
    workspace.focus_container(container_idx);
    workspace.layer = WorkspaceLayer::Tiling;

    let layout = *workspace.latest_layout.get(container_idx)?;

    // Only the default layouts support resizing
    if !matches!(workspace.layout, Layout::Default(_)) {
        return Some(Tiled {
            monitor_idx,
            layout,
            edges: ResizableEdges {
                left: false,
                right: false,
                up: false,
                down: false,
            },
        });
    }

    let len = NonZeroUsize::new(workspace.containers().len())?;
    let can = |direction: OperationDirection| {
        direction
            .destination(
                workspace.layout.as_boxed_direction().as_ref(),
                workspace.layout_flip,
                container_idx,
                len,
                workspace.layout_options,
            )
            .is_some()
    };

    Some(Tiled {
        monitor_idx,
        layout,
        edges: ResizableEdges {
            left: can(OperationDirection::Left),
            right: can(OperationDirection::Right),
            up: can(OperationDirection::Up),
            down: can(OperationDirection::Down),
        },
    })
}

/// Run an event through the window manager synchronously, falling back to the
/// event channel if the window manager handle has not been registered
fn process(event: WindowManagerEvent) {
    match WM.get() {
        Some(wm) => {
            if let Err(error) = wm.lock().process_event(event) {
                tracing::warn!("mouse drag event failed: {error}");
            }
        }
        None => {
            if let Err(error) = winevent_listener::event_tx().send(event) {
                tracing::error!("mouse drag could not send event: {error}");
            }
        }
    }
}

fn tap_dummy_key() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_DUMMY),
                dwFlags: flags,
                ..Default::default()
            },
        },
    };

    let inputs = [key(Default::default()), key(KEYEVENTF_KEYUP)];
    unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
}

fn begin(hwnd: isize, op: DragOp, x: i32, y: i32) -> Option<Drag> {
    if MODIFIER.load(Ordering::Relaxed) == MouseDragModifier::Win as u8 {
        tap_dummy_key();
    }

    let mut rect = RECT::default();
    unsafe { GetWindowRect(HWND(hwnd as *mut core::ffi::c_void), &mut rect) }.ok()?;

    let tiled = WM.get().and_then(|wm| prepare_tiled(&mut wm.lock(), hwnd));

    let near_left = x < (rect.left + rect.right) / 2;
    let near_top = y < (rect.top + rect.bottom) / 2;

    let (edge_x, edge_y) = match tiled {
        Some(t) => (
            pick_edge(near_left, t.edges.left, t.edges.right),
            pick_edge(near_top, t.edges.up, t.edges.down),
        ),
        // Floating windows: resize from the nearest corner, like Hyprland
        None => (
            if near_left { -1 } else { 1 },
            if near_top { -1 } else { 1 },
        ),
    };

    let window = Window::from(hwnd);
    if let Err(error) = window.focus(false) {
        tracing::warn!("mouse drag could not focus window: {error}");
    }

    let drag = Drag {
        hwnd,
        op,
        origin: POINT { x, y },
        rect,
        edge_x,
        edge_y,
        tiled,
        // Mouse-resize deltas are halved by the default layouts
        gain_x: 0.5,
        gain_y: 0.5,
        last_live_update: Instant::now(),
        // Animating every live resize step would make the layout lag the cursor
        restore_animation: None,
    };

    let mut drag = drag;
    if drag.is_tiled_resize() {
        drag.restore_animation = Some(ANIMATION_ENABLED_GLOBAL.swap(false, Ordering::SeqCst));
    } else {
        // Moves (and floating resizes) go through komorebi's native drag
        // handling, which swaps containers / transfers across monitors on drop
        process(WindowManagerEvent::MoveResizeStart(
            WinEvent::SystemMoveSizeStart,
            window,
        ));
    }

    tracing::info!(
        "mouse drag {op:?} started for hwnd {hwnd} (tiled: {}, edges: {edge_x},{edge_y})",
        tiled.is_some()
    );

    Some(drag)
}

fn end(mut drag: Drag, x: i32, y: i32) {
    if drag.is_tiled_resize() {
        // Final, exact step; the layout already placed every window
        drag.resize_layout(x, y);
    } else {
        drag.apply_window(x, y, false);

        if drag.op == DragOp::Move {
            MOVE_HINT.store(true, Ordering::SeqCst);
        }

        process(WindowManagerEvent::MoveResizeEnd(
            WinEvent::SystemMoveSizeEnd,
            Window::from(drag.hwnd),
        ));

        MOVE_HINT.store(false, Ordering::SeqCst);
    }

    if let Some(enabled) = drag.restore_animation {
        ANIMATION_ENABLED_GLOBAL.store(enabled, Ordering::SeqCst);
    }
}

fn worker() {
    let rx = channel().1.clone();
    let mut drag: Option<Drag> = None;
    // A message pulled off the queue while coalescing motion, handled next
    let mut pending: Option<DragMessage> = None;

    loop {
        let message = match pending.take() {
            Some(message) => message,
            None => match rx.recv() {
                Ok(message) => message,
                Err(_) => break,
            },
        };

        match message {
            DragMessage::Begin { hwnd, op, x, y } => {
                if let Some(previous) = drag.take() {
                    end(previous, x, y);
                }
                drag = begin(hwnd, op, x, y);
            }
            DragMessage::Motion { mut x, mut y } => {
                // Coalesce queued motion so the window tracks the latest position
                while let Ok(next) = rx.try_recv() {
                    if let DragMessage::Motion { x: nx, y: ny } = next {
                        (x, y) = (nx, ny);
                    } else {
                        pending = Some(next);
                        break;
                    }
                }

                if let Some(d) = drag.as_mut() {
                    d.motion(x, y);
                }
            }
            DragMessage::End { x, y } => {
                if let Some(d) = drag.take() {
                    end(d, x, y);
                }
            }
        }
    }
}
