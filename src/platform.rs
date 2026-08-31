use async_channel::Sender;
use std::{
    mem, ptr,
    sync::atomic::{AtomicBool, Ordering},
    thread,
};
use x11::xlib;

/// Set when a grab is refused. X reports that asynchronously, so the only way
/// to hear about it is to catch the error and look afterwards — and a hotkey
/// that quietly does nothing for the rest of the session is worth a line on
/// stderr rather than a mystery.
static GRAB_REFUSED: AtomicBool = AtomicBool::new(false);

#[derive(Clone, Copy, Debug)]
pub enum HotkeyAction {
    ToggleInteraction,
}

// Xlib's default error handler calls exit(), which would silently kill the
// whole app (stale pid file, dead tray) on a recoverable protocol error such as
// a grab another client already holds. Swallowing those is safe and is what
// this is for.
unsafe extern "C" fn ignore_x_error(
    _display: *mut xlib::Display,
    error: *mut xlib::XErrorEvent,
) -> i32 {
    // BadAccess from XGrabKey means someone else already holds the combination.
    if !error.is_null() && (*error).error_code == xlib::BadAccess {
        GRAB_REFUSED.store(true, Ordering::Relaxed);
    }
    0
}

// An I/O error means the connection itself is gone, and Xlib exits the process
// if this returns — there is no "ignore" to be had. It is installed anyway to
// keep the default handler's message off stderr; the loop below is what
// actually keeps a dropped connection from reaching Xlib in the first place.
unsafe extern "C" fn ignore_x_io_error(_display: *mut xlib::Display) -> i32 {
    0
}

pub fn spawn_global_hotkey(sender: Sender<HotkeyAction>) -> bool {
    // What matters is the display this process actually talks to, not what the
    // session calls itself. Under a Wayland session the overlay still runs on
    // the X11 backend through Xwayland, where the grab is worth taking: it
    // fires whenever an X11 window holds focus. Keying off XDG_SESSION_TYPE
    // instead reported "wayland" there and refused the grab outright.
    if std::env::var_os("DISPLAY").is_none()
        || std::env::var("GDK_BACKEND").as_deref() == Ok("wayland")
    {
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
            if GRAB_REFUSED.swap(false, Ordering::Relaxed) {
                eprintln!(
                    "Sysi could not take Ctrl+Alt+O: another application already holds it. \
                     Lock and unlock from the panel strip instead."
                );
            }
            let connection = xlib::XConnectionNumber(display);
            'listen: loop {
                // Take what Xlib has already buffered. XNextEvent must only be
                // called once something is queued: it blocks otherwise, and its
                // return value says nothing at all — it is 0 whether or not it
                // read an event, so reading it as an error code is what used to
                // end this thread on the very first key press and leave
                // Ctrl+Alt+O doing nothing for the rest of the session.
                while xlib::XPending(display) > 0 {
                    let mut event: xlib::XEvent = mem::zeroed();
                    xlib::XNextEvent(display, &mut event);
                    if event.get_type() == xlib::KeyPress
                        && sender
                            .send_blocking(HotkeyAction::ToggleInteraction)
                            .is_err()
                    {
                        // The overlay has gone; there is nothing left to toggle.
                        break 'listen;
                    }
                }
                // Wait on the socket rather than inside Xlib. A connection that
                // drops shows up here as a hangup we can walk away from, where
                // blocking in Xlib would reach the I/O error handler and take
                // the whole process with it.
                let mut socket = libc::pollfd {
                    fd: connection,
                    events: libc::POLLIN,
                    revents: 0,
                };
                if libc::poll(&mut socket, 1, -1) < 0 {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                if socket.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    break;
                }
            }
            xlib::XCloseDisplay(display);
        })
        .is_ok()
}
