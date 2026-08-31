mod platform;
mod state;
mod system;
mod translate;
mod ui;

use fs2::FileExt;
use gtk::prelude::*;
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    mem,
    path::{Path, PathBuf},
    process, ptr,
    rc::Rc,
};

const PANEL_EXTENSION_UUID: &str = "sysi-panel@thaihoc";

const PANEL_EXTENSION_METADATA: &str =
    include_str!("../packaging/gnome-shell-extension/metadata.json");
const PANEL_EXTENSION_JS: &str = include_str!("../packaging/gnome-shell-extension/extension.js");
const PANEL_EXTENSION_CSS: &str = include_str!("../packaging/gnome-shell-extension/stylesheet.css");

fn main() {
    prefer_x11_backend();
    if std::env::args().any(|arg| arg == "--install-panel-extension") {
        if let Err(error) = install_panel_extension() {
            eprintln!("Could not install the Sysi panel extension: {error}");
            process::exit(1);
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--toggle") {
        signal_or_restart(libc::SIGUSR1);
        return;
    }
    if let Some(action) = option_value("--panel-action") {
        if let Err(error) = write_panel_action(&action) {
            eprintln!("Could not send the Sysi panel action: {error}");
            process::exit(1);
        }
        match signal_running(libc::SIGWINCH) {
            Ok(()) => {}
            // The instance is gone (stale pid). "quit" has nothing to quit;
            // every other action starts a fresh instance that applies the
            // pending action once the UI is up.
            Err(_) if action == "quit" => {}
            Err(_) => spawn_instance(),
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--toggle-picker") {
        signal_or_restart(libc::SIGWINCH);
        return;
    }
    if std::env::args().any(|arg| arg == "--quit") {
        if signal_running(libc::SIGTERM).is_ok() {
            let _ = fs::remove_file(state::cache_dir().join("pid"));
        }
        return;
    }

    let Some(_instance_lock) = acquire_instance_lock() else {
        let _ = signal_running(libc::SIGUSR1);
        return;
    };
    if let Err(error) = install_panel_extension() {
        eprintln!("Could not refresh the Sysi panel extension: {error}");
    }
    write_pid();

    // SIGUSR1's default action terminates the process, so a toggle sent
    // while this instance is still starting (handlers are registered in
    // build()) would kill it and leave a stale pid file. Block the signals
    // until build() unblocks them; any pending one then dispatches exactly
    // once through the freshly installed handlers.
    unsafe {
        let mut set: libc::sigset_t = mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::sigaddset(&mut set, libc::SIGWINCH);
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, ptr::null_mut());
    }

    let application = gtk::Application::new(
        Some("io.sysi.Overlay"),
        gtk::gio::ApplicationFlags::NON_UNIQUE,
    );
    // The panel's quit action and `sysi --quit` deliver SIGTERM. GLib's
    // default handling terminates the process without running shutdown
    // handlers, which would leave a stale pid file behind: the panel icon
    // keeps showing and every later click silently misses. Route it through
    // a clean GTK quit instead.
    glib::source::unix_signal_add_local(libc::SIGTERM, {
        let application = application.clone();
        move || {
            application.quit();
            glib::ControlFlow::Continue
        }
    });
    let state = Rc::new(RefCell::new(state::AppState::load()));
    application.connect_activate({
        let state = state.clone();
        move |app| ui::build(app, state.clone())
    });
    application.connect_shutdown(move |_| {
        let _ = state.borrow().save();
        let _ = fs::remove_file(state::cache_dir().join("pid"));
        let _ = fs::remove_file(state::cache_dir().join("panel-state"));
    });
    application.run();
}

// Ubuntu 26.04 ships a Wayland-only GNOME session, and GDK 3 picks its Wayland
// backend whenever WAYLAND_DISPLAY is set. That backend has no answer for what
// this overlay is: keep-above, skip-taskbar, stick, and placing one window
// across every monitor are all X11 window-management, and on Wayland they are
// silent no-ops that leave the overlay behaving like an ordinary window.
// Xwayland is always running in that session, so ask for the X11 backend and
// keep the full behaviour. An explicit GDK_BACKEND from the user still wins,
// and a session with no X display at all is left to GDK's own choice.
fn prefer_x11_backend() {
    if std::env::var_os("GDK_BACKEND").is_some() {
        return;
    }
    if std::env::var_os("DISPLAY").is_none() {
        return;
    }
    std::env::set_var("GDK_BACKEND", "x11");
}

fn install_panel_extension() -> io::Result<()> {
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .map(Into::into)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".local/share"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let extension_dir = data_dir
        .join("gnome-shell/extensions")
        .join(PANEL_EXTENSION_UUID);
    fs::create_dir_all(&extension_dir)?;
    write_if_changed(
        &extension_dir.join("metadata.json"),
        PANEL_EXTENSION_METADATA,
    )?;
    write_if_changed(&extension_dir.join("extension.js"), PANEL_EXTENSION_JS)?;
    // GNOME loads stylesheet.css from the extension directory on its own; the
    // strip's whole look lives there rather than in inline styles.
    write_if_changed(&extension_dir.join("stylesheet.css"), PANEL_EXTENSION_CSS)
}

fn write_if_changed(path: &Path, contents: &str) -> io::Result<()> {
    if fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    fs::write(path, contents)
}

fn option_value(option: &str) -> Option<String> {
    let mut args = std::env::args();
    while let Some(arg) = args.next() {
        if arg == option {
            return args.next().filter(|value| !value.is_empty());
        }
    }
    None
}

fn write_panel_action(action: &str) -> io::Result<()> {
    let dir = state::cache_dir();
    fs::create_dir_all(&dir)?;
    // Appended rather than written over. Two panel clicks in quick succession
    // are two separate processes writing here, and SIGWINCH coalesces, so one
    // signal may have to carry both — overwriting simply dropped the first.
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("panel-action"))?;
    writeln!(file, "{action}")
}

fn acquire_instance_lock() -> Option<File> {
    let dir = state::cache_dir();
    fs::create_dir_all(&dir).ok()?;
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(dir.join("instance.lock"))
        .ok()?;
    file.try_lock_exclusive().ok()?;
    Some(file)
}

fn write_pid() {
    let dir = state::cache_dir();
    if fs::create_dir_all(&dir).is_ok() {
        if let Ok(mut file) = File::create(dir.join("pid")) {
            let _ = writeln!(file, "{}", process::id());
        }
    }
}

fn signal_running(signal: i32) -> io::Result<()> {
    let path = state::cache_dir().join("pid");
    let Some(pid) = read_pid(&path) else {
        return Err(not_running());
    };
    // A recycled pid would receive the signal for free; only signal an
    // actual sysi process, and drop a stale pid file so the panel icon
    // stops trusting it.
    if !is_sysi_process(pid) {
        let _ = fs::remove_file(&path);
        return Err(not_running());
    }
    let result = unsafe { libc::kill(pid, signal) };
    if result != 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            let _ = fs::remove_file(&path);
            return Err(not_running());
        }
        return Err(error);
    }
    Ok(())
}

fn not_running() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "Sysi is not running")
}

fn is_sysi_process(pid: i32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim() == "sysi")
        .unwrap_or(false)
}

fn signal_or_restart(signal: i32) {
    if signal_running(signal).is_err() {
        spawn_instance();
    }
}

fn spawn_instance() {
    let executable = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sysi"));
    let _ = std::process::Command::new(executable)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .spawn();
}

fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
