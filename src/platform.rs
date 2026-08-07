use std::{mem, ptr, sync::mpsc::Sender, thread};
use x11::xlib;

#[derive(Clone, Copy, Debug)]
pub enum HotkeyAction {
    ToggleInteraction,
    ToggleSettings,
}

pub fn spawn_global_hotkey(sender: Sender<HotkeyAction>) -> bool {
    if std::env::var("XDG_SESSION_TYPE").unwrap_or_default() != "x11" {
        return false;
    }
    thread::Builder::new()
        .name("sysi-hotkey".into())
        .spawn(move || unsafe {
            let display = xlib::XOpenDisplay(ptr::null());
            if display.is_null() {
                return;
            }
            let root = xlib::XDefaultRootWindow(display);
            let toggle_keycode = xlib::XKeysymToKeycode(display, b'o' as u64);
            let settings_keycode = xlib::XKeysymToKeycode(display, b'g' as u64);
            let base = xlib::ControlMask | xlib::Mod1Mask;
            for extra in [
                0,
                xlib::LockMask,
                xlib::Mod2Mask,
                xlib::LockMask | xlib::Mod2Mask,
            ] {
                for keycode in [toggle_keycode, settings_keycode] {
                    xlib::XGrabKey(
                        display,
                        keycode as i32,
                        base | extra,
                        root,
                        xlib::True,
                        xlib::GrabModeAsync,
                        xlib::GrabModeAsync,
                    );
                }
            }
            xlib::XSync(display, xlib::False);
            loop {
                let mut event: xlib::XEvent = mem::zeroed();
                xlib::XNextEvent(display, &mut event);
                if event.get_type() == xlib::KeyPress {
                    let action = if event.key.keycode == settings_keycode as u32 {
                        HotkeyAction::ToggleSettings
                    } else {
                        HotkeyAction::ToggleInteraction
                    };
                    if sender.send(action).is_err() {
                        break;
                    }
                }
            }
            xlib::XCloseDisplay(display);
        })
        .is_ok()
}

pub fn sample_luminance(x: i32, y: i32, width: i32, height: i32) -> Option<f64> {
    if std::env::var("XDG_SESSION_TYPE").unwrap_or_default() != "x11" {
        return None;
    }
    unsafe {
        let display = xlib::XOpenDisplay(ptr::null());
        if display.is_null() {
            return None;
        }
        let screen = xlib::XDefaultScreen(display);
        let screen_w = xlib::XDisplayWidth(display, screen).max(1);
        let screen_h = xlib::XDisplayHeight(display, screen).max(1);
        let sample_w = width.clamp(1, 32).min(screen_w);
        let sample_h = height.clamp(1, 32).min(screen_h);
        let sx = x.clamp(0, screen_w - sample_w);
        let sy = y.clamp(0, screen_h - sample_h);
        let image = xlib::XGetImage(
            display,
            xlib::XDefaultRootWindow(display),
            sx,
            sy,
            sample_w as u32,
            sample_h as u32,
            xlib::XAllPlanes(),
            xlib::ZPixmap,
        );
        if image.is_null() {
            xlib::XCloseDisplay(display);
            return None;
        }

        let red_mask = (*image).red_mask;
        let green_mask = (*image).green_mask;
        let blue_mask = (*image).blue_mask;
        let mut sum = 0.0;
        let mut count = 0.0;
        for py in (0..sample_h).step_by(4) {
            for px in (0..sample_w).step_by(4) {
                let pixel = xlib::XGetPixel(image, px, py);
                let red = channel(pixel, red_mask);
                let green = channel(pixel, green_mask);
                let blue = channel(pixel, blue_mask);
                sum += 0.2126 * red + 0.7152 * green + 0.0722 * blue;
                count += 1.0;
            }
        }
        xlib::XDestroyImage(image);
        xlib::XCloseDisplay(display);
        (count > 0.0).then_some(sum / count)
    }
}

fn channel(pixel: u64, mask: u64) -> f64 {
    if mask == 0 {
        return 0.0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    ((pixel & mask) >> shift) as f64 / max as f64
}
