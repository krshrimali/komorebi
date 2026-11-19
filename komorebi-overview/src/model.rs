//! Snapshot of komorebi's state for the monitor the overview is shown on.

use serde_json::Value;

/// Absolute screen rectangle in physical pixels (right/bottom exclusive)
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub l: i32,
    pub t: i32,
    pub r: i32,
    pub b: i32,
}

impl Rect {
    pub fn w(&self) -> i32 {
        self.r - self.l
    }

    pub fn h(&self) -> i32 {
        self.b - self.t
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }

    /// komorebi serialises rects as left/top/width/height
    fn from_komorebi(v: &Value) -> Option<Self> {
        let n = |k: &str| v.get(k).and_then(Value::as_i64).map(|n| n as i32);
        let (l, t, w, h) = (n("left")?, n("top")?, n("right")?, n("bottom")?);
        Some(Self {
            l,
            t,
            r: l + w,
            b: t + h,
        })
    }
}

#[derive(Default)]
pub struct Workspace {
    pub name: String,
    /// Windows bottom-to-top: tiled, monocle, maximized, then floating
    pub windows: Vec<isize>,
}

#[derive(Default)]
pub struct Monitor {
    pub index: usize,
    pub rect: Rect,
    pub focused_workspace: usize,
    pub workspaces: Vec<Workspace>,
}

fn ring(v: &Value) -> (&[Value], usize) {
    let elements = v
        .get("elements")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let focused = v.get("focused").and_then(Value::as_u64).unwrap_or(0) as usize;
    (elements, focused)
}

fn hwnd(window: &Value) -> Option<isize> {
    window
        .get("hwnd")
        .and_then(Value::as_i64)
        .map(|h| h as isize)
}

/// The visible window of a container (its focused one, for stacks)
fn container_window(container: &Value) -> Option<isize> {
    let (windows, focused) = ring(container.get("windows")?);
    windows.get(focused).or(windows.first()).and_then(hwnd)
}

fn workspace(v: &Value, index: usize) -> Workspace {
    let mut windows = vec![];

    if let Some(containers) = v.get("containers") {
        windows.extend(ring(containers).0.iter().filter_map(container_window));
    }
    if let Some(monocle) = v.get("monocle_container").filter(|m| !m.is_null()) {
        windows.extend(container_window(monocle));
    }
    if let Some(maximized) = v.get("maximized_window").filter(|m| !m.is_null()) {
        windows.extend(hwnd(maximized));
    }
    if let Some(floating) = v.get("floating_windows") {
        windows.extend(ring(floating).0.iter().filter_map(hwnd));
    }

    let name = v
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| (index + 1).to_string());

    Workspace { name, windows }
}

/// The komorebi monitor containing the point (x, y)
pub fn monitor_at(x: i32, y: i32) -> Result<Monitor, String> {
    let json = komorebi_client::send_query(&komorebi_client::SocketMessage::State)
        .map_err(|e| format!("komorebi is not running: {e}"))?;
    let state: Value = serde_json::from_str(&json).map_err(|e| e.to_string())?;

    let (monitors, focused_monitor) = ring(&state["monitors"]);
    let rect_of = |m: &Value| Rect::from_komorebi(&m["size"]).unwrap_or_default();

    let index = monitors
        .iter()
        .position(|m| rect_of(m).contains(x, y))
        .unwrap_or(focused_monitor);
    let monitor = monitors.get(index).ok_or("no monitors")?;

    let (workspaces, focused_workspace) = ring(&monitor["workspaces"]);

    Ok(Monitor {
        index,
        rect: rect_of(monitor),
        focused_workspace,
        workspaces: workspaces
            .iter()
            .enumerate()
            .map(|(i, w)| workspace(w, i))
            .collect(),
    })
}

/// Switch to a workspace on a monitor
pub fn focus(monitor: usize, workspace: usize) {
    let _ = komorebi_client::send_message(
        &komorebi_client::SocketMessage::FocusMonitorWorkspaceNumber(monitor, workspace),
    );
}
