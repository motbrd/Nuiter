//! Plays a recording or a video in the preview from the moment that matched (MFPlay): it works whatever player is
//! installed, including the ones that can't be told where to start.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{HSTRING, implement, w};

#[implement(IMFPMediaPlayerCallback)]
struct Events {
    start: f64,
}

impl IMFPMediaPlayerCallback_Impl for Events_Impl {
    fn OnMediaPlayerEvent(&self, header: *const MFP_EVENT_HEADER) {
        // SAFETY: MFPlay passes a valid header for the call's duration.
        let h = unsafe { &*header };
        let Some(player) = h.pMediaPlayer.as_ref() else { return };
        if h.eEventType == MFP_EVENT_TYPE_MEDIAITEM_SET {
            // SAFETY: COM calls on the player that sent the event.
            unsafe {
                if self.start > 0.0 {
                    let at = PROPVARIANT::from((self.start * 10_000_000.0) as i64);
                    let _ = player.SetPosition(&MFP_POSITIONTYPE_100NS, &at);
                }
                let _ = player.Play();
            }
        } else if h.eEventType == MFP_EVENT_TYPE_PLAYBACK_ENDED {
            crate::ui::window::later(|| crate::app::with_search(|s| s.stop_player()));
        }
    }
}

pub struct Player {
    player: IMFPMediaPlayer,
    video: Option<HWND>,
    pub playing: bool,
}

impl Player {
    /// Starts `path` at `seconds`; a video plays in a child window of `parent` at `rect` (client pixels).
    pub fn open(parent: HWND, path: &str, seconds: f64, video_rect: Option<RECT>) -> Option<Player> {
        // SAFETY: a child window and an MFPlay player owned by the returned value.
        unsafe {
            let video = video_rect.and_then(|r| {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    w!("Static"),
                    w!(""),
                    WS_CHILD | WS_VISIBLE,
                    r.left,
                    r.top,
                    r.right - r.left,
                    r.bottom - r.top,
                    Some(parent),
                    None,
                    Some(crate::ui::window::instance()),
                    None,
                )
                .ok()
            });
            let events: IMFPMediaPlayerCallback = Events { start: seconds }.into();
            let mut player: Option<IMFPMediaPlayer> = None;
            let created = MFPCreateMediaPlayer(&HSTRING::from(path), false, MFP_OPTION_NONE, &events, video, Some(&mut player));
            match (created, player) {
                (Ok(()), Some(player)) => Some(Player { player, video, playing: true }),
                _ => {
                    if let Some(v) = video {
                        let _ = DestroyWindow(v);
                    }
                    None
                }
            }
        }
    }

    pub fn toggle(&mut self) {
        // SAFETY: COM calls on our player.
        unsafe {
            if self.playing {
                let _ = self.player.Pause();
            } else {
                let _ = self.player.Play();
            }
        }
        self.playing = !self.playing;
    }

    pub fn video_window(&self) -> Option<HWND> {
        self.video
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // SAFETY: stops and releases the player, then its window.
        unsafe {
            let _ = self.player.Stop();
            let _ = self.player.Shutdown();
            if let Some(v) = self.video {
                let _ = DestroyWindow(v);
            }
        }
    }
}
