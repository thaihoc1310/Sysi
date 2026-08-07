mod platform;
mod state;
mod system;
mod ui;

use fs2::FileExt;
use gtk::prelude::*;
use std::{
    cell::RefCell,
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
    process,
    rc::Rc,
};

fn main() {
    if std::env::args().any(|arg| arg == "--toggle") {
        signal_running(libc::SIGUSR1);
        return;
    }
    if std::env::args().any(|arg| arg == "--toggle-settings") {
        signal_running(libc::SIGUSR2);
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
