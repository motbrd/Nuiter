//! Small pieces of Windows the windows use: icons from .ico bytes, popup menus (in dark mode when Windows is), message
//! boxes, the folder picker, decoding pictures with WIC, and bringing a window to the front.

use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, PCSTR, w};

pub const ICON_DARK: &[u8] = include_bytes!("../../assets/nuiter.ico");
pub const ICON_LIGHT: &[u8] = include_bytes!("../../assets/nuiter-light.ico");

/// An icon of about `size` pixels from an .ico file's bytes (the entry nearest that size).
pub fn icon_from_ico(ico: &[u8], size: i32) -> Option<HICON> {
    if ico.len() < 6 {
        return None;
    }
    let count = u16::from_le_bytes([ico[4], ico[5]]) as usize;
    let mut best: Option<(i32, usize, usize)> = None;
    for i in 0..count {
        let e = 6 + i * 16;
        if e + 16 > ico.len() {
            break;
        }
        let w = if ico[e] == 0 { 256 } else { ico[e] as i32 };
        let len = u32::from_le_bytes([ico[e + 8], ico[e + 9], ico[e + 10], ico[e + 11]]) as usize;
        let offset = u32::from_le_bytes([ico[e + 12], ico[e + 13], ico[e + 14], ico[e + 15]]) as usize;
        // The smallest entry at least as big as asked, else the biggest.
        let score = if w >= size { w - size } else { 10_000 + (size - w) };
        if best.is_none_or(|b| score < b.0) {
            best = Some((score, offset, len));
        }
    }
    let (_, offset, len) = best?;
    let data = ico.get(offset..offset + len)?;
    // SAFETY: the data is one icon image (PNG or DIB), as CreateIconFromResourceEx takes it.
    unsafe { CreateIconFromResourceEx(data, true, 0x0003_0000, size, size, LR_DEFAULTCOLOR).ok() }
}

/// Lets popup menus follow the dark mode (uxtheme's SetPreferredAppMode and FlushMenuThemes, by ordinal).
pub fn allow_dark_menus(dark: bool) {
    // SAFETY: loading uxtheme's exports by ordinal; both take and return plain values.
    unsafe {
        let Ok(ux) = LoadLibraryW(w!("uxtheme.dll")) else { return };
        if let Some(set_mode) = GetProcAddress(ux, PCSTR(135 as *const u8)) {
            let f: extern "system" fn(i32) -> i32 = std::mem::transmute(set_mode);
            f(if dark { 2 } else { 3 }); // ForceDark / ForceLight
        }
        if let Some(flush) = GetProcAddress(ux, PCSTR(136 as *const u8)) {
            let f: extern "system" fn() = std::mem::transmute(flush);
            f();
        }
    }
}

pub enum MenuItem<'a> {
    Item(u32, &'a str),
    Bold(u32, &'a str),
    Separator,
}

/// Shows a popup menu at a screen point; returns the chosen command (0: none).
pub fn popup_menu(owner: HWND, at: POINT, items: &[MenuItem<'_>]) -> u32 {
    // SAFETY: a menu built, shown and destroyed here.
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return 0 };
        for item in items {
            let _ = match item {
                MenuItem::Item(id, text) | MenuItem::Bold(id, text) => {
                    AppendMenuW(menu, MF_STRING, *id as usize, &HSTRING::from(*text))
                }
                MenuItem::Separator => AppendMenuW(menu, MF_SEPARATOR, 0, None),
            };
            if let MenuItem::Bold(id, _) = item {
                let _ = SetMenuDefaultItem(menu, *id, 0);
            }
        }
        // A menu of a window in the background closes when clicked away only if that window comes first.
        let _ = SetForegroundWindow(owner);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, at.x, at.y, None, owner, None);
        let _ = DestroyMenu(menu);
        cmd.0 as u32
    }
}

pub fn message_box(owner: HWND, text: &str, ok_cancel: bool) -> bool {
    let style = if ok_cancel { MB_OKCANCEL | MB_DEFBUTTON2 } else { MB_OK };
    // SAFETY: a modal message box.
    let r = unsafe { MessageBoxW(Some(owner), &HSTRING::from(text), w!("Nuiter"), style) };
    r == IDOK
}

/// A Yes/No question; true for Yes. `yes_first` makes Yes the default button.
pub fn ask(owner: HWND, text: &str, yes_first: bool) -> bool {
    let style = MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | if yes_first { MB_DEFBUTTON1 } else { MB_DEFBUTTON2 };
    // SAFETY: a modal message box.
    let r = unsafe { MessageBoxW(Some(owner), &HSTRING::from(text), w!("Nuiter"), style) };
    r == IDYES
}

/// The folder picker; None when cancelled.
pub fn pick_folder(owner: HWND, title: &str) -> Option<String> {
    // SAFETY: the shell's dialog on this (STA) thread.
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
        dlg.SetTitle(&HSTRING::from(title)).ok()?;
        dlg.Show(Some(owner)).ok()?;
        let item = dlg.GetResult().ok()?;
        let name = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = name.to_string().ok();
        windows::Win32::System::Com::CoTaskMemFree(Some(name.0 as *const _));
        s
    }
}

/// Decodes an encoded picture (PNG, JPEG, BMP…) into premultiplied BGRA, scaled down to fit `max_w`×`max_h`. Any
/// thread (with COM initialised).
pub fn decode_image(bytes: &[u8], max_w: u32, max_h: u32) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: WIC calls on owned interfaces.
    unsafe {
        let wic: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = wic.CreateStream().ok()?;
        stream.InitializeFromMemory(bytes).ok()?;
        let decoder = wic.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand).ok()?;
        let frame = decoder.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64).min(1.0);
        let (tw, th) = (((w as f64 * scale) as u32).max(1), ((h as f64 * scale) as u32).max(1));
        let source: IWICBitmapSource = if (tw, th) != (w, h) {
            let scaler = wic.CreateBitmapScaler().ok()?;
            scaler.Initialize(&frame, tw, th, WICBitmapInterpolationModeFant).ok()?;
            scaler.into()
        } else {
            frame.into()
        };
        let converter = wic.CreateFormatConverter().ok()?;
        converter.Initialize(&source, &GUID_WICPixelFormat32bppPBGRA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom).ok()?;
        let mut pixels = vec![0u8; (tw * th * 4) as usize];
        converter.CopyPixels(std::ptr::null(), tw * 4, &mut pixels).ok()?;
        Some((pixels, tw, th))
    }
}

/// Brings a window to the front even when Windows' foreground lock would normally refuse.
pub fn force_foreground(hwnd: HWND) {
    // SAFETY: plain window-manager calls.
    unsafe {
        let fg = GetForegroundWindow();
        if fg == hwnd {
            return;
        }
        let fg_thread = GetWindowThreadProcessId(fg, None);
        let me = GetCurrentThreadId();
        if fg_thread != me {
            let _ = AttachThreadInput(me, fg_thread, true);
        }
        let _ = SetForegroundWindow(hwnd);
        if fg_thread != me {
            let _ = AttachThreadInput(me, fg_thread, false);
        }
    }
}

/// A window shown without its first, empty frame: cloaked while it's shown and drawn, then uncloaked.
pub fn show_cloaked(hwnd: HWND, cmd: SHOW_WINDOW_CMD) {
    super::theme::dwm_int(hwnd, super::theme::DWMWA_CLOAK, 1);
    // SAFETY: plain calls on our window.
    unsafe {
        let _ = ShowWindow(hwnd, cmd);
        let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
    }
    super::theme::dwm_int(hwnd, super::theme::DWMWA_CLOAK, 0);
}
