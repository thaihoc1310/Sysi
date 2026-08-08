mod platform;
mod state;
mod system;
mod ui;

use fs2::FileExt;
use gtk::prelude::*;
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
    process,
    rc::Rc,
};

const PANEL_EXTENSION_UUID: &str = "sysi-panel@thaihoc";
const PANEL_EXTENSION_METADATA: &str =
    include_str!("../packaging/gnome-shell-extension/metadata.json");
const PANEL_EXTENSION_JS: &str = include_str!("../packaging/gnome-shell-extension/extension.js");

fn main() {
    if std::env::args().any(|arg| arg == "--install-panel-extension") {
        if let Err(error) = install_panel_extension() {
            eprintln!("Could not install the Sysi panel extension: {error}");
            process::exit(1);
        }
        return;
    }
    if std::env::args().any(|arg| arg == "--toggle") {
        signal_running(libc::SIGUSR1);
        return;
    }
    if let Some(action) = option_value("--panel-action") {
        if let Err(error) = write_panel_action(&action) {
            eprintln!("Could not send the Sysi panel action: {error}");
            process::exit(1);
        }
        signal_running(libc::SIGWINCH);
        return;
    }
    if std::env::args().any(|arg| arg == "--toggle-picker") {
        signal_running(libc::SIGWINCH);
        return;
    }
    if std::env::args().any(|arg| arg == "--quit") {
        signal_running(libc::SIGTERM);
        return;
    }

    let Some(_instance_lock) = acquire_instance_lock() else {
        signal_running(libc::SIGUSR1);
        return;
    };
    if let Err(error) = install_panel_extension() {
        eprintln!("Could not refresh the Sysi panel extension: {error}");
    }
    write_pid();

    let application = gtk::Application::new(
        Some("io.sysi.Overlay"),
        gtk::gio::ApplicationFlags::NON_UNIQUE,
    );
    let state = Rc::new(RefCell::new(state::AppState::load()));
    application.connect_activate({
        let state = state.clone();
        move |app| ui::build(app, state.clone())
    });
    application.connect_shutdown(move |_| {
        let _ = state.borrow().save();
        let _ = fs::remove_file(state::cache_dir().join("pid"));
    });
    application.run();
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
    write_if_changed(&extension_dir.join("extension.js"), PANEL_EXTENSION_JS)
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
    fs::write(dir.join("panel-action"), action)
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

fn signal_running(signal: i32) {
    let path = state::cache_dir().join("pid");
    let Some(pid) = read_pid(&path) else {
        eprintln!("Sysi is not running yet.");
        process::exit(1);
    };
    let result = unsafe { libc::kill(pid, signal) };
    if result != 0 {
        eprintln!("Could not contact Sysi process {pid}.");
        process::exit(1);
    }
}

fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}
