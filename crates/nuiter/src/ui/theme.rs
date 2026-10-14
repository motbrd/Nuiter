//! Light/dark palette following Windows' app theme and accent colour, plus Mica/Acrylic backdrops.

use std::cell::Cell;

use windows::UI::ViewManagement::{UIColorType, UISettings};
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute};
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::Controls::MARGINS;
use windows::core::w;

use super::gfx::Color;

#[derive(Clone, Copy, Debug)]
pub struct Theme {
    pub dark: bool,
    /// The taskbar (and its tray) follows the Windows mode, which can differ from the app mode.
    pub taskbar_dark: bool,
    pub backdrop: bool,
    pub window_bg: Color,
    pub panel_bg: Color,
    pub input_bg: Color,
    pub input_bg_focus: Color,
    pub hover: Color,
    pub selected: Color,
    pub stroke: Color,
    pub stroke_strong: Color,
    pub text: Color,
    pub text2: Color,
    pub text3: Color,
    pub chip_bg: Color,
    pub accent: Color,
    pub accent_text: Color,
    pub on_accent: Color,
    pub error: Color,
}

thread_local! {
    static CURRENT: Cell<Option<Theme>> = const { Cell::new(None) };
}

pub fn current() -> Theme {
    CURRENT.with(|c| match c.get() {
        Some(t) => t,
        None => {
            let t = Theme::read();
            c.set(Some(t));
            t
        }
    })
}

/// Reads the theme again (Windows' colours changed).
pub fn refresh() -> Theme {
    let t = Theme::read();
    CURRENT.with(|c| c.set(Some(t)));
    t
}

fn read_dark(value: windows::core::PCWSTR) -> bool {
    let mut v = 1u32;
    let mut size = 4u32;
    // SAFETY: reads a DWORD into a u32.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize"),
            value,
            RRF_RT_REG_DWORD,
            None,
            Some(&mut v as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    r.is_ok() && v == 0
}

/// Backdrops need Windows 11 22H2 (build 22621); NUITER_NO_BACKDROP turns them off.
pub fn backdrop_supported() -> bool {
    std::env::var_os("NUITER_NO_BACKDROP").is_none() && build_number() >= 22621
}

fn build_number() -> u32 {
    let mut v = [0u16; 16];
    let mut size = (v.len() * 2) as u32;
    // SAFETY: reads a string value into a fixed buffer.
    let r = unsafe {
        RegGetValueW(
            windows::Win32::System::Registry::HKEY_LOCAL_MACHINE,
            w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion"),
            w!("CurrentBuildNumber"),
            windows::Win32::System::Registry::RRF_RT_REG_SZ,
            None,
            Some(v.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if r.is_err() {
        return 0;
    }
    String::from_utf16_lossy(&v[..(size as usize / 2).saturating_sub(1)]).trim_end_matches('\0').parse().unwrap_or(0)
}

fn luma(c: Color) -> f32 {
    (0.299 * c.r + 0.587 * c.g + 0.114 * c.b) * 255.0
}

fn read_accent(dark: bool) -> Color {
    let r = (|| -> windows::core::Result<Color> {
        // Like WinUI: a lighter accent shade on dark backgrounds, a darker one on light backgrounds.
        let kind = if dark { UIColorType::AccentLight2 } else { UIColorType::AccentDark1 };
        let c = UISettings::new()?.GetColorValue(kind)?;
        let color = Color::rgb(c.R, c.G, c.B);
        let l = luma(color);
        if dark && l < 70.0 {
            return Ok(Color::rgb(0x60, 0xCD, 0xFF)); // accent too dark to see
        }
        if !dark && l > 200.0 {
            return Ok(Color::rgb(0x00, 0x5F, 0xB8)); // accent too light to see
        }
        Ok(color)
    })();
    r.unwrap_or(Color::rgb(0x00, 0x67, 0xC0))
}

fn lighten(c: Color, f: f32) -> Color {
    Color { r: c.r + (1.0 - c.r) * f, g: c.g + (1.0 - c.g) * f, b: c.b + (1.0 - c.b) * f, a: 1.0 }
}

fn darken(c: Color, f: f32) -> Color {
    Color { r: c.r * (1.0 - f), g: c.g * (1.0 - f), b: c.b * (1.0 - f), a: 1.0 }
}

impl Theme {
    fn read() -> Theme {
        let dark = read_dark(w!("AppsUseLightTheme"));
        let taskbar_dark = read_dark(w!("SystemUsesLightTheme"));
        let backdrop = backdrop_supported();
        let accent = read_accent(dark);
        let a = Color::argb;
        let on_accent = if luma(accent) > 150.0 { Color::rgb(0, 0, 0) } else { Color::rgb(255, 255, 255) };
        if dark {
            Theme {
                dark,
                taskbar_dark,
                backdrop,
                window_bg: if backdrop { a(0xB8202020) } else { a(0xFF202020) },
                panel_bg: a(0x0DFFFFFF),
                input_bg: a(0x12FFFFFF),
                input_bg_focus: a(0xB31E1E1E),
                hover: a(0x0FFFFFFF),
                selected: a(0x1AFFFFFF),
                stroke: a(0x14FFFFFF),
                stroke_strong: a(0x28FFFFFF),
                text: a(0xFFFFFFFF),
                text2: a(0xC8FFFFFF),
                text3: a(0x8AFFFFFF),
                chip_bg: a(0x0FFFFFFF),
                accent,
                accent_text: lighten(accent, 0.45),
                on_accent,
                error: a(0xFFE5484D),
            }
        } else {
            Theme {
                dark,
                taskbar_dark,
                backdrop,
                window_bg: if backdrop { a(0xC8F3F3F3) } else { a(0xFFF3F3F3) },
                panel_bg: a(0xB3FFFFFF),
                input_bg: a(0xB3FFFFFF),
                input_bg_focus: a(0xFFFFFFFF),
                hover: a(0x09000000),
                selected: a(0x0F000000),
                stroke: a(0x0F000000),
                stroke_strong: a(0x29000000),
                text: a(0xE4000000),
                text2: a(0x9E000000),
                text3: a(0x72000000),
                chip_bg: a(0xB3FFFFFF),
                accent,
                accent_text: darken(accent, 0.15),
                on_accent,
                error: a(0xFFE5484D),
            }
        }
    }
}

const DWMWA_USE_IMMERSIVE_DARK_MODE: u32 = 20;
const DWMWA_WINDOW_CORNER_PREFERENCE: u32 = 33;
const DWMWA_SYSTEMBACKDROP_TYPE: u32 = 38;
pub const DWMWA_CLOAK: u32 = 13;
/// DWMSBT_MAINWINDOW (Mica) and DWMSBT_TRANSIENTWINDOW (Acrylic).
pub const MICA: i32 = 2;
pub const ACRYLIC: i32 = 3;

pub fn dwm_int(hwnd: HWND, attribute: u32, value: i32) {
    // SAFETY: passes an i32 with its size.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(attribute as i32),
            &value as *const i32 as *const _,
            4,
        );
    }
}

/// The Windows 11 backdrop (Mica or Acrylic), rounded corners and a frame in the app's light or dark mode.
pub fn apply_backdrop(hwnd: HWND, kind: i32) {
    let t = current();
    dwm_int(hwnd, DWMWA_USE_IMMERSIVE_DARK_MODE, t.dark as i32);
    dwm_int(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, 2); // DWMWCP_ROUND
    if !t.backdrop {
        return;
    }
    let m = MARGINS { cxLeftWidth: -1, cxRightWidth: -1, cyTopHeight: -1, cyBottomHeight: -1 };
    // SAFETY: plain DWM calls on our window.
    unsafe {
        let _ = DwmExtendFrameIntoClientArea(hwnd, &m);
    }
    dwm_int(hwnd, DWMWA_SYSTEMBACKDROP_TYPE, kind);
}
