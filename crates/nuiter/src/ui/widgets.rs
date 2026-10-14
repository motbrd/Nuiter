//! The controls both windows draw (buttons, chips, toggles, text fields, scroll bars) and the map of what is under the
//! mouse, made while drawing.

use super::gfx::{self, Canvas, Color, Rect, TextStyle};
use super::theme::Theme;

/// Where clickable things were drawn last, topmost last.
pub struct Hits<H: Copy + PartialEq> {
    list: Vec<(Rect, H)>,
}

impl<H: Copy + PartialEq> Default for Hits<H> {
    fn default() -> Self {
        Self { list: vec![] }
    }
}

impl<H: Copy + PartialEq> Hits<H> {
    pub fn clear(&mut self) {
        self.list.clear();
    }

    pub fn add(&mut self, r: Rect, h: H) {
        self.list.push((r, h));
    }

    pub fn at(&self, x: f32, y: f32) -> Option<H> {
        self.list.iter().rev().find(|(r, _)| r.contains(x, y)).map(|(_, h)| *h)
    }

    pub fn rect_of(&self, h: H) -> Option<Rect> {
        self.list.iter().rev().find(|(_, x)| *x == h).map(|(r, _)| *r)
    }
}

/// Corners of controls (buttons, fields, combo boxes, check boxes, list items) and of cards and panels, as Windows 11
/// rounds them: one radius each, everywhere.
pub const RADIUS: f32 = 4.0;
pub const CARD_RADIUS: f32 = 6.0;

/// Buttons, text fields and combo boxes are all this high (Windows 11's standard control height).
pub const CONTROL_HEIGHT: f32 = 32.0;

pub const BUTTON_FONT: f32 = 13.0;

/// A button's width for its label.
pub fn button_width(label: &str) -> f32 {
    let (w, _) = gfx::with(|g| g.measure(label, &TextStyle::new(BUTTON_FONT, Color::TRANSPARENT), 1000.0));
    (w + 22.0).ceil()
}

pub const BUTTON_HEIGHT: f32 = CONTROL_HEIGHT;

/// A subtle (panel-coloured) button; `hot` while the mouse is over it.
pub fn subtle_button(c: &Canvas, t: &Theme, r: Rect, label: &str, hot: bool, enabled: bool) {
    let opacity = if enabled { 1.0 } else { 0.45 };
    c.round(r, RADIUS, if hot && enabled { t.selected } else { t.panel_bg }.with_alpha(opacity));
    c.stroke_round(r, RADIUS, t.stroke.with_alpha(opacity), 1.0);
    c.text(r, label, &TextStyle::new(BUTTON_FONT, t.text.with_alpha(opacity)).center().middle());
}

pub fn accent_button(c: &Canvas, t: &Theme, r: Rect, label: &str, hot: bool) {
    c.round(r, RADIUS, t.accent.with_alpha(if hot { 0.9 } else { 1.0 }));
    c.text(r, label, &TextStyle::new(BUTTON_FONT, t.on_accent).center().middle());
}

/// A 32×32 icon button: no frame until hovered.
pub fn icon_button(c: &Canvas, t: &Theme, r: Rect, glyph: char, hot: bool, color: Color) {
    if hot {
        c.round(r, RADIUS, t.hover);
    }
    c.icon(r, glyph, 14.0, color);
}

/// A filter chip; checked chips are filled with the accent.
pub fn chip(c: &Canvas, t: &Theme, r: Rect, label: &str, checked: bool, hot: bool) {
    let (bg, border, fg) = if checked {
        (t.accent, t.accent, t.on_accent)
    } else {
        (if hot { t.selected } else { t.chip_bg }, t.stroke, t.text2)
    };
    c.round(r, r.h / 2.0, bg);
    c.stroke_round(r, r.h / 2.0, border, 1.0);
    c.text(r, label, &TextStyle::new(12.5, fg).center().middle());
}

pub fn chip_width(label: &str) -> f32 {
    let (w, _) = gfx::with(|g| g.measure(label, &TextStyle::new(12.5, Color::TRANSPARENT), 1000.0));
    (w + 24.0).ceil()
}

/// The Windows 11 toggle switch with its On/Off label; returns its width. `r` is where it ends (right-aligned).
pub const TOGGLE_WIDTH: f32 = 28.0 + 12.0 + 40.0;

pub fn toggle(c: &Canvas, t: &Theme, r: Rect, on: bool, hot: bool, enabled: bool) {
    let opacity = if enabled { 1.0 } else { 0.45 };
    let label = Rect::new(r.x, r.y, 28.0, r.h);
    c.text(label, if on { "On" } else { "Off" }, &TextStyle::new(14.0, t.text.with_alpha(opacity)).right().middle());
    let track = Rect::new(r.x + 40.0, r.y + (r.h - 20.0) / 2.0, 40.0, 20.0);
    if on {
        c.round(track, 10.0, t.accent.with_alpha(opacity));
    } else {
        c.stroke_round(track, 10.0, t.text2.with_alpha(opacity), 1.0);
    }
    let d = if hot { 14.0 } else { 12.0 };
    let cx = if on { track.right() - 4.0 - 6.0 } else { track.x + 4.0 + 6.0 };
    c.ellipse(cx, track.y + 10.0, d / 2.0, d / 2.0, if on { t.on_accent } else { t.text2 }.with_alpha(opacity));
}

/// A check box (the subfolder lists'): 20×20.
pub fn check_box(c: &Canvas, t: &Theme, r: Rect, on: bool, hot: bool) {
    let b = Rect::new(r.x, r.y + (r.h - 20.0) / 2.0, 20.0, 20.0);
    if on {
        c.round(b, RADIUS, t.accent);
        c.icon(b, '\u{E73E}', 12.0, t.on_accent);
    } else {
        c.round(b, RADIUS, if hot { t.hover } else { t.input_bg });
        c.stroke_round(b, RADIUS, t.text2, 1.0);
    }
}

/// A combo box: the chosen item's name and a chevron, in a field's frame.
pub fn combo(c: &Canvas, t: &Theme, r: Rect, label: &str, hot: bool) {
    c.round(r, RADIUS, if hot { t.selected } else { t.input_bg });
    c.stroke_round(r, RADIUS, t.stroke, 1.0);
    c.text(r.inset(11.0, 0.0, 32.0, 0.0), label, &TextStyle::new(14.0, t.text).middle());
    c.icon(Rect::new(r.right() - 11.0 - 12.0, r.y, 12.0, r.h), '\u{E70D}', 11.0, t.text2);
}

/// A text field's frame: rounded, an underline that turns accent when focused, red when its text can't be used.
pub fn field_frame(c: &Canvas, t: &Theme, r: Rect, focused: bool, error: bool) {
    c.round(r, RADIUS, if focused { t.input_bg_focus } else { t.input_bg });
    c.stroke_round(r, RADIUS, t.stroke, 1.0);
    let (h, color) = if error {
        (2.0, t.error)
    } else if focused {
        (2.0, t.accent)
    } else {
        (1.0, t.stroke_strong)
    };
    c.fill(Rect::new(r.x + 2.0, r.bottom() - h, r.w - 4.0, h), color);
}

/// A scroll bar's width, at its view's right edge; what scrolls beside it keeps `SCROLL_GAP` to it.
pub const SCROLL_BAR_W: f32 = 6.0;
pub const SCROLL_GAP: f32 = 8.0;
/// The strip along a scroll bar that takes the mouse: a 6-DIP bar alone is hard to catch.
const SCROLL_GRAB_W: f32 = 12.0;

/// The thumb of a scroll bar for content of `content` height shown in `view` (scrolled by `scroll`); none when it fits.
fn scroll_thumb(view: Rect, content: f32, scroll: f32) -> Option<Rect> {
    if content <= view.h + 0.5 {
        return None;
    }
    let h = (view.h * view.h / content).clamp(24.0f32.min(view.h), view.h);
    let y = view.y + (view.h - h) * (scroll / (content - view.h)).clamp(0.0, 1.0);
    Some(Rect::new(view.right() - SCROLL_BAR_W, y, SCROLL_BAR_W, h))
}

/// A thin scroll bar (brighter while `hot`: hovered or held); returns the strip that grabs it, none when all fits.
pub fn scroll_bar(c: &Canvas, t: &Theme, view: Rect, content: f32, scroll: f32, hot: bool) -> Option<Rect> {
    let thumb = scroll_thumb(view, content, scroll)?;
    c.round(thumb, 3.0, if hot { t.text3 } else { t.stroke_strong });
    Some(Rect::new(view.right() - SCROLL_GRAB_W, view.y, SCROLL_GRAB_W, view.h))
}

/// A scroll bar held by the mouse: where it was taken and the scroll then.
#[derive(Clone, Copy, Debug)]
pub struct ScrollDrag {
    y: f32,
    scroll: f32,
}

impl ScrollDrag {
    /// Pressed at `y` on the bar's strip: the thumb is held where it is, or, pressed beside it, first jumps to
    /// centre under the mouse. Returns the drag and the scroll to show now.
    pub fn press(view: Rect, content: f32, scroll: f32, y: f32) -> (ScrollDrag, f32) {
        let scroll = match scroll_thumb(view, content, scroll) {
            Some(th) if y < th.y || y >= th.bottom() => ScrollDrag { y: th.y + th.h / 2.0, scroll }.to(view, content, y),
            _ => scroll,
        };
        (ScrollDrag { y, scroll }, scroll)
    }

    /// The scroll for the mouse at `y`.
    pub fn to(&self, view: Rect, content: f32, y: f32) -> f32 {
        let Some(th) = scroll_thumb(view, content, self.scroll) else { return 0.0 };
        let (track, max) = (view.h - th.h, content - view.h);
        if track <= 0.0 {
            return self.scroll;
        }
        (self.scroll + (y - self.y) * max / track).clamp(0.0, max)
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scroll_drag_follows_the_mouse() {
        // 1,000 DIPs of content in a 200-DIP view: a 40-DIP thumb on a 160-DIP track, 800 DIPs to scroll.
        let view = Rect::new(0.0, 100.0, 300.0, 200.0);
        // Held on the thumb: no jump, and the thumb moves with the mouse (5 DIPs of content a DIP).
        let (drag, scroll) = ScrollDrag::press(view, 1000.0, 0.0, 110.0);
        assert_eq!(scroll, 0.0);
        assert_eq!(drag.to(view, 1000.0, 190.0), 400.0);
        assert_eq!(drag.to(view, 1000.0, 400.0), 800.0);
        assert_eq!(drag.to(view, 1000.0, 0.0), 0.0);
        // Pressed below the thumb: it jumps to centre under the mouse, then follows.
        let (drag, scroll) = ScrollDrag::press(view, 1000.0, 0.0, 200.0);
        assert_eq!(scroll, 400.0);
        assert_eq!(drag.to(view, 1000.0, 210.0), 450.0);
        // Everything fits: nothing to drag.
        assert_eq!(ScrollDrag::press(view, 150.0, 0.0, 150.0).1, 0.0);
    }
}
