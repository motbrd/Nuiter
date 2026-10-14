//! For console programs: UTF-8 output, so Cyrillic and other scripts print as they are.

pub fn utf8_console() {
    // SAFETY: plain Win32 calls; failure (no console) is harmless.
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleOutputCP(65001);
        let _ = windows::Win32::System::Console::SetConsoleCP(65001);
    }
}
