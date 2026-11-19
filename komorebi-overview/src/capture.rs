//! Pixel captures: the visible monitor (for the current workspace) and
//! individual windows (for workspaces that are currently hidden/cloaked).

use crate::model::Rect;
use windows::Win32::Foundation::HWND;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Dwm::DWMWA_EXTENDED_FRAME_BOUNDS;
use windows::Win32::Graphics::Dwm::DwmGetWindowAttribute;
use windows::Win32::Graphics::Gdi::BI_RGB;
use windows::Win32::Graphics::Gdi::BITMAPINFO;
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::Graphics::Gdi::BitBlt;
use windows::Win32::Graphics::Gdi::CAPTUREBLT;
use windows::Win32::Graphics::Gdi::CreateCompatibleDC;
use windows::Win32::Graphics::Gdi::CreateDIBSection;
use windows::Win32::Graphics::Gdi::DIB_RGB_COLORS;
use windows::Win32::Graphics::Gdi::DeleteDC;
use windows::Win32::Graphics::Gdi::DeleteObject;
use windows::Win32::Graphics::Gdi::GetDC;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::Gdi::ReleaseDC;
use windows::Win32::Graphics::Gdi::SRCCOPY;
use windows::Win32::Graphics::Gdi::SelectObject;
use windows::Win32::Storage::Xps::PRINT_WINDOW_FLAGS;
use windows::Win32::Storage::Xps::PrintWindow;
use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
use windows::Win32::UI::WindowsAndMessaging::GetWindowTextW;
use windows::Win32::UI::WindowsAndMessaging::IsHungAppWindow;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;

/// PW_RENDERFULLCONTENT: renders DirectComposition content and works for cloaked windows
const PW_RENDERFULLCONTENT: u32 = 2;

/// Opaque, top-down BGRA pixels
pub struct Pixels {
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}

pub struct WindowShot {
    pub hwnd: isize,
    /// Visible frame on screen (without the invisible resize borders)
    pub frame: Rect,
    pub title: String,
    /// None when the app cannot be captured (e.g. exclusive-fullscreen games)
    pub pixels: Option<Pixels>,
}

fn hwnd(h: isize) -> HWND {
    HWND(h as *mut core::ffi::c_void)
}

fn to_rect(r: RECT) -> Rect {
    Rect {
        l: r.left,
        t: r.top,
        r: r.right,
        b: r.bottom,
    }
}

/// Visible frame of a window; falls back to the window rect
pub fn frame(h: isize) -> Option<Rect> {
    let mut r = RECT::default();
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd(h),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut r as *mut RECT as *mut _,
            size_of::<RECT>() as u32,
        )
    }
    .is_ok();

    if !ok {
        unsafe { GetWindowRect(hwnd(h), &mut r) }.ok()?;
    }

    let rect = to_rect(r);
    (rect.w() > 0 && rect.h() > 0).then_some(rect)
}

pub fn title(h: isize) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(hwnd(h), &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

/// A 32bpp top-down DIB section selected into a memory DC
struct Dib {
    dc: HDC,
    bitmap: windows::Win32::Graphics::Gdi::HBITMAP,
    old: windows::Win32::Graphics::Gdi::HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Dib {
    fn new(w: i32, h: i32) -> Option<Self> {
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            ReleaseDC(None, screen);

            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };

            let mut bits = core::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            else {
                let _ = DeleteDC(dc);
                return None;
            };
            let old = SelectObject(dc, bitmap.into());

            Some(Self {
                dc,
                bitmap,
                old,
                bits: bits as *mut u8,
                w,
                h,
            })
        }
    }

    /// Copy a sub-rectangle out as opaque pixels
    fn crop(&self, x: i32, y: i32, w: i32, h: i32) -> Pixels {
        let (x, y) = (x.clamp(0, self.w), y.clamp(0, self.h));
        let (w, h) = (w.min(self.w - x).max(1), h.min(self.h - y).max(1));
        let stride = self.w as usize * 4;
        let mut data = Vec::with_capacity(w as usize * h as usize * 4);

        for row in y..y + h {
            let start = row as usize * stride + x as usize * 4;
            let line = unsafe { core::slice::from_raw_parts(self.bits.add(start), w as usize * 4) };
            data.extend_from_slice(line);
        }

        Pixels {
            w: w as u32,
            h: h as u32,
            data,
        }
    }

    /// True when (a sample of) the bitmap is a single colour, i.e. nothing was drawn
    fn is_blank(&self) -> bool {
        let pixels = (self.w * self.h) as usize;
        let at = |i: usize| unsafe { *(self.bits as *const u32).add(i) } & 0x00ff_ffff;
        let first = at(0);
        (0..pixels)
            .step_by((pixels / 997).max(1))
            .all(|i| at(i) == first)
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// What is on screen right now inside `rect`
pub fn screen(rect: Rect) -> Option<Pixels> {
    let dib = Dib::new(rect.w(), rect.h())?;
    unsafe {
        let screen = GetDC(None);
        let ok = BitBlt(
            dib.dc,
            0,
            0,
            rect.w(),
            rect.h(),
            Some(screen),
            rect.l,
            rect.t,
            SRCCOPY | CAPTUREBLT,
        );
        ReleaseDC(None, screen);
        ok.ok()?;
    }
    Some(dib.crop(0, 0, rect.w(), rect.h()))
}

/// Render a (possibly cloaked) window into pixels, cropped to its visible frame
pub fn window(h: isize) -> Option<WindowShot> {
    unsafe {
        if !IsWindow(Some(hwnd(h))).as_bool() {
            return None;
        }
    }

    let frame = frame(h)?;
    let title = title(h);

    // Never block on (or poke) a hung application
    let pixels = if unsafe { IsHungAppWindow(hwnd(h)) }.as_bool() {
        None
    } else {
        capture_window(h, frame)
    };

    Some(WindowShot {
        hwnd: h,
        frame,
        title,
        pixels,
    })
}

fn capture_window(h: isize, frame: Rect) -> Option<Pixels> {
    let mut wr = RECT::default();
    unsafe { GetWindowRect(hwnd(h), &mut wr) }.ok()?;
    let window = to_rect(wr);
    if window.w() <= 0 || window.h() <= 0 || window.w() > 16384 || window.h() > 16384 {
        return None;
    }

    let dib = Dib::new(window.w(), window.h())?;
    let printed = unsafe { PrintWindow(hwnd(h), dib.dc, PRINT_WINDOW_FLAGS(PW_RENDERFULLCONTENT)) };
    if !printed.as_bool() || dib.is_blank() {
        return None;
    }

    Some(dib.crop(frame.l - window.l, frame.t - window.t, frame.w(), frame.h()))
}
