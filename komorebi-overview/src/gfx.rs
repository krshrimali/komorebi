//! Thin Direct2D / DirectWrite / WIC wrapper for the overview.

use crate::capture::Pixels;
use windows::Win32::Foundation::GENERIC_READ;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U;
use windows::Win32::Graphics::Direct2D::Common::D2D1_ALPHA_MODE_PREMULTIPLIED;
use windows::Win32::Graphics::Direct2D::Common::D2D1_ALPHA_MODE_IGNORE;
use windows::Win32::Graphics::Direct2D::Common::D2D1_BORDER_MODE_HARD;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COMPOSITE_MODE_SOURCE_OVER;
use windows::Win32::Graphics::Direct2D::Common::D2D1_PIXEL_FORMAT;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_UNKNOWN;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::CLSCTX_INPROC_SERVER;
use windows::Win32::System::Com::CoCreateInstance;
use windows::core::HSTRING;
use windows::core::Interface;
use windows::core::w;
use windows_numerics::Matrix3x2;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub l: f32,
    pub t: f32,
    pub r: f32,
    pub b: f32,
}

impl RectF {
    pub fn new(l: f32, t: f32, w: f32, h: f32) -> Self {
        Self {
            l,
            t,
            r: l + w,
            b: t + h,
        }
    }
    pub fn w(&self) -> f32 {
        self.r - self.l
    }
    pub fn h(&self) -> f32 {
        self.b - self.t
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.l && x < self.r && y >= self.t && y < self.b
    }
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        let m = |x: f32, y: f32| x + (y - x) * t;
        Self {
            l: m(a.l, b.l),
            t: m(a.t, b.t),
            r: m(a.r, b.r),
            b: m(a.b, b.b),
        }
    }
    /// Scale around the centre
    pub fn scaled(&self, s: f32) -> Self {
        let (cx, cy) = ((self.l + self.r) / 2.0, (self.t + self.b) / 2.0);
        let (hw, hh) = (self.w() * s / 2.0, self.h() * s / 2.0);
        Self {
            l: cx - hw,
            t: cy - hh,
            r: cx + hw,
            b: cy + hh,
        }
    }
    pub fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F {
            left: self.l,
            top: self.t,
            right: self.r,
            bottom: self.b,
        }
    }
}

pub fn rgba(hex: u32, a: f32) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: ((hex >> 16) & 0xff) as f32 / 255.0,
        g: ((hex >> 8) & 0xff) as f32 / 255.0,
        b: (hex & 0xff) as f32 / 255.0,
        a,
    }
}

pub struct Gfx {
    pub rt: ID2D1HwndRenderTarget,
    dc: ID2D1DeviceContext,
    dwrite: IDWriteFactory,
    brush: ID2D1SolidColorBrush,
    blur: Option<ID2D1Effect>,
}

impl Gfx {
    pub fn new(hwnd: HWND, w: u32, h: u32) -> windows::core::Result<Self> {
        unsafe {
            let factory: ID2D1Factory1 =
                D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;

            let props = D2D1_RENDER_TARGET_PROPERTIES {
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_UNKNOWN,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                // 1 DIP == 1 physical pixel; the process is per-monitor DPI aware
                dpiX: 96.0,
                dpiY: 96.0,
                ..Default::default()
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U {
                    width: w,
                    height: h,
                },
                // Default present options wait for vblank: smooth, tear-free animation
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = factory.CreateHwndRenderTarget(&props, &hwnd_props)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            let dc: ID2D1DeviceContext = rt.cast()?;
            let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let brush = rt.CreateSolidColorBrush(&rgba(0xffffff, 1.0), None)?;

            Ok(Self {
                rt,
                dc,
                dwrite,
                brush,
                blur: None,
            })
        }
    }

    pub fn bitmap(&self, pixels: &Pixels) -> Option<ID2D1Bitmap> {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                // GDI leaves alpha undefined: treat captures as opaque
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        unsafe {
            self.rt
                .CreateBitmap(
                    D2D_SIZE_U {
                        width: pixels.w,
                        height: pixels.h,
                    },
                    Some(pixels.data.as_ptr() as *const _),
                    pixels.w * 4,
                    &props,
                )
                .ok()
        }
    }

    /// Decode an image file (the desktop wallpaper) through WIC
    pub fn image_file(&self, path: &str) -> Option<ID2D1Bitmap> {
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
            let decoder = wic
                .CreateDecoderFromFilename(
                    &HSTRING::from(path),
                    None,
                    GENERIC_READ,
                    WICDecodeMetadataCacheOnDemand,
                )
                .ok()?;
            let frame = decoder.GetFrame(0).ok()?;
            let converter = wic.CreateFormatConverter().ok()?;
            converter
                .Initialize(
                    &frame,
                    &GUID_WICPixelFormat32bppPBGRA,
                    WICBitmapDitherTypeNone,
                    None,
                    0.0,
                    WICBitmapPaletteTypeMedianCut,
                )
                .ok()?;
            self.rt.CreateBitmapFromWicBitmap(&converter, None).ok()
        }
    }

    pub fn begin(&self) {
        unsafe {
            self.rt.BeginDraw();
            self.rt.Clear(Some(&rgba(0x0b0d12, 1.0)));
        }
    }

    /// False when the device was lost and everything must be recreated
    pub fn end(&self) -> bool {
        unsafe { self.rt.EndDraw(None, None) }.is_ok()
    }

    pub fn resize(&self, w: u32, h: u32) {
        unsafe {
            let _ = self.rt.Resize(&D2D_SIZE_U {
                width: w,
                height: h,
            });
        }
    }

    /// The blur input changes on every open
    pub fn reset_blur(&mut self) {
        self.blur = None;
    }

    pub fn fill(&self, rect: RectF, radius: f32, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rect.d2d(),
                    radiusX: radius,
                    radiusY: radius,
                },
                &self.brush,
            );
        }
    }

    pub fn stroke(&self, rect: RectF, radius: f32, width: f32, color: D2D1_COLOR_F) {
        unsafe {
            self.brush.SetColor(&color);
            self.rt.DrawRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rect.d2d(),
                    radiusX: radius,
                    radiusY: radius,
                },
                &self.brush,
                width,
                None,
            );
        }
    }

    /// Draw `bitmap` stretched over `dest`, clipped to a rounded rectangle.
    /// `src` selects the part of the bitmap to show (None = all of it).
    pub fn image_rounded(
        &self,
        bitmap: &ID2D1Bitmap,
        src: Option<RectF>,
        dest: RectF,
        radius: f32,
        opacity: f32,
    ) {
        unsafe {
            let size = bitmap.GetSize();
            let src = src.unwrap_or(RectF::new(0.0, 0.0, size.width, size.height));
            let (sx, sy) = (dest.w() / src.w(), dest.h() / src.h());
            let brush_props = D2D1_BRUSH_PROPERTIES {
                opacity,
                transform: Matrix3x2 {
                    M11: sx,
                    M12: 0.0,
                    M21: 0.0,
                    M22: sy,
                    M31: dest.l - src.l * sx,
                    M32: dest.t - src.t * sy,
                },
            };
            let bitmap_props = D2D1_BITMAP_BRUSH_PROPERTIES {
                extendModeX: D2D1_EXTEND_MODE_CLAMP,
                extendModeY: D2D1_EXTEND_MODE_CLAMP,
                interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
            };
            if let Ok(brush) =
                self.rt
                    .CreateBitmapBrush(bitmap, Some(&bitmap_props), Some(&brush_props))
            {
                self.rt.FillRoundedRectangle(
                    &D2D1_ROUNDED_RECT {
                        rect: dest.d2d(),
                        radiusX: radius,
                        radiusY: radius,
                    },
                    &brush,
                );
            }
        }
    }

    /// High-quality downscaled bitmap draw (no rounding)
    pub fn image(&self, bitmap: &ID2D1Bitmap, dest: RectF, opacity: f32) {
        unsafe {
            self.dc.DrawBitmap(
                bitmap,
                Some(&dest.d2d()),
                opacity,
                D2D1_INTERPOLATION_MODE_HIGH_QUALITY_CUBIC,
                None,
                None,
            );
        }
    }

    pub fn clip(&self, rect: RectF) {
        unsafe {
            self.rt
                .PushAxisAlignedClip(&rect.d2d(), D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        }
    }

    pub fn unclip(&self) {
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    /// Gaussian-blurred copy of `bitmap` drawn at the origin
    pub fn blurred(&mut self, bitmap: &ID2D1Bitmap, sigma: f32) {
        unsafe {
            if self.blur.is_none() {
                let Ok(effect) = self.dc.CreateEffect(&CLSID_D2D1GaussianBlur) else {
                    return;
                };
                let _ = effect.SetValue(
                    D2D1_GAUSSIANBLUR_PROP_STANDARD_DEVIATION.0 as u32,
                    D2D1_PROPERTY_TYPE_FLOAT,
                    &sigma.to_le_bytes(),
                );
                let _ = effect.SetValue(
                    D2D1_GAUSSIANBLUR_PROP_BORDER_MODE.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &(D2D1_BORDER_MODE_HARD.0 as u32).to_le_bytes(),
                );
                let _ = effect.SetValue(
                    D2D1_GAUSSIANBLUR_PROP_OPTIMIZATION.0 as u32,
                    D2D1_PROPERTY_TYPE_ENUM,
                    &(D2D1_GAUSSIANBLUR_OPTIMIZATION_SPEED.0 as u32).to_le_bytes(),
                );
                effect.SetInput(0, bitmap, true);
                self.blur = Some(effect);
            }

            if let Some(effect) = &self.blur
                && let Ok(output) = effect.GetOutput()
            {
                self.dc.DrawImage(
                    &output,
                    None,
                    None,
                    D2D1_INTERPOLATION_MODE_LINEAR,
                    D2D1_COMPOSITE_MODE_SOURCE_OVER,
                );
            }
        }
    }

    fn format(&self, size: f32, bold: bool) -> Option<IDWriteTextFormat> {
        unsafe {
            let format = self
                .dwrite
                .CreateTextFormat(
                    w!("Segoe UI Variable Display"),
                    None,
                    if bold {
                        DWRITE_FONT_WEIGHT_SEMI_BOLD
                    } else {
                        DWRITE_FONT_WEIGHT_NORMAL
                    },
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!("en-us"),
                )
                .ok()?;
            let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
            Some(format)
        }
    }

    /// Width of `text` at `size`
    pub fn measure(&self, text: &str, size: f32, bold: bool) -> f32 {
        unsafe {
            let Some(format) = self.format(size, bold) else {
                return 0.0;
            };
            let utf16: Vec<u16> = text.encode_utf16().collect();
            let Ok(layout) = self.dwrite.CreateTextLayout(&utf16, &format, 10000.0, 1000.0)
            else {
                return 0.0;
            };
            let mut metrics = DWRITE_TEXT_METRICS::default();
            let _ = layout.GetMetrics(&mut metrics);
            metrics.widthIncludingTrailingWhitespace
        }
    }

    /// Text centred in `rect` (ellipsised by clipping)
    pub fn text(&self, text: &str, rect: RectF, size: f32, bold: bool, color: D2D1_COLOR_F) {
        unsafe {
            let Some(format) = self.format(size, bold) else {
                return;
            };
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            self.brush.SetColor(&color);
            let utf16: Vec<u16> = text.encode_utf16().collect();
            self.rt.DrawText(
                &utf16,
                &format,
                &rect.d2d(),
                &self.brush,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
                DWRITE_MEASURING_MODE_NATURAL,
            );
        }
    }
}

/// Current desktop wallpaper path, if any
pub fn wallpaper_path() -> Option<String> {
    use windows::Win32::UI::WindowsAndMessaging::SPI_GETDESKWALLPAPER;
    use windows::Win32::UI::WindowsAndMessaging::SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS;
    use windows::Win32::UI::WindowsAndMessaging::SystemParametersInfoW;

    let mut buf = [0u16; 512];
    unsafe {
        SystemParametersInfoW(
            SPI_GETDESKWALLPAPER,
            buf.len() as u32,
            Some(buf.as_mut_ptr() as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let path = String::from_utf16_lossy(&buf[..len]);
    (!path.is_empty() && std::path::Path::new(&path).exists()).then_some(path)
}

