//! Band ("telop") mode: a borderless, translucent subtitle strip floating over other windows.

use eframe::egui::{Pos2, Rect, Vec2};
use objc2::rc::Retained;
use objc2_app_kit::{NSColor, NSView, NSWindow, NSWindowCollectionBehavior};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

/// NSStatusWindowLevel: above ordinary floating windows, so the band stays over video players.
const BAND_WINDOW_LEVEL: isize = 25;

/// The display (in macOS points, top-left origin) that contains `point`, or the main display.
pub fn display_rect_containing(point: Pos2) -> Rect {
    use core_graphics::display::CGDisplay;
    let rect_of = |id| {
        let b = CGDisplay::new(id).bounds();
        Rect::from_min_size(
            Pos2::new(b.origin.x as f32, b.origin.y as f32),
            Vec2::new(b.size.width as f32, b.size.height as f32),
        )
    };
    CGDisplay::active_displays()
        .unwrap_or_default()
        .into_iter()
        .map(rect_of)
        .find(|r| r.contains(point))
        .unwrap_or_else(|| rect_of(CGDisplay::main().id))
}

/// Height of the band for the given UI font size (two lines of subtitle plus the original line).
pub fn band_height(font_size_points: f32) -> f32 {
    font_size_points * (1.9 * 2.7 + 1.7) + 36.0
}

/// The top-left corner of a window of `size` centered on `center`, kept inside `display`. The band opens
/// where the normal window was, so it is never somewhere the user does not expect it.
pub fn centered_on(display: Rect, center: Pos2, size: Vec2) -> Pos2 {
    let margin = 8.0;
    let place = |center: f32, size: f32, min: f32, max: f32| {
        let lowest = min + margin;
        (center - size / 2.0).clamp(lowest, (max - size - margin).max(lowest))
    };
    Pos2::new(
        place(center.x, size.x, display.min.x, display.max.x),
        place(center.y, size.y, display.min.y, display.max.y),
    )
}

/// The font-size unit that makes the band's content fill a band of this height.
pub fn unit_for_height(height: f32) -> f32 {
    ((height - 36.0) / (1.9 * 2.7 + 1.7)).max(8.0)
}

fn native_window(frame: &eframe::Frame) -> Option<Retained<NSWindow>> {
    let RawWindowHandle::AppKit(handle) = frame.window_handle().ok()?.as_raw() else {
        return None;
    };
    // SAFETY: the handle points at the live NSView of this application's window, and this runs on the main thread.
    let view: &NSView = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

/// Applies the macOS window traits the band needs; `false` makes it an ordinary opaque window again.
pub fn configure_window(frame: &eframe::Frame, band: bool) {
    let Some(window) = native_window(frame) else {
        return;
    };
    if band {
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces | NSWindowCollectionBehavior::FullScreenAuxiliary,
        );
        window.setLevel(BAND_WINDOW_LEVEL);
        window.setHasShadow(false);
        window.setOpaque(false);
        window.setBackgroundColor(Some(&NSColor::clearColor()));
    } else {
        // The window is created transparent (the band needs that), so the ordinary window must be made opaque
        // again or its title bar would show whatever is behind it.
        window.setCollectionBehavior(NSWindowCollectionBehavior::Default);
        window.setHasShadow(true);
        window.setOpaque(true);
        window.setBackgroundColor(None);
    }
}
