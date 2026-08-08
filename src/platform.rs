use async_channel::Sender;
use std::{mem, ptr, thread};
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
                    if sender.send_blocking(action).is_err() {
                        break;
                    }
                }
            }
            xlib::XCloseDisplay(display);
        })
        .is_ok()
}
