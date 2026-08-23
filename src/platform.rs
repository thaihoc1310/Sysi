use async_channel::Sender;
use std::{mem, ptr, thread};
use x11::xlib;

#[derive(Clone, Copy, Debug)]
pub enum HotkeyAction {
    ToggleInteraction,
}

// Xlib's default error handlers call exit(), which would silently kill the
// whole app (stale pid file, dead tray) on any X error — e.g. a conflicting
// grab or the display connection dropping. Ignore recoverable errors and end
// the thread quietly when the connection itself is lost.
unsafe extern "C" fn ignore_x_error(
    _display: *mut xlib::Display,
    _error: *mut xlib::XErrorEvent,
) -> i32 {
    0
}

unsafe extern "C" fn ignore_x_io_error(_display: *mut xlib::Display) -> i32 {
    0
}

pub fn spawn_global_hotkey(sender: Sender<HotkeyAction>) -> bool {
    if std::env::var("XDG_SESSION_TYPE").unwrap_or_default() != "x11" {
        return false;
    }
    thread::Builder::new()
        .name("sysi-hotkey".into())
        .spawn(move || unsafe {
            xlib::XSetErrorHandler(Some(ignore_x_error));
            xlib::XSetIOErrorHandler(Some(ignore_x_io_error));
            let display = xlib::XOpenDisplay(ptr::null());
            if display.is_null() {
                return;
            }
            let root = xlib::XDefaultRootWindow(display);
            let toggle_keycode = xlib::XKeysymToKeycode(display, b'o' as u64);
            let base = xlib::ControlMask | xlib::Mod1Mask;
            for extra in [
                0,
                xlib::LockMask,
                xlib::Mod2Mask,
                xlib::LockMask | xlib::Mod2Mask,
            ] {
                xlib::XGrabKey(
                    display,
                    toggle_keycode as i32,
                    base | extra,
                    root,
                    xlib::True,
                    xlib::GrabModeAsync,
                    xlib::GrabModeAsync,
                );
            }
            xlib::XSync(display, xlib::False);
            loop {
                let mut event: xlib::XEvent = mem::zeroed();
                if xlib::XNextEvent(display, &mut event) == 0 {
                    // The display connection is gone (display reset, session
                    // teardown). Stop polling instead of exiting the app.
                    break;
                }
                if event.get_type() == xlib::KeyPress
                    && sender
                        .send_blocking(HotkeyAction::ToggleInteraction)
                        .is_err()
                {
                    break;
                }
            }
            xlib::XCloseDisplay(display);
        })
        .is_ok()
}
