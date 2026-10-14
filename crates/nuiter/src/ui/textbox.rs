//! A single-line text field: editing, selection, the clipboard and drawing. Text is kept as UTF-16, as Windows hands
//! characters over and DirectWrite measures positions.

use windows::Win32::Graphics::DirectWrite::{DWRITE_HIT_TEST_METRICS, IDWriteTextLayout};
use windows::Win32::UI::Input::KeyboardAndMouse::*;

use super::clipboard;
use super::gfx::{self, Canvas, Color, Rect, TextStyle};

#[derive(Default, Clone)]
pub struct TextBox {
    pub text: Vec<u16>,
    pub caret: usize,
    pub anchor: usize,
    /// How far the text is scrolled left (DIPs) so the caret stays in view.
    scroll: f32,
    /// Where the caret was drawn last (for the IME's window), in DIPs.
    pub caret_rect: Rect,
    last_layout_x: f32,
}

/// What a key did.
#[derive(Debug, PartialEq, Eq)]
pub enum Edit {
    None,
    /// Only the caret or selection moved.
    Moved,
    Changed,
}

impl TextBox {
    pub fn new(text: &str) -> TextBox {
        let t: Vec<u16> = text.encode_utf16().collect();
        let n = t.len();
        TextBox { text: t, caret: n, anchor: n, ..Default::default() }
    }

    pub fn value(&self) -> String {
        String::from_utf16_lossy(&self.text)
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.encode_utf16().collect();
        self.caret = self.text.len();
        self.anchor = self.caret;
        self.scroll = 0.0;
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    pub fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.text.len();
    }

    fn delete_selection(&mut self) -> bool {
        let (a, b) = self.selection();
        if a == b {
            return false;
        }
        self.text.drain(a..b);
        self.caret = a;
        self.anchor = a;
        true
    }

    pub fn insert(&mut self, s: &[u16]) {
        self.delete_selection();
        // One line: line breaks and tabs become spaces, other control characters go.
        let clean: Vec<u16> = s
            .iter()
            .filter_map(|&c| match c {
                0x0A | 0x09 => Some(0x20),
                0x0D => None,
                c if c < 0x20 => None,
                c => Some(c),
            })
            .collect();
        let at = self.caret;
        self.text.splice(at..at, clean.iter().copied());
        self.caret = at + clean.len();
        self.anchor = self.caret;
    }

    /// A character from WM_CHAR.
    pub fn char_input(&mut self, c: u16) -> Edit {
        if c < 0x20 || c == 0x7F {
            return Edit::None;
        }
        self.insert(&[c]);
        Edit::Changed
    }

    fn is_word(c: u16) -> bool {
        char::from_u32(c as u32).is_some_and(|c| c.is_alphanumeric() || c == '_')
    }

    fn word_left(&self, from: usize) -> usize {
        let mut i = from;
        while i > 0 && !Self::is_word(self.text[i - 1]) {
            i -= 1;
        }
        while i > 0 && Self::is_word(self.text[i - 1]) {
            i -= 1;
        }
        i
    }

    fn word_right(&self, from: usize) -> usize {
        let mut i = from;
        let n = self.text.len();
        while i < n && Self::is_word(self.text[i]) {
            i += 1;
        }
        while i < n && !Self::is_word(self.text[i]) {
            i += 1;
        }
        i
    }

    /// Steps over a surrogate pair as one character.
    fn prev(&self, i: usize) -> usize {
        if i >= 2 && (0xDC00..0xE000).contains(&self.text[i - 1]) && (0xD800..0xDC00).contains(&self.text[i - 2]) { i - 2 } else { i.saturating_sub(1) }
    }

    fn next(&self, i: usize) -> usize {
        let n = self.text.len();
        if i + 1 < n && (0xD800..0xDC00).contains(&self.text[i]) { i + 2 } else { (i + 1).min(n) }
    }

    /// Editing keys (WM_KEYDOWN). Keys a field doesn't use return Edit::None.
    pub fn key_down(&mut self, vk: u16, ctrl: bool, shift: bool) -> Edit {
        let n = self.text.len();
        let move_to = |s: &mut Self, to: usize| {
            s.caret = to;
            if !shift {
                s.anchor = to;
            }
            Edit::Moved
        };
        match VIRTUAL_KEY(vk) {
            VK_LEFT => {
                if !shift && self.has_selection() {
                    let (a, _) = self.selection();
                    return move_to(self, a);
                }
                let to = if ctrl { self.word_left(self.caret) } else { self.prev(self.caret) };
                move_to(self, to)
            }
            VK_RIGHT => {
                if !shift && self.has_selection() {
                    let (_, b) = self.selection();
                    return move_to(self, b);
                }
                let to = if ctrl { self.word_right(self.caret) } else { self.next(self.caret) };
                move_to(self, to)
            }
            VK_HOME => move_to(self, 0),
            VK_END => move_to(self, n),
            VK_BACK => {
                if self.delete_selection() {
                    return Edit::Changed;
                }
                if self.caret == 0 {
                    return Edit::None;
                }
                let from = if ctrl { self.word_left(self.caret) } else { self.prev(self.caret) };
                self.text.drain(from..self.caret);
                self.caret = from;
                self.anchor = from;
                Edit::Changed
            }
            VK_DELETE => {
                if self.delete_selection() {
                    return Edit::Changed;
                }
                if self.caret >= n {
                    return Edit::None;
                }
                let to = if ctrl { self.word_right(self.caret) } else { self.next(self.caret) };
                self.text.drain(self.caret..to);
                Edit::Changed
            }
            k if ctrl && k == VIRTUAL_KEY(b'A' as u16) => {
                self.select_all();
                Edit::Moved
            }
            k if ctrl && k == VIRTUAL_KEY(b'C' as u16) => {
                if self.has_selection() {
                    let (a, b) = self.selection();
                    clipboard::set_text(&String::from_utf16_lossy(&self.text[a..b]));
                }
                Edit::None
            }
            k if ctrl && k == VIRTUAL_KEY(b'X' as u16) => {
                if self.has_selection() {
                    let (a, b) = self.selection();
                    clipboard::set_text(&String::from_utf16_lossy(&self.text[a..b]));
                    self.delete_selection();
                    return Edit::Changed;
                }
                Edit::None
            }
            k if ctrl && k == VIRTUAL_KEY(b'V' as u16) => match clipboard::get_text() {
                Some(t) => {
                    let w: Vec<u16> = t.encode_utf16().collect();
                    self.insert(&w);
                    Edit::Changed
                }
                None => Edit::None,
            },
            VK_INSERT if shift => match clipboard::get_text() {
                Some(t) => {
                    let w: Vec<u16> = t.encode_utf16().collect();
                    self.insert(&w);
                    Edit::Changed
                }
                None => Edit::None,
            },
            _ => Edit::None,
        }
    }

    fn layout(&self, style: &TextStyle) -> IDWriteTextLayout {
        let text = String::from_utf16_lossy(&self.text);
        let s = TextStyle { ellipsis: false, wrap: false, ..*style };
        gfx::with(|g| g.layout(&text, &s, 100_000.0, 100.0))
    }

    /// The text position nearest `x` (DIPs, relative to the field's text area as last drawn).
    pub fn hit(&self, x: f32, style: &TextStyle) -> usize {
        let l = self.layout(style);
        let mut trailing = windows::core::BOOL(0);
        let mut inside = windows::core::BOOL(0);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        // SAFETY: valid out pointers.
        unsafe {
            let _ = l.HitTestPoint(x - self.last_layout_x, 1.0, &mut trailing, &mut inside, &mut m);
        }
        (m.textPosition + if trailing.as_bool() { m.length } else { 0 }) as usize
    }

    pub fn click(&mut self, x: f32, style: &TextStyle, shift: bool) {
        self.caret = self.hit(x, style).min(self.text.len());
        if !shift {
            self.anchor = self.caret;
        }
    }

    pub fn drag(&mut self, x: f32, style: &TextStyle) {
        self.caret = self.hit(x, style).min(self.text.len());
    }

    /// Selects the word at `x` (a double click).
    pub fn select_word(&mut self, x: f32, style: &TextStyle) {
        let at = self.hit(x, style).min(self.text.len());
        let mut a = at;
        while a > 0 && Self::is_word(self.text[a - 1]) {
            a -= 1;
        }
        let mut b = at;
        while b < self.text.len() && Self::is_word(self.text[b]) {
            b += 1;
        }
        self.anchor = a;
        self.caret = b;
    }

    fn x_of(l: &IDWriteTextLayout, pos: usize) -> f32 {
        let (mut x, mut y) = (0f32, 0f32);
        let mut m = DWRITE_HIT_TEST_METRICS::default();
        // SAFETY: valid out pointers.
        unsafe {
            let _ = l.HitTestTextPosition(pos as u32, false, &mut x, &mut y, &mut m);
        }
        x
    }

    /// Draws the text (or `placeholder`), the selection and, when `caret_on`, the caret, inside `r`, vertically
    /// centred.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, c: &Canvas, r: Rect, style: &TextStyle, placeholder: &str, placeholder_color: Color, selection: Color, focused: bool, caret_on: bool) {
        let line_h = gfx::with(|g| g.line_height(style));
        let top = r.y + (r.h - line_h) / 2.0;
        c.clip(r);
        if self.text.is_empty() {
            if !placeholder.is_empty() {
                c.text(Rect::new(r.x, top, r.w, line_h + 4.0), placeholder, &TextStyle { color: placeholder_color, ..*style });
            }
            self.scroll = 0.0;
        }
        let l = self.layout(style);
        let caret_x = Self::x_of(&l, self.caret);
        // Keep the caret in view.
        if caret_x - self.scroll > r.w - 2.0 {
            self.scroll = caret_x - r.w + 2.0;
        }
        if caret_x - self.scroll < 0.0 {
            self.scroll = caret_x;
        }
        let x0 = r.x - self.scroll;
        self.last_layout_x = x0;
        if focused && self.has_selection() {
            let (a, b) = self.selection();
            let (xa, xb) = (Self::x_of(&l, a), Self::x_of(&l, b));
            c.fill(Rect::new(x0 + xa, top, xb - xa, line_h), selection);
        }
        if !self.text.is_empty() {
            c.draw_layout(&l, x0, top, style.color);
        }
        self.caret_rect = Rect::new(x0 + caret_x, top, 1.0, line_h);
        if focused && caret_on {
            c.fill(Rect::new(x0 + caret_x, top, 1.0, line_h), style.color);
        }
        c.unclip();
    }
}
