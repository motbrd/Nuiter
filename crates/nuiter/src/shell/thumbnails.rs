//! Explorer's thumbnails (photos, PDFs, videos, Office files) and icons via IShellItemImageFactory, made on a
//! single-threaded-apartment thread of their own (shell thumbnail providers expect one) and handed to the UI thread.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, mpsc};

use windows::Win32::Foundation::SIZE;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::Shell::{IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_BIGGERSIZEOK, SIIGBF_ICONONLY, SIIGBF_THUMBNAILONLY};
use windows::core::HSTRING;

/// Which requests are still wanted: the list's (new results replace them) or the preview's.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    List,
    Preview,
}

static LIST_GEN: AtomicU64 = AtomicU64::new(0);
static PREVIEW_GEN: AtomicU64 = AtomicU64::new(0);

/// A picture of what the file holds, or its icon when the shell has none.
pub struct Thumb {
    /// Straight-alpha BGRA.
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// The file's icon: shown no bigger than an icon, not filling a preview.
    pub icon: bool,
}

/// Gets the thumbnail, or None when there's none.
type Done = Box<dyn FnOnce(Option<Thumb>) + Send>;

struct Request {
    path: String,
    size: i32,
    lane: Lane,
    generation: u64,
    done: Done,
}

static QUEUE: Mutex<Option<mpsc::Sender<Request>>> = Mutex::new(None);

/// Starts a new generation of a lane: requests of older ones are dropped unread.
pub fn next_generation(lane: Lane) -> u64 {
    match lane {
        Lane::List => LIST_GEN.fetch_add(1, Ordering::SeqCst) + 1,
        Lane::Preview => PREVIEW_GEN.fetch_add(1, Ordering::SeqCst) + 1,
    }
}

fn current(lane: Lane) -> u64 {
    match lane {
        Lane::List => LIST_GEN.load(Ordering::SeqCst),
        Lane::Preview => PREVIEW_GEN.load(Ordering::SeqCst),
    }
}

/// A thumbnail (or the file's icon when it has none), to `done` on the thumbnail thread.
pub fn request(path: &str, size: i32, lane: Lane, generation: u64, done: impl FnOnce(Option<Thumb>) + Send + 'static) {
    let mut q = QUEUE.lock().unwrap();
    if q.is_none() {
        let (tx, rx) = mpsc::channel::<Request>();
        let _ = std::thread::Builder::new().name("Thumbnails".into()).stack_size(1 << 20).spawn(move || {
            nuiter_core::win::init_com_sta();
            while let Ok(r) = rx.recv() {
                if r.generation != current(r.lane) {
                    continue;
                }
                // The shell sometimes fails once (a file still being looked at elsewhere): one more try. Thumbnails
                // only, so a file without one is known by its icon.
                let thumb = load(&r.path, r.size, false).or_else(|| load(&r.path, r.size, false));
                let img = match thumb {
                    Some((pixels, width, height)) => Some(Thumb { pixels, width, height, icon: false }),
                    None => load(&r.path, r.size, true).map(|(pixels, width, height)| Thumb { pixels, width, height, icon: true }),
                };
                (r.done)(img);
            }
        });
        *q = Some(tx);
    }
    if let Some(tx) = q.as_ref() {
        let _ = tx.send(Request { path: path.into(), size, lane, generation, done: Box::new(done) });
    }
}

fn load(path: &str, size: i32, icon_only: bool) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: shell and GDI calls on objects owned here; the bitmap is deleted after its bits are copied.
    unsafe {
        let factory: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None).ok()?;
        let flags = if icon_only { SIIGBF_ICONONLY } else { SIIGBF_THUMBNAILONLY | SIIGBF_BIGGERSIZEOK };
        let hbmp = factory.GetImage(SIZE { cx: size, cy: size }, flags).ok()?;
        let r = bits_of(hbmp);
        let _ = DeleteObject(hbmp.into());
        r
    }
}

/// A bitmap's pixels as top-down BGRA. The shell's bitmaps carry straight alpha; one without any alpha is opaque.
unsafe fn bits_of(hbmp: HBITMAP) -> Option<(Vec<u8>, u32, u32)> {
    // SAFETY: GDI calls on a valid bitmap with buffers of the reported size.
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(hbmp.into(), std::mem::size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as *mut _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
        if w <= 0 || h <= 0 {
            return None;
        }
        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut px = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, hbmp, 0, h as u32, Some(px.as_mut_ptr() as *mut _), &mut bmi, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        if lines == 0 {
            return None;
        }
        if !px.iter().skip(3).step_by(4).any(|&a| a != 0) {
            for a in px.iter_mut().skip(3).step_by(4) {
                *a = 255;
            }
        }
        Some((px, w as u32, h as u32))
    }
}
