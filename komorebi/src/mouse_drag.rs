//! Hyprland-style modifier + mouse drag to move and resize windows.
//!
//! A low-level mouse hook watches for `modifier + button` presses. The press
//! and release are swallowed so the click never reaches the application, and a
//! worker thread moves/resizes the window under the cursor as the mouse moves.
//!
//! At the start and end of a drag, synthetic `MoveResizeStart` and
//! `MoveResizeEnd` events are pushed into komorebi's normal event channel, so
//! the existing logic that handles native title-bar drags (container swaps,
//! cross-monitor moves, resize ratio updates) is reused unchanged.

use std::cell::Cell;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use crossbeam_channel::Receiver;
use crossbeam_channel::Sender;
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
use crate::window_manager_event::WindowManagerEvent;
use crate::winevent::WinEvent;
use crate::winevent_listener;

/// Smallest width/height a window can be resized down to with the mouse
const MIN_SIZE: i32 = 100;

/// An unassigned virtual key, tapped while Win is held so that releasing Win
/// after a drag does not open the Start menu
const VK_DUMMY: u16 = 0xE8;

pub static MOUSE_DRAG_ENABLED: AtomicBool = AtomicBool::new(false);
static MODIFIER: AtomicU8 = AtomicU8::new(MouseDragModifier::Win as u8);
static MOVE_BUTTON: AtomicU8 = AtomicU8::new(MouseDragButton::Left as u8);
static RESIZE_BUTTON: AtomicU8 = AtomicU8::new(MouseDragButton::Right as u8);

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
    /// Mouse button that resizes the window under the cursor from the nearest corner (default: Right)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resize_button: Option<MouseDragButton>,
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
// Hook thread: must stay fast, so it only decides whether to swallow input
// and forwards points to the worker.
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

        // Only real application windows: skips the taskbar, bars, desktop
        // icon host, tooltips and other caption-less surfaces
        let style = GetWindowLongPtrW(root, GWL_STYLE) as u32;
        if style & WS_CHILD.0 != 0 || style & (WS_CAPTION.0 | WS_THICKFRAME.0) == 0 {
            return None;
        }

        if IsZoomed(root).as_bool() {
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

        if let Some((button, false)) = button_for(msg)
            && button == active
        {
            ACTIVE_BUTTON.set(0);
            let _ = tx.send(DragMessage::End { x: pt.x, y: pt.y });
            return true;
        }

        return false;
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
        // Ignore synthetic input (including our own and komorebi's focus nudges)
        if info.flags & LLMHF_INJECTED == 0 && handle_mouse(wparam.0 as u32, info.pt) {
            return LRESULT(1);
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Worker thread: focuses, moves and resizes windows, and talks to komorebi.
// ---------------------------------------------------------------------------

struct Drag {
    hwnd: isize,
    op: DragOp,
    origin: POINT,
    rect: RECT,
    /// Which horizontal edge a resize drags: -1 left, 1 right
    edge_x: i32,
    /// Which vertical edge a resize drags: -1 top, 1 bottom
    edge_y: i32,
}

impl Drag {
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
                if self.edge_x < 0 {
                    r.left = (r.left + dx).min(r.right - MIN_SIZE);
                } else {
                    r.right = (r.right + dx).max(r.left + MIN_SIZE);
                }

                if self.edge_y < 0 {
                    r.top = (r.top + dy).min(r.bottom - MIN_SIZE);
                } else {
                    r.bottom = (r.bottom + dy).max(r.top + MIN_SIZE);
                }
            }
        }

        r
    }

    fn apply(&self, x: i32, y: i32, asynchronous: bool) {
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

fn send_event(event: WindowManagerEvent) {
    if let Err(error) = winevent_listener::event_tx().send(event) {
        tracing::error!("mouse drag could not send event: {error}");
    }
}

fn worker() {
    let rx = channel().1.clone();
    let mut drag: Option<Drag> = None;

    while let Ok(mut message) = rx.recv() {
        // Coalesce queued motion so the window tracks the latest cursor position
        if matches!(message, DragMessage::Motion { .. }) {
            while let Ok(next) = rx.try_recv() {
                if matches!(next, DragMessage::Motion { .. }) {
                    message = next;
                } else {
                    if let DragMessage::Motion { x, y } = message
                        && let Some(d) = &drag
                    {
                        d.apply(x, y, true);
                    }
                    message = next;
                    break;
                }
            }
        }

        match message {
            DragMessage::Begin { hwnd, op, x, y } => {
                if MODIFIER.load(Ordering::Relaxed) == MouseDragModifier::Win as u8 {
                    tap_dummy_key();
                }

                let mut rect = RECT::default();
                if unsafe { GetWindowRect(HWND(hwnd as *mut core::ffi::c_void), &mut rect) }
                    .is_err()
                {
                    drag = None;
                    continue;
                }

                // Resize from the corner nearest to the cursor, like Hyprland
                let edge_x = if x < (rect.left + rect.right) / 2 { -1 } else { 1 };
                let edge_y = if y < (rect.top + rect.bottom) / 2 { -1 } else { 1 };

                let window = Window::from(hwnd);
                if let Err(error) = window.focus(false) {
                    tracing::warn!("mouse drag could not focus window: {error}");
                }

                send_event(WindowManagerEvent::MoveResizeStart(
                    WinEvent::SystemMoveSizeStart,
                    window,
                ));

                tracing::info!("mouse drag {op:?} started for hwnd {hwnd}");

                drag = Some(Drag {
                    hwnd,
                    op,
                    origin: POINT { x, y },
                    rect,
                    edge_x,
                    edge_y,
                });
            }
            DragMessage::Motion { x, y } => {
                if let Some(d) = &drag {
                    d.apply(x, y, true);
                }
            }
            DragMessage::End { x, y } => {
                if let Some(d) = drag.take() {
                    d.apply(x, y, false);
                    send_event(WindowManagerEvent::MoveResizeEnd(
                        WinEvent::SystemMoveSizeEnd,
                        Window::from(d.hwnd),
                    ));
                }
            }
        }
    }
}
