//! Drawing: Direct2D in software (no GPU driver loaded: the windows are light, and software rendering presents reliably
//! into a DWM backdrop) and DirectWrite text, in device-independent pixels.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use windows::Win32::Foundation::{D2DERR_RECREATE_TARGET, HWND, RECT};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject,
    GetDC, HBITMAP, HDC, HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject,
};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::core::{Interface, w};
use windows_numerics::{Matrix3x2, Vector2};

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const fn argb(v: u32) -> Color {
        Color {
            a: ((v >> 24) & 0xFF) as f32 / 255.0,
            r: ((v >> 16) & 0xFF) as f32 / 255.0,
            g: ((v >> 8) & 0xFF) as f32 / 255.0,
            b: (v & 0xFF) as f32 / 255.0,
        }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
    }

    pub fn with_alpha(self, a: f32) -> Color {
        Color { a: self.a * a, ..self }
    }

    pub const TRANSPARENT: Color = Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };

    fn d2d(self) -> D2D1_COLOR_F {
        D2D1_COLOR_F { r: self.r, g: self.g, b: self.b, a: self.a }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    /// Shrunk by (left, top, right, bottom).
    pub fn inset(&self, l: f32, t: f32, r: f32, b: f32) -> Rect {
        Rect { x: self.x + l, y: self.y + t, w: (self.w - l - r).max(0.0), h: (self.h - t - b).max(0.0) }
    }
    fn d2d(&self) -> D2D_RECT_F {
        D2D_RECT_F { left: self.x, top: self.y, right: self.x + self.w, bottom: self.y + self.h }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Font {
    Text,
    Display,
    Icons,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Align {
    Start,
    Center,
    End,
}

#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub font: Font,
    pub size: f32,
    pub weight: u32,
    pub color: Color,
    pub align: Align,
    pub valign: Align,
    pub wrap: bool,
    pub ellipsis: bool,
    pub line_height: Option<f32>,
}

impl TextStyle {
    pub fn new(size: f32, color: Color) -> TextStyle {
        TextStyle {
            font: Font::Text,
            size,
            weight: 400,
            color,
            align: Align::Start,
            valign: Align::Start,
            wrap: false,
            ellipsis: true,
            line_height: None,
        }
    }
    pub fn bold(mut self) -> Self {
        self.weight = 600;
        self
    }
    pub fn font(mut self, f: Font) -> Self {
        self.font = f;
        self
    }
    pub fn wrap(mut self) -> Self {
        self.wrap = true;
        self.ellipsis = false;
        self
    }
    pub fn center(mut self) -> Self {
        self.align = Align::Center;
        self
    }
    pub fn right(mut self) -> Self {
        self.align = Align::End;
        self
    }
    pub fn middle(mut self) -> Self {
        self.valign = Align::Center;
        self
    }
    pub fn lines(mut self, h: f32) -> Self {
        self.line_height = Some(h);
        self
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct FormatKey {
    font: Font,
    size: u32,
    weight: u32,
}

/// The factories and text formats, one set per UI thread.
pub struct Gfx {
    pub d2d: ID2D1Factory,
    pub dw: IDWriteFactory,
    formats: RefCell<HashMap<FormatKey, (IDWriteTextFormat, IDWriteInlineObject)>>,
    icon_family: &'static str,
    text_family: &'static str,
    display_family: &'static str,
}

thread_local! {
    static GFX: RefCell<Option<std::rc::Rc<Gfx>>> = const { RefCell::new(None) };
    static GENERATION: Cell<u64> = const { Cell::new(1) };
}

pub fn with<R>(f: impl FnOnce(&Gfx) -> R) -> R {
    let g = GFX.with(|g| {
        g.borrow_mut().get_or_insert_with(|| std::rc::Rc::new(Gfx::new().expect("Direct2D and DirectWrite are part of Windows"))).clone()
    });
    f(&g)
}

/// Lets the factories and text formats go while no window is showing (they're made again for the next one): Direct2D
/// and DirectWrite keep caches behind them.
pub fn release() {
    GFX.with(|g| g.borrow_mut().take());
}

impl Gfx {
    fn new() -> windows::core::Result<Gfx> {
        // SAFETY: plain factory creation.
        unsafe {
            let d2d: ID2D1Factory = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
            let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
            let has = |name: &str| -> bool {
                let mut collection = None;
                if dw.GetSystemFontCollection(&mut collection, false).is_err() {
                    return false;
                }
                let Some(c) = collection else { return false };
                let mut index = 0u32;
                let mut exists = windows::core::BOOL(0);
                c.FindFamilyName(&windows::core::HSTRING::from(name), &mut index, &mut exists).is_ok() && exists.as_bool()
            };
            let icon_family = if has("Segoe Fluent Icons") { "Segoe Fluent Icons" } else { "Segoe MDL2 Assets" };
            let (text_family, display_family) =
                if has("Segoe UI Variable Text") { ("Segoe UI Variable Text", "Segoe UI Variable Display") } else { ("Segoe UI", "Segoe UI") };
            Ok(Gfx { d2d, dw, formats: RefCell::new(HashMap::new()), icon_family, text_family, display_family })
        }
    }

    fn format(&self, font: Font, size: f32, weight: u32) -> (IDWriteTextFormat, IDWriteInlineObject) {
        let key = FormatKey { font, size: (size * 100.0) as u32, weight };
        if let Some(f) = self.formats.borrow().get(&key) {
            return f.clone();
        }
        let family = match font {
            Font::Text => self.text_family,
            Font::Display => self.display_family,
            Font::Icons => self.icon_family,
        };
        // SAFETY: DirectWrite calls with valid strings.
        let made = unsafe {
            let f = self
                .dw
                .CreateTextFormat(
                    &windows::core::HSTRING::from(family),
                    None,
                    DWRITE_FONT_WEIGHT(weight as i32),
                    DWRITE_FONT_STYLE_NORMAL,
                    DWRITE_FONT_STRETCH_NORMAL,
                    size,
                    w!(""),
                )
                .expect("a text format");
            let sign = self.dw.CreateEllipsisTrimmingSign(&f).expect("an ellipsis");
            (f, sign)
        };
        self.formats.borrow_mut().insert(key, made.clone());
        made
    }

    /// A laid-out piece of text, `max_w` wide (and `max_h` high, for vertical alignment).
    pub fn layout(&self, text: &str, style: &TextStyle, max_w: f32, max_h: f32) -> IDWriteTextLayout {
        let (format, sign) = self.format(style.font, style.size, style.weight);
        let wide: Vec<u16> = text.encode_utf16().collect();
        // SAFETY: DirectWrite calls with valid arguments.
        unsafe {
            let l = self.dw.CreateTextLayout(&wide, &format, max_w.max(1.0), max_h.max(1.0)).expect("a text layout");
            let _ = l.SetTextAlignment(match style.align {
                Align::Start => DWRITE_TEXT_ALIGNMENT_LEADING,
                Align::Center => DWRITE_TEXT_ALIGNMENT_CENTER,
                Align::End => DWRITE_TEXT_ALIGNMENT_TRAILING,
            });
            let _ = l.SetParagraphAlignment(match style.valign {
                Align::Start => DWRITE_PARAGRAPH_ALIGNMENT_NEAR,
                Align::Center => DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
                Align::End => DWRITE_PARAGRAPH_ALIGNMENT_FAR,
            });
            let _ = l.SetWordWrapping(if style.wrap { DWRITE_WORD_WRAPPING_WRAP } else { DWRITE_WORD_WRAPPING_NO_WRAP });
            if style.ellipsis {
                let t = DWRITE_TRIMMING { granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER, delimiter: 0, delimiterCount: 0 };
                let _ = l.SetTrimming(&t, &sign);
            }
            if let Some(lh) = style.line_height {
                let _ = l.SetLineSpacing(DWRITE_LINE_SPACING_METHOD_UNIFORM, lh, lh * 0.8);
            }
            l
        }
    }

    /// The size text takes, wrapped at `max_w` when the style wraps.
    pub fn measure(&self, text: &str, style: &TextStyle, max_w: f32) -> (f32, f32) {
        let l = self.layout(text, style, if style.wrap { max_w } else { 100_000.0 }, 100_000.0);
        let mut m = DWRITE_TEXT_METRICS::default();
        // SAFETY: valid out pointer.
        unsafe {
            let _ = l.GetMetrics(&mut m);
        }
        (m.widthIncludingTrailingWhitespace, m.height)
    }

    /// How high one line of the style is (the font's own line height, not a guess from its size).
    pub fn line_height(&self, style: &TextStyle) -> f32 {
        self.measure("A", &TextStyle { wrap: false, ..*style }, 1000.0).1
    }
}

/// A picture to draw: premultiplied BGRA kept in memory, its Direct2D bitmap made on first use.
pub struct Image {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    bitmap: RefCell<Option<(u64, ID2D1Bitmap)>>,
}

impl Image {
    /// From straight-alpha BGRA (alpha is multiplied in).
    pub fn from_bgra(mut pixels: Vec<u8>, width: u32, height: u32) -> Image {
        for p in pixels.as_chunks_mut::<4>().0 {
            let a = p[3] as u32;
            if a != 255 {
                p[0] = (p[0] as u32 * a / 255) as u8;
                p[1] = (p[1] as u32 * a / 255) as u8;
                p[2] = (p[2] as u32 * a / 255) as u8;
            }
        }
        Image { pixels, width, height, bitmap: RefCell::new(None) }
    }

    /// Already premultiplied BGRA.
    pub fn from_pbgra(pixels: Vec<u8>, width: u32, height: u32) -> Image {
        Image { pixels, width, height, bitmap: RefCell::new(None) }
    }

}

/// The pixels a window shows: a 32-bit top-down DIB in a memory DC, the size of the client area.
struct Surface {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Surface {
    fn new(width: i32, height: i32) -> Option<Surface> {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        // SAFETY: a memory DC and a DIB section owned by the Surface, released in Drop.
        unsafe {
            let dc = CreateCompatibleDC(None);
            if dc.is_invalid() {
                return None;
            }
            let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(dc);
                return None;
            };
            let old = SelectObject(dc, bitmap.into());
            Some(Surface { dc, bitmap, old, bits: bits as *mut u8, width, height })
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        // SAFETY: the DC and bitmap this Surface made.
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

/// A window's drawing surface. Direct2D draws (in software) into a DIB of our own, which GDI copies into the window
/// with its alpha (the DWM backdrop shows through where it's translucent).
///
/// Not an HWND render target: that one presents through DXGI, which skips presenting to a cloaked window or while the
/// screen is off. The first frame of a window shown cloaked (`native::show_cloaked`) then never arrived, and DWM showed
/// a surface nobody had painted: white, or a white frame round a canvas made while the window still had the thick
/// frame of WS_THICKFRAME. A copy into the window is there the moment it's made, and always the client area's size.
pub struct Canvas {
    rt: ID2D1DCRenderTarget,
    brush: ID2D1SolidColorBrush,
    generation: u64,
    hwnd: HWND,
    surface: RefCell<Option<Surface>>,
    /// The opacity brushes and masks of the layers pushed now, kept until each layer is popped.
    layer_resources: RefCell<Vec<(Option<ID2D1Brush>, Option<ID2D1Geometry>)>>,
    /// Frames Direct2D refused (the first few are logged).
    failures: Cell<u32>,
    pub dpi: f32,
}

impl Canvas {
    pub fn new(hwnd: HWND, dpi: f32) -> windows::core::Result<Canvas> {
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_SOFTWARE,
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: dpi,
            dpiY: dpi,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let generation = GENERATION.with(|g| {
            let v = g.get() + 1;
            g.set(v);
            v
        });
        with(|g| {
            // SAFETY: Direct2D calls with valid descriptions.
            unsafe {
                let rt = g.d2d.CreateDCRenderTarget(&props)?;
                rt.SetDpi(dpi, dpi);
                rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
                let brush = rt.CreateSolidColorBrush(&Color::rgb(0, 0, 0).d2d(), None)?;
                Ok(Canvas {
                    rt,
                    brush,
                    generation,
                    hwnd,
                    surface: RefCell::new(None),
                    layer_resources: RefCell::new(vec![]),
                    failures: Cell::new(0),
                    dpi,
                })
            }
        })
    }

    /// The client area changed size: the next frame is drawn at the new one (the surface follows by itself).
    pub fn resize(&self, _width: u32, _height: u32) {}

    pub fn set_dpi(&mut self, dpi: f32) {
        self.dpi = dpi;
        // SAFETY: plain call.
        unsafe { self.rt.SetDpi(dpi, dpi) };
    }

    pub fn begin(&self, clear: Color) {
        let mut rc = RECT::default();
        // SAFETY: valid window and out pointer.
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let (w, h) = ((rc.right - rc.left).max(1), (rc.bottom - rc.top).max(1));
        let mut surface = self.surface.borrow_mut();
        if surface.as_ref().is_none_or(|s| s.width != w || s.height != h) {
            *surface = None;
            *surface = Surface::new(w, h);
        }
        // SAFETY: the DIB holds w*h*4 bytes; Direct2D draws into it between BeginDraw and EndDraw.
        unsafe {
            if let Some(s) = surface.as_ref() {
                // Blank (fully transparent), so nothing of the last frame blends into this one.
                std::ptr::write_bytes(s.bits, 0, (w * h * 4) as usize);
                let _ = self.rt.BindDC(s.dc, &RECT { left: 0, top: 0, right: w, bottom: h });
            }
            self.rt.BeginDraw();
            self.rt.SetTransform(&Matrix3x2::identity());
            self.rt.Clear(Some(&clear.d2d()));
        }
    }

    /// Ends the frame and puts it in the window. False when the target has to be made again.
    ///
    /// A frame Direct2D refuses is logged and left out: copied in, its blank surface turned the whole window into bare
    /// backdrop, a grey pane.
    pub fn end(&self) -> bool {
        // SAFETY: ends the drawing begun above.
        let r = unsafe { self.rt.EndDraw(None, None) };
        if let Err(e) = &r {
            let n = self.failures.get() + 1;
            self.failures.set(n);
            if n <= 3 {
                nuiter_core::Log::warn(format!("Drawing failed: {e}"));
            }
            return e.code() != D2DERR_RECREATE_TARGET;
        }
        if let Some(s) = self.surface.borrow().as_ref() {
            // SAFETY: plain GDI calls with this window's DC and our memory DC.
            unsafe {
                let dc = GetDC(Some(self.hwnd));
                let _ = BitBlt(dc, 0, 0, s.width, s.height, Some(s.dc), 0, 0, SRCCOPY);
                ReleaseDC(Some(self.hwnd), dc);
            }
        }
        true
    }

    fn brush(&self, c: Color) -> &ID2D1SolidColorBrush {
        // SAFETY: plain call.
        unsafe { self.brush.SetColor(&c.d2d()) };
        &self.brush
    }

    pub fn fill(&self, r: Rect, c: Color) {
        if c.a <= 0.0 {
            return;
        }
        // SAFETY: drawing call.
        unsafe { self.rt.FillRectangle(&r.d2d(), self.brush(c)) };
    }

    pub fn round(&self, r: Rect, radius: f32, c: Color) {
        if c.a <= 0.0 {
            return;
        }
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        // SAFETY: drawing call.
        unsafe { self.rt.FillRoundedRectangle(&rr, self.brush(c)) };
    }

    /// A rounded outline drawn inside `r` (as WPF's borders are).
    pub fn stroke_round(&self, r: Rect, radius: f32, c: Color, width: f32) {
        if c.a <= 0.0 {
            return;
        }
        let half = width / 2.0;
        let rr = D2D1_ROUNDED_RECT { rect: r.inset(half, half, half, half).d2d(), radiusX: (radius - half).max(0.0), radiusY: (radius - half).max(0.0) };
        // SAFETY: drawing call.
        unsafe { self.rt.DrawRoundedRectangle(&rr, self.brush(c), width, None) };
    }

    pub fn ellipse(&self, cx: f32, cy: f32, rx: f32, ry: f32, c: Color) {
        let e = D2D1_ELLIPSE { point: Vector2 { X: cx, Y: cy }, radiusX: rx, radiusY: ry };
        // SAFETY: drawing call.
        unsafe { self.rt.FillEllipse(&e, self.brush(c)) };
    }

    /// Text in a box; returns the height the text took.
    pub fn text(&self, r: Rect, text: &str, style: &TextStyle) -> f32 {
        if text.is_empty() || r.w <= 0.0 {
            return 0.0;
        }
        let layout = with(|g| g.layout(text, style, r.w, r.h.max(1.0)));
        self.draw_layout(&layout, r.x, r.y, style.color);
        let mut m = DWRITE_TEXT_METRICS::default();
        // SAFETY: valid out pointer.
        unsafe {
            let _ = layout.GetMetrics(&mut m);
        }
        m.height
    }

    pub fn draw_layout(&self, layout: &IDWriteTextLayout, x: f32, y: f32, c: Color) {
        // SAFETY: drawing call.
        unsafe {
            self.rt.DrawTextLayout(Vector2 { X: x, Y: y }, layout, self.brush(c), D2D1_DRAW_TEXT_OPTIONS_ENABLE_COLOR_FONT | D2D1_DRAW_TEXT_OPTIONS_CLIP);
        }
    }

    /// A glyph of the icon font, centred in `r`.
    pub fn icon(&self, r: Rect, glyph: char, size: f32, c: Color) {
        let style = TextStyle { font: Font::Icons, ellipsis: false, ..TextStyle::new(size, c) }.center().middle();
        let mut buf = [0u8; 4];
        let layout = with(|g| g.layout(glyph.encode_utf8(&mut buf), &style, r.w.max(size * 2.0), r.h.max(size * 2.0)));
        let (ox, oy) = ((r.w - r.w.max(size * 2.0)) / 2.0, (r.h - r.h.max(size * 2.0)) / 2.0);
        self.draw_layout(&layout, r.x + ox, r.y + oy, c);
    }

    fn bitmap(&self, img: &Image) -> Option<ID2D1Bitmap> {
        let mut cached = img.bitmap.borrow_mut();
        if let Some((g, b)) = cached.as_ref()
            && *g == self.generation
        {
            return Some(b.clone());
        }
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT { format: DXGI_FORMAT_B8G8R8A8_UNORM, alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        // SAFETY: the pixels are width*height*4 bytes.
        let b = unsafe {
            self.rt.CreateBitmap(
                D2D_SIZE_U { width: img.width, height: img.height },
                Some(img.pixels.as_ptr() as *const _),
                img.width * 4,
                &props,
            )
        }
        .ok()?;
        *cached = Some((self.generation, b.clone()));
        Some(b)
    }

    /// A picture into `dest`, from its part `src` (pixels), with rounded corners when `radius` > 0.
    pub fn image(&self, img: &Image, dest: Rect, src: Option<Rect>, radius: f32, opacity: f32) {
        let Some(b) = self.bitmap(img) else { return };
        let s = src.map(|s| s.d2d());
        // SAFETY: drawing calls; the layer is popped right after.
        unsafe {
            if radius > 0.0
                && let Some(geometry) = self.rounded_geometry(dest, radius)
            {
                let params = D2D1_LAYER_PARAMETERS {
                    contentBounds: dest.d2d(),
                    geometricMask: std::mem::ManuallyDrop::new(Some(geometry)),
                    maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
                    maskTransform: Matrix3x2::identity(),
                    opacity: 1.0,
                    opacityBrush: std::mem::ManuallyDrop::new(None),
                    layerOptions: D2D1_LAYER_OPTIONS_NONE,
                };
                self.rt.PushLayer(&params, None);
                self.rt.DrawBitmap(&b, Some(&dest.d2d()), opacity, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, s.as_ref().map(|s| s as *const _));
                self.rt.PopLayer();
                let _ = std::mem::ManuallyDrop::into_inner(params.geometricMask);
                return;
            }
            self.rt.DrawBitmap(&b, Some(&dest.d2d()), opacity, D2D1_BITMAP_INTERPOLATION_MODE_LINEAR, s.as_ref().map(|s| s as *const _));
        }
    }

    pub fn clip(&self, r: Rect) {
        // SAFETY: matched by unclip().
        unsafe { self.rt.PushAxisAlignedClip(&r.d2d(), D2D1_ANTIALIAS_MODE_ALIASED) };
    }

    pub fn unclip(&self) {
        // SAFETY: pops the clip pushed by clip().
        unsafe { self.rt.PopAxisAlignedClip() };
    }

    /// Everything drawn until `pop_layer` is faded to `opacity`.
    pub fn push_opacity(&self, opacity: f32) {
        self.push_layer(D2D_RECT_F { left: -1e6, top: -1e6, right: 1e6, bottom: 1e6 }, opacity, None, None);
    }

    /// Everything drawn in `r` until `pop_layer` fades out over its last `fade` DIPs: scrolled content that goes on
    /// below a panel's edge, instead of a line cut in half.
    pub fn push_fade_bottom(&self, r: Rect, fade: f32) {
        let stops = [
            D2D1_GRADIENT_STOP { position: 0.0, color: D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 1.0 } },
            D2D1_GRADIENT_STOP { position: 1.0, color: D2D1_COLOR_F { r: 0.0, g: 0.0, b: 0.0, a: 0.0 } },
        ];
        let props = D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
            startPoint: Vector2 { X: r.x, Y: r.bottom() - fade },
            endPoint: Vector2 { X: r.x, Y: r.bottom() },
        };
        // SAFETY: Direct2D calls with valid descriptions.
        let brush = unsafe {
            self.rt
                .CreateGradientStopCollection(&stops, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)
                .and_then(|g| self.rt.CreateLinearGradientBrush(&props, None, &g))
                .and_then(|b| b.cast::<ID2D1Brush>())
                .ok()
        };
        self.push_layer(r.d2d(), 1.0, brush, None);
    }

    /// Everything drawn until `pop_layer` is cut to `r` with rounded corners.
    pub fn push_round_clip(&self, r: Rect, radius: f32) {
        self.push_layer(r.d2d(), 1.0, None, self.rounded_geometry(r, radius));
    }

    /// A rounded rectangle made by this canvas's own factory. Direct2D refuses a whole frame that mixes factories
    /// (D2DERR_WRONG_FACTORY), and the shared one in `Gfx` is replaced after `release`: closing Settings while the
    /// search window was hidden but still had its canvas made every later frame with a rounded picture fail, and the
    /// window showed nothing but its grey backdrop.
    fn rounded_geometry(&self, r: Rect, radius: f32) -> Option<ID2D1Geometry> {
        let rr = D2D1_ROUNDED_RECT { rect: r.d2d(), radiusX: radius, radiusY: radius };
        // SAFETY: Direct2D calls with a valid description.
        unsafe { self.rt.GetFactory().and_then(|f| f.CreateRoundedRectangleGeometry(&rr)).and_then(|g| g.cast()).ok() }
    }

    fn push_layer(&self, bounds: D2D_RECT_F, opacity: f32, brush: Option<ID2D1Brush>, mask: Option<ID2D1Geometry>) {
        let params = D2D1_LAYER_PARAMETERS {
            contentBounds: bounds,
            geometricMask: std::mem::ManuallyDrop::new(mask.clone()),
            maskAntialiasMode: D2D1_ANTIALIAS_MODE_PER_PRIMITIVE,
            maskTransform: Matrix3x2::identity(),
            opacity,
            opacityBrush: std::mem::ManuallyDrop::new(brush.clone()),
            layerOptions: D2D1_LAYER_OPTIONS_NONE,
        };
        // SAFETY: matched by pop_layer(); the brush and the mask are kept alive until then.
        unsafe { self.rt.PushLayer(&params, None) };
        let _ = std::mem::ManuallyDrop::into_inner(params.opacityBrush);
        let _ = std::mem::ManuallyDrop::into_inner(params.geometricMask);
        self.layer_resources.borrow_mut().push((brush, mask));
    }

    pub fn pop_layer(&self) {
        // SAFETY: pops the layer pushed by push_opacity(), push_fade_bottom() or push_round_clip().
        unsafe { self.rt.PopLayer() };
        self.layer_resources.borrow_mut().pop();
    }

    pub fn translate(&self, x: f32, y: f32) {
        // SAFETY: plain call.
        unsafe { self.rt.SetTransform(&Matrix3x2::translation(x, y)) };
    }

    pub fn rotate_around(&self, degrees: f32, cx: f32, cy: f32) {
        // SAFETY: plain call.
        unsafe { self.rt.SetTransform(&Matrix3x2::rotation_around(degrees, Vector2 { X: cx, Y: cy })) };
    }

    pub fn reset_transform(&self) {
        // SAFETY: plain call.
        unsafe { self.rt.SetTransform(&Matrix3x2::identity()) };
    }

}
