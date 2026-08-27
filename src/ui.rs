use crate::{
    platform,
    state::{AppState, ColorMode, Note, Point, Size, SystemDetails, TimerStyle},
    system::{SystemReader, SystemSnapshot},
};
use cairo::{Context, FontSlant, FontWeight, RectangleInt, Region};
use gdk::prelude::*;
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    f64::consts::{PI, TAU},
    fs,
    rc::Rc,
    time::{Duration, Instant},
};

const SYSTEM_WIDTH: i32 = 196;
const SYSTEM_HEIGHT: i32 = 76;
const SYSTEM_SINGLE_WIDTH: i32 = 76;
const TIMER_SIZE: i32 = 116;
const NOTE_WIDTH: i32 = 218;
const NOTE_HEIGHT: i32 = 124;
const HISTORY_WIDTH: i32 = 236;
const HISTORY_HEIGHT: i32 = 252;
// One rendered row (.note-preview padding + the inherited note font) plus the
// list spacing, and the header + list padding above it. Used to scale how many
// rows the window renders to how tall the user dragged it.
const HISTORY_ROW_HEIGHT: i32 = 30;
const HISTORY_CHROME_HEIGHT: i32 = 32;
const RESIZE_HIT_SIZE: i32 = 18;

type CallbackSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
type SystemValues = Rc<RefCell<SystemSnapshot>>;

struct SystemCard {
    card: gtk::EventBox,
    drag: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    canvas: gtk::DrawingArea,
    values: SystemValues,
    details: Rc<Cell<SystemDetails>>,
    resize: ResizeHandle,
}

#[derive(Clone)]
struct SystemDetailsPreview {
    card: gtk::EventBox,
    canvas: gtk::DrawingArea,
    values: SystemValues,
    details: Rc<Cell<SystemDetails>>,
}

#[derive(Clone)]
struct ResizeHandle {
    hitbox: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
}

#[derive(Clone, Copy)]
struct ResizeBounds {
    min_width: i32,
    min_height: i32,
    max_width: i32,
    max_height: i32,
    aspect_ratio: Option<f64>,
    preserve_current_aspect: bool,
}

#[derive(Clone)]
struct RegisteredWidget {
    key: String,
    widget: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    edit_only: Option<gtk::EventBox>,
    editor: Option<gtk::TextView>,
}

struct TimerRuntime {
    duration_seconds: i64,
    remaining: Duration,
    target: Option<Instant>,
    started: bool,
    alarm: bool,
    phase: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ScreenRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

pub fn build(app: &gtk::Application, state: Rc<RefCell<AppState>>) {
    let window = gtk::ApplicationWindow::new(app);
    window.set_title("Sysi Overlay");
    window.set_decorated(false);
    window.set_resizable(false);
    window.set_app_paintable(true);
    window.set_keep_above(true);
    window.set_skip_taskbar_hint(true);
    window.set_skip_pager_hint(true);
    window.set_accept_focus(false);
    window.set_focus_on_map(false);
    window.stick();
    // Dock windows cannot receive keyboard focus. Utility keeps this overlay out
    // of GNOME's panel layer while remaining a focusable edit surface.
    window.set_type_hint(gdk::WindowTypeHint::Utility);

    if let Some(screen) = gtk::prelude::WidgetExt::screen(&window) {
        if let Some(visual) = screen.rgba_visual() {
            window.set_visual(Some(&visual));
        }
    }

    let screen = gdk::Screen::default().expect("Sysi requires a graphical display");
    let root_window = screen.root_window().expect("display root window");
    let scale = root_window.scale_factor().max(1);
    let screen_width = root_window.width() / scale;
    let screen_height = root_window.height() / scale;
    let screens = logical_screen_rects(scale, screen_width, screen_height);
    let primary_screen =
        logical_primary_screen(scale, screen_width, screen_height).unwrap_or(screens[0]);
    {
        let mut data = state.borrow_mut();
        if data.layout_version < 3 {
            data.positions.insert(
                "system".into(),
                Point {
                    x: primary_screen.x + 28,
                    y: primary_screen.y + 78,
                },
            );
            data.positions.insert(
                "timer".into(),
                Point {
                    x: primary_screen.x + primary_screen.width - TIMER_SIZE - 30,
                    y: primary_screen.y + 68,
                },
            );
            data.positions.insert(
                "notes".into(),
                Point {
                    x: primary_screen.x + 32,
                    y: primary_screen.y + 270,
                },
            );
            data.positions.insert(
                "picker".into(),
                Point {
                    x: primary_screen.x + primary_screen.width / 2 - 30,
                    y: primary_screen.y + 34,
                },
            );
            for (index, note) in data.notes.iter_mut().filter(|note| note.pinned).enumerate() {
                note.position = Point {
                    x: primary_screen.x + 330 + (index as i32 % 3) * 245,
                    y: primary_screen.y + 250 + (index as i32 / 3) * 170,
                };
            }
            data.layout_version = 3;
            let _ = data.save();
        }
        if data.layout_version < 4 {
            let old_size = data.sizes.get("timer").copied();
            let compact_size = match data.timer_style {
                TimerStyle::Digital
                    if old_size
                        == Some(Size {
                            width: 108,
                            height: 48,
                        }) =>
                {
                    Some(TimerStyle::Digital.default_size())
                }
                TimerStyle::Ring | TimerStyle::Ticks | TimerStyle::Arc
                    if old_size
                        == Some(Size {
                            width: 132,
                            height: 132,
                        }) =>
                {
                    Some(data.timer_style.default_size())
                }
                _ => None,
            };
            if let Some(size) = compact_size {
                data.sizes.insert("timer".into(), size);
            }
            data.layout_version = 4;
            let _ = data.save();
        }
        if data.layout_version < 5 {
            if data.timer_style == TimerStyle::Digital
                && data.sizes.get("timer").copied()
                    == Some(Size {
                        width: 96,
                        height: 40,
                    })
            {
                data.sizes
                    .insert("timer".into(), TimerStyle::Digital.default_size());
            }
            data.layout_version = 5;
            let _ = data.save();
        }
        if data.layout_version < 6 {
            let details = data.settings.system_details;
            if system_meter_count(details) == 1
                && !details.processes
                && !details.cores
                && data.sizes.get("system").copied()
                    == Some(Size {
                        width: SYSTEM_WIDTH,
                        height: SYSTEM_HEIGHT,
                    })
            {
                data.sizes.insert(
                    "system".into(),
                    Size {
                        width: SYSTEM_SINGLE_WIDTH,
                        height: SYSTEM_HEIGHT,
                    },
                );
            }
            data.layout_version = 6;
            let _ = data.save();
        }
    }
    // The overlay covers the X root window, which always spans every
    // monitor; monitor-derived bounds would double-count Xinerama screens.
    window.set_default_size(screen_width, screen_height);
    window.move_(0, 0);

    install_css(&screen);

    let root = gtk::Fixed::new();
    root.set_hexpand(true);
    root.set_vexpand(true);
    root.style_context().add_class("overlay-root");
    window.add(&root);
    window.connect_draw(|_, ctx| {
        let _ = ctx.save();
        ctx.set_operator(cairo::Operator::Source);
        ctx.set_source_rgba(0.0, 0.0, 0.0, 0.0);
        let _ = ctx.paint();
        let _ = ctx.restore();
        glib::Propagation::Proceed
    });

    let registry: Rc<RefCell<Vec<RegisteredWidget>>> = Rc::new(RefCell::new(Vec::new()));
    let interactive = Rc::new(Cell::new(true));
    window.set_accept_focus(true);
    window.style_context().add_class("editing");
    let (system_color_mode, timer_color_mode, picker_color_mode, system_details) = {
        let data = state.borrow();
        (
            saved_color_mode(&data, "system"),
            saved_color_mode(&data, "timer"),
            saved_color_mode(&data, "picker"),
            data.settings.system_details,
        )
    };

    let system_card = build_system_card(system_color_mode, system_details);
    apply_widget_size(
        &system_card.card,
        "system",
        &state,
        system_content_size(system_details, 0),
    );
    place_card(
        &root,
        &system_card.card,
        state
            .borrow()
            .positions
            .get("system")
            .copied()
            .unwrap_or(Point { x: 34, y: 52 }),
    );
    register(
        &registry,
        "system",
        &system_card.card,
        system_card.color_mode.clone(),
    );
    attach_color_mode_menu(
        &system_card.card,
        "system".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        None,
        Some(SystemDetailsPreview {
            card: system_card.card.clone(),
            canvas: system_card.canvas.clone(),
            values: system_card.values.clone(),
            details: system_card.details.clone(),
        }),
    );
    attach_drag(
        &system_card.drag,
        &system_card.card,
        &root,
        "system".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
    );
    attach_resize(
        &system_card.resize,
        &system_card.card,
        &root,
        "system".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
        ResizeBounds {
            min_width: 72,
            min_height: 64,
            max_width: 520,
            max_height: 640,
            aspect_ratio: None,
            preserve_current_aspect: false,
        },
    );

    let timer_card = build_timer_card(state.clone(), interactive.clone(), timer_color_mode);
    let timer_default_size = timer_card.style.get().default_size();
    let timer_default = Point {
        x: (primary_screen.x + primary_screen.width - timer_default_size.width - 34)
            .max(primary_screen.x + 8),
        y: primary_screen.y + 34,
    };
    let timer_position = state
        .borrow()
        .positions
        .get("timer")
        .copied()
        .unwrap_or(timer_default);
    apply_widget_size(&timer_card.card, "timer", &state, timer_default_size);
    place_card(&root, &timer_card.card, timer_position);
    register(
        &registry,
        "timer",
        &timer_card.card,
        timer_card.color_mode.clone(),
    );
    attach_color_mode_menu(
        &timer_card.card,
        "timer".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        Some(TimerStylePreview {
            style: timer_card.style.clone(),
            size: timer_card.style_size.clone(),
            card: timer_card.card.clone(),
            canvas: timer_card.canvas.clone(),
            window: window.clone(),
            registry: registry.clone(),
            interactive: interactive.clone(),
            typography: timer_card.typography.clone(),
            open_edit: timer_card.open_edit.clone(),
        }),
        None,
    );
    attach_drag(
        &timer_card.drag,
        &timer_card.card,
        &root,
        "timer".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
    );
    attach_resize(
        &timer_card.resize,
        &timer_card.card,
        &root,
        "timer".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
        ResizeBounds {
            min_width: 72,
            min_height: 36,
            max_width: 320,
            max_height: 320,
            aspect_ratio: Some(1.0),
            preserve_current_aspect: true,
        },
    );

    let widget_picker = build_widget_picker(picker_color_mode);
    let picker_position = Point {
        x: primary_screen.x + 12,
        y: primary_screen.y + 30,
    };
    place_card(&root, &widget_picker.card, picker_position);
    register(
        &registry,
        "picker",
        &widget_picker.card,
        widget_picker.color_mode.clone(),
    );
    attach_color_mode_menu(
        &widget_picker.card,
        "picker".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        None,
        None,
    );
    attach_drag(
        &widget_picker.drag,
        &widget_picker.card,
        &root,
        "picker".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
    );
    let history = build_history_window(saved_color_mode(&state.borrow(), "history"));
    let history_position = state
        .borrow()
        .positions
        .get("history")
        .copied()
        .unwrap_or(Point {
            x: primary_screen.x + 28,
            y: primary_screen.y + 186,
        });
    apply_widget_size(
        &history.card,
        "history",
        &state,
        Size {
            width: HISTORY_WIDTH,
            height: HISTORY_HEIGHT,
        },
    );
    place_card(&root, &history.card, history_position);
    register(
        &registry,
        "history",
        &history.card,
        history.color_mode.clone(),
    );
    if let Some(item) = registry
        .borrow_mut()
        .iter_mut()
        .find(|item| item.key == "history")
    {
        item.edit_only = Some(history.header.clone());
    }
    attach_color_mode_menu(
        &history.card,
        "history".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        None,
        None,
    );
    attach_drag(
        &history.header,
        &history.card,
        &root,
        "history".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
    );
    attach_resize(
        &history.resize,
        &history.card,
        &root,
        "history".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
        ResizeBounds {
            min_width: 132,
            min_height: 92,
            max_width: 620,
            max_height: 760,
            aspect_ratio: None,
            preserve_current_aspect: false,
        },
    );

    let note_refresh: CallbackSlot = Rc::new(RefCell::new(None));
    // How many rows the list renders, derived from the window height so
    // dragging the grip down shows more entries and dragging it up shows
    // fewer. Kept in a cell so every rebuild path agrees on the current one.
    let history_limit = Rc::new(Cell::new(history_row_budget(
        state
            .borrow()
            .sizes
            .get("history")
            .map(|size| size.height)
            .filter(|height| *height > 0)
            .unwrap_or(HISTORY_HEIGHT),
    )));
    // Search mode is tracked explicitly: show_all() (on unlock, or when the
    // window is reopened) would otherwise reveal both header bars at once.
    let searching = Rc::new(Cell::new(false));

    let refresh_closure: Rc<dyn Fn()> = {
        let root = root.clone();
        let state = state.clone();
        let registry = registry.clone();
        let list = history.list.clone();
        let note_refresh = note_refresh.clone();
        let interactive = interactive.clone();
        let window = window.clone();
        let search = history.bar.search.clone();
        let history_limit = history_limit.clone();
        Rc::new(move || {
            rebuild_note_list(
                &list,
                &root,
                state.clone(),
                note_refresh.clone(),
                &search.text(),
                history_limit.get(),
            );
            rebuild_pinned_notes(
                &root,
                state.clone(),
                registry.clone(),
                note_refresh.clone(),
                interactive.clone(),
                window.clone(),
            );
            refresh_input_shape(&window, &registry, interactive.get());
            glib::idle_add_local_once({
                let window = window.clone();
                let registry = registry.clone();
                let interactive = interactive.clone();
                move || refresh_input_shape(&window, &registry, interactive.get())
            });
        })
    };
    *note_refresh.borrow_mut() = Some(refresh_closure.clone());
    refresh_closure();

    // Typing in the search box rebuilds only the history rows — pinned notes
    // and the input shape are untouched — and the rebuild is debounced so a
    // fast typing burst stays smooth.
    let refresh_history: Rc<dyn Fn()> = {
        let list = history.list.clone();
        let root = root.clone();
        let state = state.clone();
        let note_refresh = note_refresh.clone();
        let search = history.bar.search.clone();
        let history_limit = history_limit.clone();
        Rc::new(move || {
            rebuild_note_list(
                &list,
                &root,
                state.clone(),
                note_refresh.clone(),
                &search.text(),
                history_limit.get(),
            );
        })
    };
    let pending_search: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    history.bar.search.connect_changed({
        let refresh_history = refresh_history.clone();
        let pending_search = pending_search.clone();
        move |_| {
            if let Some(source) = pending_search.borrow_mut().take() {
                source.remove();
            }
            let refresh_history = refresh_history.clone();
            let pending_for_timer = pending_search.clone();
            let source = glib::timeout_add_local_once(Duration::from_millis(120), move || {
                pending_for_timer.borrow_mut().take();
                refresh_history();
            });
            *pending_search.borrow_mut() = Some(source);
        }
    });

    // Resizing changes how many rows fit, so re-render when the row budget
    // actually changes — never on every allocation, which would loop.
    history.card.connect_size_allocate({
        let history_limit = history_limit.clone();
        let refresh_history = refresh_history.clone();
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        move |_, allocation| {
            let next = history_row_budget(allocation.height());
            if next == history_limit.get() {
                return;
            }
            history_limit.set(next);
            // Adding rows from inside size-allocate re-enters GTK's layout;
            // defer to the next idle instead.
            if let Some(source) = pending.borrow_mut().take() {
                source.remove();
            }
            let refresh_history = refresh_history.clone();
            let pending_for_idle = pending.clone();
            let source = glib::idle_add_local_once(move || {
                pending_for_idle.borrow_mut().take();
                refresh_history();
            });
            *pending.borrow_mut() = Some(source);
        }
    });

    // The search icon turns the header into the search field, plus the "\u{00d7}"
    // that puts the plain title bar back.
    history.bar.open_search.connect_clicked({
        let bar = history.bar.clone();
        let window = window.clone();
        let searching = searching.clone();
        move |_| {
            searching.set(true);
            bar.set_search_mode(true);
            present_overlay(&window);
            bar.search.grab_focus();
        }
    });
    let close_history_search: Rc<dyn Fn()> = {
        let bar = history.bar.clone();
        let searching = searching.clone();
        Rc::new(move || {
            searching.set(false);
            bar.set_search_mode(false);
            // Clearing fires `changed`, which reruns the (now empty) query and
            // restores the full list.
            bar.search.set_text("");
        })
    };
    history.bar.close_search.connect_clicked({
        let close_history_search = close_history_search.clone();
        move |_| close_history_search()
    });

    let toggle_history: Rc<dyn Fn()> = {
        let card = history.card.clone();
        let header = history.header.clone();
        let bar = history.bar.clone();
        let searching = searching.clone();
        let state = state.clone();
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        let root = root.clone();
        let screens = screens.clone();
        let picker = widget_picker.card.clone();
        Rc::new(move || {
            let open = !card.is_visible();
            if open {
                // Always come back near the click, so a window left
                // half off-screen is reachable again by its header.
                reopen_widget(
                    &card,
                    "history",
                    &root,
                    &state,
                    &screens,
                    primary_screen,
                    Size {
                        width: HISTORY_WIDTH,
                        height: HISTORY_HEIGHT,
                    },
                    Some(&picker),
                    &registry,
                );
                card.show_all();
                // show_all() reveals both slot occupants and the header
                // itself; restore the search mode and the lock-mode rule.
                bar.set_search_mode(searching.get());
                header.set_visible(interactive.get());
            } else {
                card.hide();
            }
            state.borrow_mut().settings.history_open = open;
            let _ = state.borrow().save();
            refresh_input_shape(&window, &registry, interactive.get());
            glib::idle_add_local_once({
                let window = window.clone();
                let registry = registry.clone();
                let interactive = interactive.clone();
                let root = root.clone();
                let screens = screens.clone();
                let state = state.clone();
                move || {
                    if open {
                        clamp_registered_widgets(&root, &registry, &screens, &state);
                    }
                    refresh_input_shape(&window, &registry, interactive.get());
                }
            });
        })
    };
    history.hide.connect_clicked({
        let toggle_history = toggle_history.clone();
        move |_| toggle_history()
    });

    let (system_enabled, timer_enabled, color_mode) = {
        let settings = &state.borrow().settings;
        (settings.system, settings.timer, settings.color_mode)
    };
    widget_picker.system.set_active(system_enabled);
    widget_picker.timer.set_active(timer_enabled);
    widget_picker.mode.set_label(color_mode.label());

    widget_picker.system.connect_toggled({
        let target = system_card.card.clone();
        let state = state.clone();
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        let root = root.clone();
        let screens = screens.clone();
        let picker = widget_picker.card.clone();
        move |button| {
            let enabled = button.is_active();
            if enabled {
                reopen_widget(
                    &target,
                    "system",
                    &root,
                    &state,
                    &screens,
                    primary_screen,
                    Size {
                        width: SYSTEM_WIDTH,
                        height: SYSTEM_HEIGHT,
                    },
                    Some(&picker),
                    &registry,
                );
                target.show_all();
            } else {
                target.hide();
            }
            state.borrow_mut().settings.system = enabled;
            let _ = state.borrow().save();
            refresh_input_shape(&window, &registry, interactive.get());
        }
    });
    widget_picker.timer.connect_toggled({
        let target = timer_card.card.clone();
        let state = state.clone();
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        let root = root.clone();
        let screens = screens.clone();
        let picker = widget_picker.card.clone();
        move |button| {
            let enabled = button.is_active();
            if enabled {
                reopen_widget(
                    &target,
                    "timer",
                    &root,
                    &state,
                    &screens,
                    primary_screen,
                    Size {
                        width: TIMER_SIZE,
                        height: TIMER_SIZE,
                    },
                    Some(&picker),
                    &registry,
                );
                target.show_all();
            } else {
                target.hide();
            }
            state.borrow_mut().settings.timer = enabled;
            let _ = state.borrow().save();
            refresh_input_shape(&window, &registry, interactive.get());
        }
    });
    widget_picker.mode.connect_clicked({
        let state = state.clone();
        let registry = registry.clone();
        move |button| {
            let next = state.borrow().settings.color_mode.next();
            {
                let mut data = state.borrow_mut();
                data.settings.color_mode = next;
                data.widget_color_modes.clear();
            }
            let _ = state.borrow().save();
            button.set_label(next.label());
            apply_color_mode(&registry, next);
        }
    });

    widget_picker.new_note.connect_clicked({
        let root = root.clone();
        let revealer = widget_picker.revealer.clone();
        let picker = widget_picker.card.clone();
        let state = state.clone();
        let refresh = refresh_closure.clone();
        let screens = screens.clone();
        let registry = registry.clone();
        let window = window.clone();
        move |_| {
            // Open the note near the pointer (the action comes from the GNOME
            // panel, so this lands just under the menu) instead of a fixed
            // corner; fall back to the picker anchor without a pointer.
            let allocation = picker.allocation();
            let desired = pointer_position()
                .map(|(x, y)| Point {
                    x: x as i32 - NOTE_WIDTH / 2,
                    y: y as i32 + 24,
                })
                .unwrap_or(Point {
                    x: allocation.x() + 205,
                    y: allocation.y() + 40,
                });
            let mut position = clamp_to_screens(desired, NOTE_WIDTH, NOTE_HEIGHT, &screens);
            let mut data = state.borrow_mut();
            // Cascade so back-to-back notes don't stack invisibly on top of
            // each other.
            for _ in 0..12 {
                let occupied = data.notes.iter().any(|note| {
                    note.pinned
                        && (note.position.x - position.x).abs() < 12
                        && (note.position.y - position.y).abs() < 12
                });
                if !occupied {
                    break;
                }
                let next = clamp_to_screens(
                    Point {
                        x: position.x + 26,
                        y: position.y + 26,
                    },
                    NOTE_WIDTH,
                    NOTE_HEIGHT,
                    &screens,
                );
                if next == position {
                    break;
                }
                position = next;
            }
            let id = data.next_note_id;
            data.next_note_id += 1;
            data.notes.push(Note {
                id,
                text: String::new(),
                pinned: true,
                position,
            });
            let _ = data.save();
            drop(data);
            revealer.set_reveal_child(false);
            refresh();
            // Focus the fresh editor once the rebuilt card is mapped, so the
            // user can start typing immediately, like a native notes app.
            glib::idle_add_local_once({
                let registry = registry.clone();
                let window = window.clone();
                let key = format!("note:{id}");
                move || {
                    let registry = registry.borrow();
                    if let Some(editor) = registry
                        .iter()
                        .find(|item| item.key == key)
                        .and_then(|item| item.editor.clone())
                    {
                        present_overlay(&window);
                        editor.grab_focus();
                    }
                }
            });
            let _ = &root;
        }
    });
    widget_picker.quit.connect_clicked({
        let app = app.clone();
        move |_| app.quit()
    });

    let toggle_action: Rc<dyn Fn()> = {
        let interactive = interactive.clone();
        let window = window.clone();
        let registry = registry.clone();
        let lock = widget_picker.lock.clone();
        let commit_timer_edit = timer_card.commit_edit.clone();
        Rc::new(move || {
            let enabled = !interactive.get();
            interactive.set(enabled);
            if enabled {
                window.set_accept_focus(true);
                window.style_context().add_class("editing");
            } else {
                commit_timer_edit();
                window.set_accept_focus(false);
                window.style_context().remove_class("editing");
            }
            lock.set_label(if enabled { "LOCK" } else { "UNLOCK" });
            set_edit_chrome_visibility(&registry, enabled);
            for item in registry.borrow().iter() {
                item.widget.queue_draw();
            }
            refresh_input_shape(&window, &registry, enabled);
        })
    };

    widget_picker.lock.connect_clicked({
        let toggle_action = toggle_action.clone();
        move |_| toggle_action()
    });

    let dispatch_panel_action: Rc<dyn Fn()> = {
        let system = widget_picker.system.clone();
        let timer = widget_picker.timer.clone();
        let mode = widget_picker.mode.clone();
        let lock = widget_picker.lock.clone();
        let new_note = widget_picker.new_note.clone();
        let quit = widget_picker.quit.clone();
        let toggle_history = toggle_history.clone();
        let interactive = interactive.clone();
        Rc::new(move || match take_panel_action().as_deref() {
            Some("toggle-system") => system.set_active(!system.is_active()),
            Some("toggle-timer") => timer.set_active(!timer.is_active()),
            Some("next-color-mode") => mode.clicked(),
            Some("toggle-lock") => lock.clicked(),
            Some("new-note") => {
                // A note created while locked would be read-only; unlock so
                // the user can type into it right away.
                if !interactive.get() {
                    lock.clicked();
                }
                new_note.clicked();
            }
            Some("toggle-history") => toggle_history(),
            Some("quit") => quit.clicked(),
            _ => {}
        })
    };

    window.connect_key_press_event({
        let toggle_action = toggle_action.clone();
        let interactive = interactive.clone();
        let searching = searching.clone();
        let history_card = history.card.clone();
        let close_history_search = close_history_search.clone();
        move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                // The overlay sees key events before the focused widget, so
                // Escape while searching must close the search box rather than
                // lock the whole overlay out from under the user.
                if searching.get() && history_card.is_visible() {
                    close_history_search();
                    return glib::Propagation::Stop;
                }
                if interactive.get() {
                    toggle_action();
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        }
    });

    window.connect_delete_event({
        let state = state.clone();
        move |_, _| {
            let _ = state.borrow().save();
            glib::Propagation::Proceed
        }
    });

    // Connected before the first map so it also runs for it: a remap hands
    // the overlay a fresh, unshaped X window, and the cached shape would
    // otherwise decide there was nothing to push.
    window.connect_map({
        let registry = registry.clone();
        let interactive = interactive.clone();
        move |window| {
            invalidate_input_shape_cache();
            refresh_input_shape(window, &registry, interactive.get());
        }
    });

    window.show_all();
    set_edit_chrome_visibility(&registry, true);
    // The gear and its controls live in GNOME's real panel. This invisible
    // widget remains only as the native history popover anchor.
    widget_picker.plus.hide();
    widget_picker.card.hide();
    widget_picker.revealer.set_reveal_child(false);
    // show_all() above revealed both slot occupants; the window starts on the
    // plain title bar, and stays hidden until the panel or a saved session
    // opens it.
    history.bar.set_search_mode(false);
    if !state.borrow().settings.history_open {
        history.card.hide();
    }
    if !system_enabled {
        system_card.card.hide();
    }
    if !timer_enabled {
        timer_card.card.hide();
    }
    window.present();
    window.move_(0, 0);

    glib::idle_add_local_once({
        let window = window.clone();
        let registry = registry.clone();
        let state = state.clone();
        let root = root.clone();
        let screens = screens.clone();
        let interactive = interactive.clone();
        move || {
            clamp_registered_widgets(&root, &registry, &screens, &state);
            refresh_input_shape(&window, &registry, interactive.get());
        }
    });

    // The input shape decides whether a click lands on Sysi or falls through
    // to the app underneath, and every widget rectangle in it comes from a
    // GTK allocation. Any re-layout — a rebuilt note that is not allocated
    // yet when its own refresh runs, a card resized by the details menu, a
    // timer that changed style — moved those rectangles without updating the
    // shape, so the widget rendered but swallowed no clicks until some
    // unrelated action refreshed it. Re-derive it from the layout itself:
    // GtkFixed re-allocates on every move and size change, and the refresh is
    // a no-op unless the rectangles actually moved.
    root.connect_size_allocate({
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        let queued = Rc::new(Cell::new(false));
        move |_, _| {
            // Coalesce a burst of allocations into one refresh, and never
            // reshape from inside size-allocate itself.
            if queued.replace(true) {
                return;
            }
            glib::idle_add_local_once({
                let window = window.clone();
                let registry = registry.clone();
                let interactive = interactive.clone();
                let queued = queued.clone();
                move || {
                    queued.set(false);
                    refresh_input_shape(&window, &registry, interactive.get());
                }
            });
        }
    });

    let (hotkey_tx, hotkey_rx) = async_channel::unbounded();
    platform::spawn_global_hotkey(hotkey_tx);
    glib::MainContext::default().spawn_local({
        let toggle_action = toggle_action.clone();
        async move {
            while let Ok(action) = hotkey_rx.recv().await {
                match action {
                    platform::HotkeyAction::ToggleInteraction => toggle_action(),
                }
            }
        }
    });

    glib::source::unix_signal_add_local(libc::SIGUSR1, {
        let toggle_action = toggle_action.clone();
        move || {
            toggle_action();
            glib::ControlFlow::Continue
        }
    });
    // The panel extension writes the one-shot action and then sends SIGWINCH.
    // GLib dispatches it safely on GTK's main thread without a polling loop.
    glib::source::unix_signal_add_local(libc::SIGWINCH, {
        let dispatch_panel_action = dispatch_panel_action.clone();
        move || {
            dispatch_panel_action();
            glib::ControlFlow::Continue
        }
    });

    // A panel action left pending by the launcher (the previous instance
    // was down and the click restarted us) is applied once the UI is up.
    dispatch_panel_action();

    // Re-clamp widgets and re-cover the screen when monitors are added,
    // removed, or rescaled.
    screen.connect_monitors_changed({
        let window = window.clone();
        let root = root.clone();
        let registry = registry.clone();
        let state = state.clone();
        move |screen| {
            let root_window = screen.root_window().expect("display root window");
            let scale = root_window.scale_factor().max(1);
            let width = root_window.width() / scale;
            let height = root_window.height() / scale;
            let screens = logical_screen_rects(scale, width, height);
            window.set_default_size(width, height);
            window.move_(0, 0);
            clamp_registered_widgets(&root, &registry, &screens, &state);
        }
    });

    // The signals blocked at startup are safe to deliver now: both handlers
    // are registered above, so a pending toggle dispatches instead of
    // terminating the process (SIGUSR1's default action is death).
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGUSR1);
        libc::sigaddset(&mut set, libc::SIGWINCH);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut());
    }

    track_widget_hover(registry.clone(), history.scroller.clone());
    start_system_updates(system_card, state.clone());
    start_timer_updates(timer_card, state, window, registry, interactive);
}

fn build_system_card(initial_color_mode: ColorMode, initial_details: SystemDetails) -> SystemCard {
    let (card, body, _, color_mode, resize) = card_shell("", "", initial_color_mode);
    let values = Rc::new(RefCell::new(SystemSnapshot::default()));
    let details = Rc::new(Cell::new(initial_details));
    let drag = gtk::EventBox::new();
    drag.set_visible_window(false);
    drag.set_above_child(true);
    drag.set_hexpand(true);
    drag.set_vexpand(true);
    let canvas = gtk::DrawingArea::new();
    canvas.set_size_request(1, 1);
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    drag.add(&canvas);
    body.pack_start(&drag, true, true, 0);
    canvas.connect_draw({
        let values = values.clone();
        let color_mode = color_mode.clone();
        let details = details.clone();
        move |area, ctx| {
            draw_system(area, ctx, &values.borrow(), color_mode.get(), details.get());
            glib::Propagation::Proceed
        }
    });
    SystemCard {
        card,
        drag,
        color_mode,
        canvas,
        values,
        details,
        resize,
    }
}

fn start_system_updates(system: SystemCard, state: Rc<RefCell<AppState>>) {
    let reader = Rc::new(RefCell::new(SystemReader::default()));
    let update: Rc<dyn Fn()> = Rc::new({
        let reader = reader.clone();
        let canvas = system.canvas.clone();
        let values = system.values.clone();
        let details = system.details.clone();
        let card = system.card.clone();
        let state = state.clone();
        move || {
            let options = details.get();
            let snapshot = reader.borrow_mut().read(options.processes, options.cores);
            let desired = system_content_size(options, snapshot.cores.len());
            let current = card.allocation();
            // Only auto-grow the default layout (new core rows); a size the
            // user set with the resize handle must not be overridden.
            let custom = state.borrow().sizes.get("system").copied();
            if custom.is_none()
                && current.width() == desired.width
                && current.height() < desired.height
            {
                card.set_size_request(desired.width, desired.height);
                card.queue_resize();
            }
            *values.borrow_mut() = snapshot;
            canvas.queue_draw();
        }
    });
    update();
    glib::timeout_add_local(Duration::from_secs(2), move || {
        update();
        glib::ControlFlow::Continue
    });
}

fn draw_system(
    area: &gtk::DrawingArea,
    ctx: &Context,
    values: &SystemSnapshot,
    color_mode: ColorMode,
    details: SystemDetails,
) {
    let allocation = area.allocation();
    let width = f64::from(allocation.width().max(1));
    let meter_count = system_meter_count(details);
    let meter_width = if meter_count == 1 {
        f64::from(SYSTEM_SINGLE_WIDTH)
    } else {
        188.0
    };
    let scale = (width / meter_width).clamp(0.1, 1.0);
    let content_width = meter_width * scale;
    let (ink, muted, accent) = match color_mode {
        ColorMode::Light => ((0.97, 0.97, 0.97), (0.72, 0.72, 0.72), (0.9, 0.9, 0.9)),
        ColorMode::Gray => ((0.7, 0.7, 0.7), (0.5, 0.5, 0.5), (0.64, 0.64, 0.64)),
        ColorMode::Dark => ((0.08, 0.08, 0.08), (0.24, 0.24, 0.24), (0.14, 0.14, 0.14)),
    };
    let meters = match (details.cpu, details.ram) {
        (true, true) => [
            Some((47.0, values.cpu_percent, "CPU")),
            Some((141.0, values.memory_percent, "RAM")),
        ],
        (true, false) => [Some((38.0, values.cpu_percent, "CPU")), None],
        (false, true) => [Some((38.0, values.memory_percent, "RAM")), None],
        (false, false) => [None, None],
    };
    if meters[0].is_some() {
        let _ = ctx.save();
        ctx.translate((width - content_width) / 2.0, 0.0);
        ctx.scale(scale, scale);
        for (x, value, title) in meters.into_iter().flatten() {
            ctx.set_line_width(6.5);
            ctx.set_line_cap(cairo::LineCap::Round);
            ctx.set_source_rgba(muted.0, muted.1, muted.2, 0.22);
            ctx.new_sub_path();
            ctx.arc(x, 35.0, 28.0, -PI * 0.75, PI * 0.75);
            let _ = ctx.stroke();
            ctx.set_source_rgba(accent.0, accent.1, accent.2, 0.96);
            ctx.new_sub_path();
            ctx.arc(
                x,
                35.0,
                28.0,
                -PI * 0.75,
                -PI * 0.75 + PI * 1.5 * (value / 100.0).clamp(0.0, 1.0),
            );
            let _ = ctx.stroke();
            center_text(
                ctx,
                x,
                37.0,
                &format!("{value:.0}%"),
                18.0,
                FontWeight::Bold,
                ink,
            );
            center_text(ctx, x, 56.0, title, 8.5, FontWeight::Bold, muted);
        }
        let _ = ctx.restore();
    }
    let mut cursor_y = if details.cpu || details.ram {
        76.0 * scale
    } else {
        2.0
    };
    if details.processes {
        draw_system_processes(ctx, values, width, cursor_y, ink, muted);
        cursor_y += 108.0;
    }
    if details.cores {
        draw_system_cores(ctx, &values.cores, width, cursor_y, ink, muted, accent);
    }
}

fn system_content_size(details: SystemDetails, core_count: usize) -> Size {
    let mut height = if details.cpu || details.ram {
        SYSTEM_HEIGHT
    } else {
        10
    };
    if details.processes {
        height += 108;
    }
    if details.cores {
        height += 16 + (core_count.max(1).div_ceil(4) as i32 * 17);
    }
    Size {
        width: if details.processes || details.cores {
            318
        } else if system_meter_count(details) == 1 {
            SYSTEM_SINGLE_WIDTH
        } else {
            SYSTEM_WIDTH
        },
        height,
    }
}

fn system_meter_count(details: SystemDetails) -> usize {
    usize::from(details.cpu) + usize::from(details.ram)
}

fn draw_system_processes(
    ctx: &Context,
    values: &SystemSnapshot,
    width: f64,
    top: f64,
    ink: (f64, f64, f64),
    muted: (f64, f64, f64),
) {
    draw_left_text(
        ctx,
        5.0,
        top + 11.0,
        "PROCESS",
        8.0,
        FontWeight::Bold,
        muted,
    );
    draw_right_text(
        ctx,
        width - 96.0,
        top + 11.0,
        "CPU",
        8.0,
        FontWeight::Bold,
        muted,
    );
    draw_right_text(
        ctx,
        width - 57.0,
        top + 11.0,
        "ID",
        8.0,
        FontWeight::Bold,
        muted,
    );
    draw_right_text(
        ctx,
        width - 5.0,
        top + 11.0,
        "MEM",
        8.0,
        FontWeight::Bold,
        muted,
    );
    ctx.set_source_rgba(muted.0, muted.1, muted.2, 0.18);
    ctx.set_line_width(1.0);
    ctx.move_to(4.0, top + 15.0);
    ctx.line_to(width - 4.0, top + 15.0);
    let _ = ctx.stroke();
    for (row, process) in values.processes.iter().take(5).enumerate() {
        let baseline = top + 29.0 + row as f64 * 17.0;
        let label = truncate_text(&process.name, 23);
        draw_left_text(ctx, 5.0, baseline, &label, 9.5, FontWeight::Normal, ink);
        draw_right_text(
            ctx,
            width - 96.0,
            baseline,
            &format!("{:.1}%", process.cpu_percent),
            9.5,
            FontWeight::Normal,
            ink,
        );
        draw_right_text(
            ctx,
            width - 57.0,
            baseline,
            &process.pid.to_string(),
            9.5,
            FontWeight::Normal,
            ink,
        );
        draw_right_text(
            ctx,
            width - 5.0,
            baseline,
            &format_memory(process.memory_kib),
            9.5,
            FontWeight::Normal,
            ink,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_system_cores(
    ctx: &Context,
    cores: &[f64],
    width: f64,
    top: f64,
    ink: (f64, f64, f64),
    muted: (f64, f64, f64),
    accent: (f64, f64, f64),
) {
    let column_width = (width - 10.0) / 4.0;
    for (index, value) in cores.iter().enumerate() {
        let column = index % 4;
        let row = index / 4;
        let x = 5.0 + column as f64 * column_width;
        let y = top + 11.0 + row as f64 * 17.0;
        draw_left_text(
            ctx,
            x,
            y,
            &format!("C{:02}", index + 1),
            8.0,
            FontWeight::Bold,
            muted,
        );
        ctx.set_line_width(2.3);
        ctx.set_line_cap(cairo::LineCap::Round);
        ctx.set_source_rgba(muted.0, muted.1, muted.2, 0.22);
        ctx.move_to(x + 23.0, y - 3.0);
        ctx.line_to(x + 48.0, y - 3.0);
        let _ = ctx.stroke();
        ctx.set_source_rgb(accent.0, accent.1, accent.2);
        ctx.move_to(x + 23.0, y - 3.0);
        ctx.line_to(x + 23.0 + 25.0 * (value / 100.0).clamp(0.0, 1.0), y - 3.0);
        let _ = ctx.stroke();
        draw_right_text(
            ctx,
            x + column_width - 2.0,
            y,
            &format!("{value:.0}%"),
            8.0,
            FontWeight::Bold,
            ink,
        );
    }
}

fn draw_left_text(
    ctx: &Context,
    x: f64,
    baseline: f64,
    text: &str,
    size: f64,
    weight: FontWeight,
    color: (f64, f64, f64),
) {
    ctx.select_font_face("Noto Sans", FontSlant::Normal, weight);
    ctx.set_font_size(size);
    ctx.set_source_rgb(color.0, color.1, color.2);
    ctx.move_to(x, baseline);
    let _ = ctx.show_text(text);
}

fn draw_right_text(
    ctx: &Context,
    right: f64,
    baseline: f64,
    text: &str,
    size: f64,
    weight: FontWeight,
    color: (f64, f64, f64),
) {
    ctx.select_font_face("Noto Sans", FontSlant::Normal, weight);
    ctx.set_font_size(size);
    let width = ctx
        .text_extents(text)
        .map(|metrics| metrics.x_advance())
        .unwrap_or(0.0);
    draw_left_text(ctx, right - width, baseline, text, size, weight, color);
}

fn truncate_text(text: &str, max_chars: usize) -> String {
    let mut result: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        result.push('…');
    }
    result
}

fn format_memory(kib: u64) -> String {
    if kib >= 1024 * 1024 {
        format!("{:.1}G", kib as f64 / 1_048_576.0)
    } else {
        format!("{:.0}M", kib as f64 / 1024.0)
    }
}

struct TimerCard {
    card: gtk::EventBox,
    drag: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    style: Rc<Cell<TimerStyle>>,
    style_size: Rc<Cell<Size>>,
    typography: gtk::CssProvider,
    canvas: gtk::DrawingArea,
    stack: gtk::Stack,
    label: gtk::Label,
    action: gtk::Label,
    runtime: Rc<RefCell<TimerRuntime>>,
    alarm: Rc<Cell<bool>>,
    hovered: Rc<Cell<bool>>,
    open_edit: Rc<dyn Fn()>,
    commit_edit: Rc<dyn Fn()>,
    resize: ResizeHandle,
    wake_updates: CallbackSlot,
}

#[derive(Clone)]
struct TimerStylePreview {
    style: Rc<Cell<TimerStyle>>,
    size: Rc<Cell<Size>>,
    card: gtk::EventBox,
    canvas: gtk::DrawingArea,
    window: gtk::ApplicationWindow,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
    typography: gtk::CssProvider,
    open_edit: Rc<dyn Fn()>,
}

fn build_timer_card(
    state: Rc<RefCell<AppState>>,
    _interactive: Rc<Cell<bool>>,
    initial_color_mode: ColorMode,
) -> TimerCard {
    let (card, body, drag, color_mode, resize) = card_shell("", "", initial_color_mode);
    card.style_context().add_class("timer-card");
    let style = Rc::new(Cell::new(state.borrow().timer_style));
    let style_size = Rc::new(Cell::new(style.get().default_size()));
    card.style_context().add_class(style.get().css_class());
    let duration = state.borrow().timer_seconds.clamp(1, 24 * 3600);
    let runtime = Rc::new(RefCell::new(TimerRuntime {
        duration_seconds: duration,
        remaining: Duration::from_secs(duration as u64),
        target: None,
        started: false,
        alarm: false,
        phase: 0.0,
    }));
    let alarm = Rc::new(Cell::new(false));
    let hovered = Rc::new(Cell::new(false));
    let wake_updates: CallbackSlot = Rc::new(RefCell::new(None));

    let overlay = gtk::Overlay::new();
    overlay.set_halign(gtk::Align::Fill);
    overlay.set_valign(gtk::Align::Fill);
    overlay.set_hexpand(true);
    overlay.set_vexpand(true);
    let canvas = gtk::DrawingArea::new();
    canvas.set_size_request(1, 1);
    canvas.set_hexpand(true);
    canvas.set_vexpand(true);
    overlay.add(&canvas);

    let interaction = gtk::EventBox::new();
    interaction.set_visible_window(false);
    interaction.set_above_child(true);
    interaction.set_halign(gtk::Align::Fill);
    interaction.set_valign(gtk::Align::Fill);
    interaction.add_events(
        gdk::EventMask::ENTER_NOTIFY_MASK
            | gdk::EventMask::LEAVE_NOTIFY_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
            | gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK,
    );
    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::None);
    stack.set_hhomogeneous(false);
    stack.set_vhomogeneous(false);
    stack.set_halign(gtk::Align::Center);
    stack.set_valign(gtk::Align::Center);

    let label = gtk::Label::new(Some(&format_duration(duration)));
    label.set_selectable(false);
    label.style_context().add_class("timer-value");
    let action = gtk::Label::new(Some("START"));
    action.set_selectable(false);
    action.style_context().add_class("timer-action");
    action.set_halign(gtk::Align::Center);
    action.set_valign(gtk::Align::Center);
    action.set_opacity(1.0);
    action.hide();
    let editor = gtk::Entry::new();
    editor.set_width_chars(5);
    editor.set_can_focus(true);
    editor.set_max_length(8);
    editor.set_alignment(0.5);
    editor.style_context().add_class("timer-editor");
    stack.add_named(&label, "time");
    stack.add_named(&editor, "editor");
    stack.set_visible_child_name("time");
    let text_overlay = gtk::Overlay::new();
    text_overlay.set_halign(gtk::Align::Center);
    text_overlay.set_valign(gtk::Align::Center);
    text_overlay.add(&stack);
    text_overlay.add_overlay(&action);
    text_overlay.set_overlay_pass_through(&action, true);
    interaction.add(&text_overlay);
    overlay.add_overlay(&interaction);
    body.pack_start(&overlay, true, true, 0);

    let typography = gtk::CssProvider::new();
    for widget in [
        &label.clone().upcast::<gtk::Widget>(),
        &action.clone().upcast(),
        &editor.clone().upcast(),
    ] {
        widget
            .style_context()
            .add_provider(&typography, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
    }
    apply_timer_typography(&typography, style.get(), style_size.get());
    card.connect_size_allocate({
        let style_size = style_size.clone();
        let style = style.clone();
        let typography = typography.clone();
        move |_, allocation| {
            let size = Size {
                width: allocation.width().max(1),
                height: allocation.height().max(1),
            };
            style_size.set(size);
            apply_timer_typography(&typography, style.get(), size);
        }
    });

    canvas.connect_draw({
        let runtime = runtime.clone();
        let color_mode = color_mode.clone();
        let style = style.clone();
        move |area, ctx| {
            draw_timer_style(area, ctx, &runtime.borrow(), color_mode.get(), style.get());
            glib::Propagation::Proceed
        }
    });

    let editing = Rc::new(Cell::new(false));
    let click_start = Rc::new(Cell::new(None::<(f64, f64)>));
    let commit_edit: Rc<dyn Fn()> = Rc::new({
        let runtime = runtime.clone();
        let state = state.clone();
        let alarm = alarm.clone();
        let label = label.clone();
        let editor = editor.clone();
        let stack = stack.clone();
        let canvas = canvas.clone();
        let editing = editing.clone();
        let interaction = interaction.clone();
        let card = card.clone();
        let action = action.clone();
        let hovered = hovered.clone();
        move || {
            if !editing.replace(false) {
                return;
            }
            if let Some(seconds) = parse_timer_input(&editor.text()) {
                let seconds = seconds.clamp(1, 24 * 3600);
                let mut timer = runtime.borrow_mut();
                timer.duration_seconds = seconds;
                timer.remaining = Duration::from_secs(seconds as u64);
                timer.target = None;
                timer.started = false;
                timer.alarm = false;
                timer.phase = 0.0;
                alarm.set(false);
                label.set_text(&format_duration(seconds));
                state.borrow_mut().timer_seconds = seconds;
                let _ = state.borrow().save();
            }
            card.style_context().remove_class("alarm");
            label.style_context().remove_class("timer-alarm-value");
            interaction.set_above_child(true);
            stack.set_visible_child_name("time");
            if hovered.get() {
                action.set_text(timer_action_text(&runtime.borrow()));
                label.set_opacity(0.28);
                action.show();
            } else {
                label.set_opacity(1.0);
                action.hide();
            }
            canvas.queue_draw();
        }
    });
    let cancel_edit: Rc<dyn Fn()> = Rc::new({
        let runtime = runtime.clone();
        let label = label.clone();
        let stack = stack.clone();
        let canvas = canvas.clone();
        let editing = editing.clone();
        let interaction = interaction.clone();
        let action = action.clone();
        let hovered = hovered.clone();
        move || {
            if !editing.replace(false) {
                return;
            }
            label.set_text(&format_duration_ceil(runtime.borrow().remaining));
            interaction.set_above_child(true);
            stack.set_visible_child_name("time");
            if hovered.get() {
                label.set_opacity(0.28);
                action.set_text(timer_action_text(&runtime.borrow()));
                action.show();
            } else {
                label.set_opacity(1.0);
                action.hide();
            }
            canvas.queue_draw();
        }
    });
    editor.connect_activate({
        let commit_edit = commit_edit.clone();
        move |_| commit_edit()
    });
    editor.connect_changed({
        let commit_edit = commit_edit.clone();
        let auto_formatting = Rc::new(Cell::new(false));
        move |entry| {
            if auto_formatting.get() {
                return;
            }
            let raw = entry.text();
            if raw.len() != 4 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                return;
            }
            let formatted = format!("{}:{}", &raw[..2], &raw[2..]);
            if parse_timer_input(&formatted).is_none() {
                return;
            }
            auto_formatting.set(true);
            entry.set_text(&formatted);
            entry.set_position(-1);
            auto_formatting.set(false);
            commit_edit();
        }
    });
    editor.connect_focus_out_event({
        let cancel_edit = cancel_edit.clone();
        move |_, _| {
            cancel_edit();
            glib::Propagation::Proceed
        }
    });
    editor.connect_key_press_event({
        let cancel_edit = cancel_edit.clone();
        move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                cancel_edit();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    interaction.connect_enter_notify_event({
        let hovered = hovered.clone();
        let runtime = runtime.clone();
        let label = label.clone();
        let action = action.clone();
        let editing = editing.clone();
        move |_, _| {
            hovered.set(true);
            if !editing.get() {
                action.set_text(timer_action_text(&runtime.borrow()));
                label.set_opacity(0.28);
                action.show();
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_leave_notify_event({
        let hovered = hovered.clone();
        let label = label.clone();
        let action = action.clone();
        let editing = editing.clone();
        move |_, _| {
            hovered.set(false);
            if !editing.get() {
                label.set_opacity(1.0);
                action.hide();
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_motion_notify_event({
        let hovered = hovered.clone();
        let runtime = runtime.clone();
        let label = label.clone();
        let action = action.clone();
        let editing = editing.clone();
        move |_, _| {
            hovered.set(true);
            if !editing.get() {
                action.set_text(timer_action_text(&runtime.borrow()));
                label.set_opacity(0.28);
                action.show();
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_button_press_event({
        let click_start = click_start.clone();
        move |_, event| {
            if event.button() == 1 && event.event_type() == gdk::EventType::ButtonPress {
                click_start.set(Some(event.position()));
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_button_release_event({
        let runtime = runtime.clone();
        let alarm = alarm.clone();
        let label = label.clone();
        let action = action.clone();
        let canvas = canvas.clone();
        let card = card.clone();
        let wake_updates = wake_updates.clone();
        let click_start = click_start.clone();
        move |_, event| {
            if event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            let Some((start_x, start_y)) = click_start.replace(None) else {
                return glib::Propagation::Proceed;
            };
            let (end_x, end_y) = event.position();
            if (end_x - start_x).abs() + (end_y - start_y).abs() > 5.0 {
                return glib::Propagation::Proceed;
            }
            let mut timer = runtime.borrow_mut();
            if timer.alarm {
                timer.alarm = false;
                timer.target = None;
                timer.started = false;
                timer.remaining = Duration::from_secs(timer.duration_seconds as u64);
                alarm.set(false);
                card.style_context().remove_class("alarm");
                label.style_context().remove_class("timer-alarm-value");
            } else if let Some(target) = timer.target.take() {
                timer.remaining = target.saturating_duration_since(Instant::now());
            } else {
                if timer.remaining.is_zero() {
                    timer.remaining = Duration::from_secs(timer.duration_seconds as u64);
                }
                timer.started = true;
                timer.target = Some(Instant::now() + timer.remaining);
            }
            label.set_text(&format_duration_ceil(timer.remaining));
            action.set_text(timer_action_text(&timer));
            drop(timer);
            canvas.queue_draw();
            if let Some(wake) = wake_updates.borrow().as_ref() {
                wake();
            }
            glib::Propagation::Stop
        }
    });

    let open_edit: Rc<dyn Fn()> = Rc::new({
        let runtime = runtime.clone();
        let label = label.clone();
        let action = action.clone();
        let editor = editor.clone();
        let stack = stack.clone();
        let editing = editing.clone();
        let interaction = interaction.clone();
        let canvas = canvas.clone();
        move || {
            if editing.replace(true) {
                return;
            }
            let mut timer = runtime.borrow_mut();
            if let Some(target) = timer.target.take() {
                timer.remaining = target.saturating_duration_since(Instant::now());
                timer.started = true;
            }
            label.set_text(&format_duration_ceil(timer.remaining));
            action.set_text(timer_action_text(&timer));
            drop(timer);
            editor.set_text(&format_duration(runtime.borrow().duration_seconds));
            label.set_opacity(1.0);
            action.hide();
            stack.set_visible_child_name("editor");
            interaction.set_above_child(false);
            editor.grab_focus();
            editor.select_region(0, -1);
            canvas.queue_draw();
        }
    });

    TimerCard {
        card,
        drag,
        color_mode,
        style,
        style_size,
        typography,
        canvas,
        stack,
        label,
        action,
        runtime,
        alarm,
        hovered,
        open_edit,
        commit_edit,
        resize,
        wake_updates,
    }
}

fn timer_action_text(timer: &TimerRuntime) -> &'static str {
    if timer.alarm {
        "DISMISS"
    } else if timer.target.is_some() {
        "PAUSE"
    } else if timer.started {
        "RESUME"
    } else {
        "START"
    }
}

fn parse_timer_input(value: &str) -> Option<i64> {
    let parts: Vec<&str> = value.trim().split(':').collect();
    let seconds = match parts.as_slice() {
        [minutes] => minutes.parse::<i64>().ok()?.checked_mul(60)?,
        [minutes, seconds] => {
            let minutes = minutes.parse::<i64>().ok()?;
            let seconds = seconds.parse::<i64>().ok()?;
            if !(0..60).contains(&seconds) {
                return None;
            }
            minutes.checked_mul(60)?.checked_add(seconds)?
        }
        [hours, minutes, seconds] => {
            let hours = hours.parse::<i64>().ok()?;
            let minutes = minutes.parse::<i64>().ok()?;
            let seconds = seconds.parse::<i64>().ok()?;
            if !(0..60).contains(&minutes) || !(0..60).contains(&seconds) {
                return None;
            }
            hours
                .checked_mul(3600)?
                .checked_add(minutes.checked_mul(60)?)?
                .checked_add(seconds)?
        }
        _ => return None,
    };
    (seconds > 0).then_some(seconds)
}

fn draw_timer_style(
    area: &gtk::DrawingArea,
    ctx: &Context,
    timer: &TimerRuntime,
    color_mode: ColorMode,
    style: TimerStyle,
) {
    match style {
        TimerStyle::Ring => draw_timer_ring(area, ctx, timer, color_mode),
        TimerStyle::Digital => draw_timer_digital_alarm(area, ctx, timer, color_mode),
        TimerStyle::Ticks => draw_timer_ticks(area, ctx, timer, color_mode),
        TimerStyle::Arc => draw_timer_arc(area, ctx, timer, color_mode),
    }
}

fn timer_gray(color_mode: ColorMode) -> f64 {
    match color_mode {
        ColorMode::Light => 0.91,
        ColorMode::Gray => 0.6,
        ColorMode::Dark => 0.12,
    }
}

fn timer_ratio(timer: &TimerRuntime) -> f64 {
    (timer.remaining.as_secs_f64() / timer.duration_seconds.max(1) as f64).clamp(0.0, 1.0)
}

fn timer_center(area: &gtk::DrawingArea, inset: f64) -> (f64, f64, f64) {
    let allocation = area.allocation();
    let cx = f64::from(allocation.width()) / 2.0;
    let cy = f64::from(allocation.height()) / 2.0;
    (cx, cy, (cx.min(cy) - inset).max(1.0))
}

fn draw_timer_ring(
    area: &gtk::DrawingArea,
    ctx: &Context,
    timer: &TimerRuntime,
    color_mode: ColorMode,
) {
    let (cx, cy, radius) = timer_center(area, 9.0);
    let gray = timer_gray(color_mode);
    let ratio = timer_ratio(timer);
    let start = -PI / 2.0;

    ctx.set_line_width(8.0);
    ctx.set_line_cap(cairo::LineCap::Round);
    ctx.set_source_rgba(gray, gray, gray, 0.16);
    ctx.new_sub_path();
    ctx.arc(cx, cy, radius, 0.0, TAU);
    let _ = ctx.stroke();

    if timer.alarm {
        let pulse = 0.42 + 0.48 * (timer.phase.sin() * 0.5 + 0.5);
        ctx.set_source_rgba(gray, gray, gray, pulse);
        ctx.new_sub_path();
        ctx.arc(cx, cy, radius, 0.0, TAU);
        let _ = ctx.stroke();
    } else if ratio > 0.0 {
        ctx.set_source_rgba(gray, gray, gray, 0.92);
        ctx.new_sub_path();
        ctx.arc(cx, cy, radius, start, start + TAU * ratio);
        let _ = ctx.stroke();
    }
}

fn draw_timer_digital_alarm(
    area: &gtk::DrawingArea,
    ctx: &Context,
    timer: &TimerRuntime,
    color_mode: ColorMode,
) {
    if !timer.alarm {
        return;
    }
    let (cx, cy, radius) = timer_center(area, 11.0);
    let pulse = 0.25 + 0.6 * (timer.phase.sin() * 0.5 + 0.5);
    let gray = timer_gray(color_mode);
    ctx.set_source_rgba(gray, gray, gray, pulse);
    ctx.set_line_width(2.0);
    ctx.set_line_cap(cairo::LineCap::Round);
    for direction in [-1.0, 1.0] {
        let x = cx + direction * radius * 0.82;
        ctx.move_to(x, cy - radius * 0.18);
        ctx.line_to(x + direction * 5.0, cy);
        ctx.line_to(x, cy + radius * 0.18);
    }
    let _ = ctx.stroke();
}

fn draw_timer_ticks(
    area: &gtk::DrawingArea,
    ctx: &Context,
    timer: &TimerRuntime,
    color_mode: ColorMode,
) {
    let (cx, cy, radius) = timer_center(area, 8.0);
    let gray = timer_gray(color_mode);
    let ratio = timer_ratio(timer);
    let active_ticks = (ratio * 24.0).ceil() as usize;
    let pulse = 0.38 + 0.52 * (timer.phase.sin() * 0.5 + 0.5);
    ctx.set_line_width(2.3);
    ctx.set_line_cap(cairo::LineCap::Round);
    for index in 0..24 {
        let angle = -PI / 2.0 + TAU * index as f64 / 24.0;
        let (sin, cos) = angle.sin_cos();
        let inner = radius - if index % 6 == 0 { 7.0 } else { 4.5 };
        let alpha = if timer.alarm {
            pulse
        } else if index < active_ticks {
            0.9
        } else {
            0.2
        };
        ctx.set_source_rgba(gray, gray, gray, alpha);
        ctx.move_to(cx + cos * inner, cy + sin * inner);
        ctx.line_to(cx + cos * radius, cy + sin * radius);
        let _ = ctx.stroke();
    }
}

fn draw_timer_arc(
    area: &gtk::DrawingArea,
    ctx: &Context,
    timer: &TimerRuntime,
    color_mode: ColorMode,
) {
    let (cx, cy, radius) = timer_center(area, 8.0);
    let gray = timer_gray(color_mode);
    let ratio = timer_ratio(timer);
    let start = -PI * 0.84;
    let sweep = TAU * 0.74;
    ctx.set_line_width(3.2);
    ctx.set_line_cap(cairo::LineCap::Round);
    ctx.set_source_rgba(gray, gray, gray, 0.2);
    ctx.new_sub_path();
    ctx.arc(cx, cy, radius, start, start + sweep);
    let _ = ctx.stroke();

    let alpha = if timer.alarm {
        0.38 + 0.52 * (timer.phase.sin() * 0.5 + 0.5)
    } else {
        0.92
    };
    ctx.set_source_rgba(gray, gray, gray, alpha);
    ctx.new_sub_path();
    ctx.arc(
        cx,
        cy,
        radius,
        start,
        start + if timer.alarm { sweep } else { sweep * ratio },
    );
    let _ = ctx.stroke();
}

fn start_timer_updates(
    timer_ui: TimerCard,
    _state: Rc<RefCell<AppState>>,
    window: gtk::ApplicationWindow,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
) {
    let source: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let alarm_was_active = Rc::new(Cell::new(false));
    let wake_updates: Rc<dyn Fn()> = Rc::new({
        let source = source.clone();
        let runtime = timer_ui.runtime.clone();
        let label = timer_ui.label.clone();
        let action = timer_ui.action.clone();
        let stack = timer_ui.stack.clone();
        let canvas = timer_ui.canvas.clone();
        let card = timer_ui.card.clone();
        let alarm = timer_ui.alarm.clone();
        let hovered = timer_ui.hovered.clone();
        let alarm_was_active = alarm_was_active.clone();
        move || {
            if source.borrow().is_some() {
                return;
            }
            let active = {
                let timer = runtime.borrow();
                timer.target.is_some() || timer.alarm
            };
            if !active {
                return;
            }

            let source_for_tick = source.clone();
            let runtime = runtime.clone();
            let label = label.clone();
            let action = action.clone();
            let stack = stack.clone();
            let canvas = canvas.clone();
            let card = card.clone();
            let alarm = alarm.clone();
            let hovered = hovered.clone();
            let alarm_was_active = alarm_was_active.clone();
            let window = window.clone();
            let registry = registry.clone();
            let interactive = interactive.clone();
            let id = glib::timeout_add_local(Duration::from_millis(100), move || {
                let mut timer = runtime.borrow_mut();
                timer.phase += 0.2;
                if let Some(target) = timer.target {
                    timer.remaining = target.saturating_duration_since(Instant::now());
                    label.set_text(&format_duration_ceil(timer.remaining));
                    if timer.remaining.is_zero() {
                        timer.target = None;
                        timer.alarm = true;
                        alarm.set(true);
                        label.set_text("TIME'S UP!");
                        label.style_context().add_class("timer-alarm-value");
                        card.style_context().add_class("alarm");
                    }
                }
                let keep_running = timer.target.is_some() || timer.alarm;
                if keep_running {
                    canvas.queue_draw();
                }
                if hovered.get() && stack.visible_child_name().as_deref() != Some("editor") {
                    action.set_text(timer_action_text(&timer));
                    action.show();
                }
                let alarm_active = timer.alarm;
                drop(timer);

                if alarm_was_active.replace(alarm_active) != alarm_active {
                    refresh_input_shape(&window, &registry, interactive.get());
                }
                if keep_running {
                    glib::ControlFlow::Continue
                } else {
                    source_for_tick.borrow_mut().take();
                    glib::ControlFlow::Break
                }
            });
            *source.borrow_mut() = Some(id);
        }
    });
    *timer_ui.wake_updates.borrow_mut() = Some(wake_updates);
}

fn rebuild_note_list(
    list: &gtk::Box,
    root: &gtk::Fixed,
    state: Rc<RefCell<AppState>>,
    refresh: CallbackSlot,
    query: &str,
    limit: usize,
) {
    for child in list.children() {
        list.remove(&child);
    }
    let query = query.trim().to_lowercase();
    // Borrow the notes instead of cloning every one; only matches become
    // widgets, and only as many as the window is tall enough to scroll
    // through, so a huge history stays cheap per keystroke.
    let notes = state.borrow();
    let mut shown = 0_usize;
    for note in notes
        .notes
        .iter()
        .rev()
        .filter(|note| query.is_empty() || note.text.to_lowercase().contains(&query))
        .take(limit)
    {
        // First non-empty line, so a note starting with a blank line still
        // shows its content instead of "Untitled note". The headline is not
        // truncated here: the row label ellipsizes, so widening the window
        // reveals more of it and narrowing it reveals less.
        let headline = note
            .text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim();
        let headline = if headline.is_empty() {
            "Untitled note"
        } else {
            headline
        };
        let row = draggable_note_preview(
            &format!(
                "{}  {headline}",
                if note.pinned { "\u{25cf}" } else { "\u{25cb}" }
            ),
            note.id,
            root,
            state.clone(),
            refresh.clone(),
        );
        list.pack_start(&row, false, false, 0);
        shown += 1;
    }
    drop(notes);
    if shown == 0 {
        let empty = gtk::Label::new(Some(if query.is_empty() {
            "No notes yet"
        } else {
            "No matches"
        }));
        empty.set_xalign(0.0);
        empty.set_ellipsize(gtk::pango::EllipsizeMode::End);
        empty.style_context().add_class("history-empty");
        list.pack_start(&empty, false, false, 0);
    }

    list.show_all();
}

fn draggable_note_preview(
    text: &str,
    note_id: u64,
    root: &gtk::Fixed,
    state: Rc<RefCell<AppState>>,
    refresh: CallbackSlot,
) -> gtk::EventBox {
    let row = gtk::EventBox::new();
    row.set_visible_window(false);
    row.style_context().add_class("note-preview");
    row.set_tooltip_text(Some("Click or drag onto the desktop to pin"));
    row.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON1_MOTION_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK,
    );
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.add(&label);

    let start = Rc::new(Cell::new(None::<(f64, f64)>));
    let ghost: Rc<RefCell<Option<gtk::Label>>> = Rc::new(RefCell::new(None));
    row.connect_button_press_event({
        let start = start.clone();
        move |_, event| {
            if event.button() == 1 {
                start.set(Some(event.root()));
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    row.connect_motion_notify_event({
        let start = start.clone();
        let ghost = ghost.clone();
        let root = root.clone();
        let text = text.to_string();
        move |_, event| {
            let Some((sx, sy)) = start.get() else {
                return glib::Propagation::Proceed;
            };
            // A lost release (broken grab) would leave the ghost stranded on
            // the overlay; drop the drag as soon as the button is no longer down.
            if !event.state().contains(gdk::ModifierType::BUTTON1_MASK) {
                start.set(None);
                if let Some(floating) = ghost.borrow_mut().take() {
                    root.remove(&floating);
                }
                return glib::Propagation::Proceed;
            }
            let (x, y) = event.root();
            if ghost.borrow().is_none() && ((x - sx).abs() > 5.0 || (y - sy).abs() > 5.0) {
                let floating = gtk::Label::new(Some(&truncate_chars(&text, 34)));
                floating.style_context().add_class("note-ghost");
                root.put(&floating, x as i32 - 72, y as i32 - 18);
                floating.show();
                *ghost.borrow_mut() = Some(floating);
            }
            if let Some(floating) = ghost.borrow().as_ref() {
                root.move_(floating, x as i32 - 72, y as i32 - 18);
            }
            glib::Propagation::Stop
        }
    });
    row.connect_button_release_event({
        let start = start.clone();
        let ghost = ghost.clone();
        let root = root.clone();
        move |_, event| {
            start.set(None);
            let floating = ghost.borrow_mut().take();
            let dragged = floating.is_some();
            let (x, y) = event.root();
            let desired = if let Some(floating) = floating {
                root.remove(&floating);
                // Pin the note exactly where the ghost preview was dropped.
                Point {
                    x: (x as i32 - 72).max(0),
                    y: (y as i32 - 18).max(0),
                }
            } else {
                Point {
                    x: (x as i32 + 18).max(0),
                    y: (y as i32 + 18).max(0),
                }
            };
            let root_allocation = root.allocation();
            let screens = logical_screen_rects(
                root.scale_factor(),
                root_allocation.width(),
                root_allocation.height(),
            );
            let point = clamp_to_screens(desired, NOTE_WIDTH, NOTE_HEIGHT, &screens);
            let mut data = state.borrow_mut();
            if let Some(note) = data.notes.iter_mut().find(|note| note.id == note_id) {
                // Only an explicit drag repositions. A plain click on a row
                // whose note is already on the desktop used to teleport that
                // note to the pointer, which read as the note jumping away.
                if dragged || !note.pinned {
                    note.position = point;
                }
                note.pinned = true;
            }
            let _ = data.save();
            drop(data);
            if let Some(callback) = refresh.borrow().as_ref() {
                callback();
            }
            glib::Propagation::Stop
        }
    });
    row
}

fn rebuild_pinned_notes(
    root: &gtk::Fixed,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    refresh: CallbackSlot,
    interactive: Rc<Cell<bool>>,
    window: gtk::ApplicationWindow,
) {
    let old: Vec<gtk::EventBox> = registry
        .borrow()
        .iter()
        .filter(|item| item.key.starts_with("note:"))
        .map(|item| item.widget.clone())
        .collect();
    for widget in old {
        root.remove(&widget);
    }
    registry
        .borrow_mut()
        .retain(|item| !item.key.starts_with("note:"));

    let pinned: Vec<Note> = state
        .borrow()
        .notes
        .iter()
        .filter(|note| note.pinned)
        .cloned()
        .collect();
    for note in pinned {
        let key = format!("note:{}", note.id);
        let initial_color_mode = saved_color_mode(&state.borrow(), &key);
        let (card, body, _drag, color_mode, resize) = card_shell("", "", initial_color_mode);
        card.style_context().add_class("pinned-note");
        // Own a GdkWindow so the hover tracker can test pointer containment and
        // fade the note scrollbar in without GTK3's per-window :hover routing.
        card.set_visible_window(true);

        let header_drag = gtk::EventBox::new();
        header_drag.set_visible_window(true);
        header_drag.set_hexpand(true);
        header_drag.style_context().add_class("note-header");
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        header.set_hexpand(true);
        let delete = small_button("×");
        delete.style_context().add_class("note-window-button");
        delete.style_context().add_class("note-close");
        delete.set_tooltip_text(Some("Delete note"));
        let unpin = small_button("−");
        unpin.style_context().add_class("note-window-button");
        unpin.style_context().add_class("note-hide");
        unpin.set_tooltip_text(Some("Move to History"));
        header.pack_start(&delete, false, false, 0);
        header.pack_start(&unpin, false, false, 0);
        header_drag.add(&header);
        body.pack_start(&header_drag, false, false, 0);

        let editor = gtk::TextView::new();
        editor.set_editable(interactive.get());
        editor.set_can_focus(true);
        editor.set_cursor_visible(true);
        editor.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
        editor.connect_button_press_event(|editor, event| {
            if event.button() == 1 {
                editor.grab_focus();
            }
            glib::Propagation::Proceed
        });
        // WordChar (not Word) so an overlong token breaks instead of forcing
        // the layout wider than the card.
        editor.set_wrap_mode(gtk::WrapMode::WordChar);
        editor.set_size_request(1, 1);
        editor.set_hexpand(true);
        editor.set_vexpand(true);
        editor.style_context().add_class("pinned-editor");
        editor.buffer().expect("note buffer").set_text(&note.text);
        let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scroller.add_events(gdk::EventMask::BUTTON_PRESS_MASK);
        scroller.connect_button_press_event({
            let editor = editor.clone();
            move |_, event| {
                if event.button() == 1 {
                    editor.grab_focus();
                }
                glib::Propagation::Proceed
            }
        });
        // Horizontal must NOT be Never: GTK3 propagates the child's minimum
        // width through a Never-policy scrolled window, and a TextView's
        // minimum width is its current text layout width — GtkFixed then
        // allocates that minimum, so the note could never shrink below its
        // text and even grew while typing. External keeps the text wrapping
        // to the card (WordChar guarantees no horizontal overflow) without
        // ever propagating the text width. Automatic (vertical) keeps the
        // note at its fixed height and reveals a scrollbar on overflow.
        scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Automatic);
        scroller.set_shadow_type(gtk::ShadowType::None);
        scroller.set_overlay_scrolling(false);
        scroller.set_size_request(1, 1);
        scroller.set_hexpand(true);
        scroller.set_vexpand(true);
        // Keep both axes bounded to the card's requested size: the note must
        // not grow with its content (horizontally or vertically) past the size
        // the user set, so long text wraps and scrolls instead.
        scroller.set_propagate_natural_width(false);
        scroller.set_propagate_natural_height(false);
        scroller.add(&editor);
        body.pack_start(&scroller, true, true, 0);

        apply_widget_size(
            &card,
            &key,
            &state,
            Size {
                width: NOTE_WIDTH,
                height: NOTE_HEIGHT,
            },
        );
        root.put(&card, note.position.x, note.position.y);
        register(&registry, &key, &card, color_mode);
        if let Some(item) = registry
            .borrow_mut()
            .iter_mut()
            .find(|item| item.key == key)
        {
            item.edit_only = Some(header_drag.clone());
            item.editor = Some(editor.clone());
        }
        attach_color_mode_menu(
            &card,
            key.clone(),
            state.clone(),
            registry.clone(),
            interactive.clone(),
            None,
            None,
        );
        attach_drag(
            &header_drag,
            &card,
            root,
            key.clone(),
            state.clone(),
            registry.clone(),
            interactive.clone(),
            window.clone(),
        );
        attach_resize(
            &resize,
            &card,
            root,
            key,
            state.clone(),
            registry.clone(),
            interactive.clone(),
            window.clone(),
            ResizeBounds {
                min_width: 75,
                min_height: 92,
                max_width: 540,
                max_height: 440,
                aspect_ratio: None,
                preserve_current_aspect: false,
            },
        );

        let pending_save: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        editor.buffer().expect("note buffer").connect_changed({
            let state = state.clone();
            let pending_save = pending_save.clone();
            let id = note.id;
            move |buffer| {
                let text = buffer
                    .text(&buffer.start_iter(), &buffer.end_iter(), false)
                    .map(|value| value.to_string())
                    .unwrap_or_default();
                if let Some(note) = state.borrow_mut().notes.iter_mut().find(|n| n.id == id) {
                    note.text = text;
                }
                if let Some(source) = pending_save.borrow_mut().take() {
                    source.remove();
                }
                let state = state.clone();
                let pending_save_for_timeout = pending_save.clone();
                let source = glib::timeout_add_local_once(Duration::from_millis(450), move || {
                    let _ = state.borrow().save();
                    pending_save_for_timeout.borrow_mut().take();
                });
                *pending_save.borrow_mut() = Some(source);
            }
        });
        unpin.connect_clicked({
            let state = state.clone();
            let refresh = refresh.clone();
            let id = note.id;
            move |_| {
                if let Some(note) = state.borrow_mut().notes.iter_mut().find(|n| n.id == id) {
                    note.pinned = false;
                }
                let _ = state.borrow().save();
                if let Some(callback) = refresh.borrow().as_ref() {
                    callback();
                }
            }
        });
        delete.connect_clicked({
            let state = state.clone();
            let refresh = refresh.clone();
            let id = note.id;
            move |_| {
                let mut data = state.borrow_mut();
                data.notes.retain(|note| note.id != id);
                data.sizes.remove(&format!("note:{id}"));
                data.widget_color_modes.remove(&format!("note:{id}"));
                drop(data);
                let _ = state.borrow().save();
                if let Some(callback) = refresh.borrow().as_ref() {
                    callback();
                }
            }
        });
        card.show_all();
        header_drag.set_visible(interactive.get());
    }
}

fn track_widget_hover(registry: Rc<RefCell<Vec<RegisteredWidget>>>, history: gtk::ScrolledWindow) {
    // GTK3 delivers enter/leave to the window under the pointer, so hovering
    // the editor (which owns its own window) never sets :hover on the note
    // card. Poll the pointer position cheaply — one timer serves both the
    // pinned notes and the history list — and toggle classes that fade the
    // scrollbar thumbs in instead of relying on CSS :hover propagation.
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(device) = gdk::Display::default()
            .and_then(|display| display.default_seat())
            .and_then(|seat| seat.pointer())
        else {
            return glib::ControlFlow::Continue;
        };
        let (_, pointer_x, pointer_y) = device.position();
        let notes: Vec<gtk::EventBox> = registry
            .borrow()
            .iter()
            .filter(|item| item.key.starts_with("note:"))
            .map(|item| item.widget.clone())
            .collect();
        for card in notes {
            let hovered = card.window().is_some_and(|window| {
                let (_, origin_x, origin_y) = window.origin();
                pointer_x >= origin_x
                    && pointer_x < origin_x + window.width()
                    && pointer_y >= origin_y
                    && pointer_y < origin_y + window.height()
            });
            let context = card.style_context();
            if hovered {
                context.add_class("note-hover");
            } else {
                context.remove_class("note-hover");
            }
        }
        let context = history.style_context();
        let hovered = history.window().is_some_and(|window| {
            let (_, origin_x, origin_y) = window.origin();
            pointer_x >= origin_x
                && pointer_x < origin_x + window.width()
                && pointer_y >= origin_y
                && pointer_y < origin_y + window.height()
        });
        if hovered {
            context.add_class("history-hover");
        } else {
            context.remove_class("history-hover");
        }
        glib::ControlFlow::Continue
    });
}

// The history list is its own note-shaped overlay window: a draggable header
// that flips between a title bar and a search field, a scrolling column of
// note headlines, and a resize grip. Widening it reveals more of each
// headline; heightening it renders and shows more rows.
struct HistoryWindow {
    card: gtk::EventBox,
    header: gtk::EventBox,
    bar: HistoryHeader,
    hide: gtk::Button,
    list: gtk::Box,
    scroller: gtk::ScrolledWindow,
    color_mode: Rc<Cell<ColorMode>>,
    resize: ResizeHandle,
}

// The two modes share one row: the hide button never moves, the title and the
// entry occupy the same slot, and the magnifier and the "clear" cross share
// the trailing slot. Only the occupant of each slot changes, so the caret
// lands exactly where the title text was.
#[derive(Clone)]
struct HistoryHeader {
    title: gtk::Label,
    search: gtk::Entry,
    open_search: gtk::Button,
    close_search: gtk::Button,
}

impl HistoryHeader {
    fn set_search_mode(&self, searching: bool) {
        self.title.set_visible(!searching);
        self.open_search.set_visible(!searching);
        self.search.set_visible(searching);
        self.close_search.set_visible(searching);
    }
}

struct WidgetPicker {
    card: gtk::EventBox,
    drag: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    plus: gtk::Button,
    revealer: gtk::Revealer,
    system: gtk::ToggleButton,
    timer: gtk::ToggleButton,
    mode: gtk::Button,
    lock: gtk::Button,
    new_note: gtk::Button,
    quit: gtk::Button,
}

fn build_widget_picker(initial_color_mode: ColorMode) -> WidgetPicker {
    let card = gtk::EventBox::new();
    card.set_visible_window(false);
    card.style_context().add_class("picker-widget");
    let color_mode = Rc::new(Cell::new(initial_color_mode));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 5);
    card.add(&content);

    let drag = gtk::EventBox::new();
    drag.set_visible_window(false);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    drag.add(&top);
    content.pack_start(&drag, false, false, 0);

    let slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    slot.style_context().add_class("empty-slot");
    let plus = gtk::Button::with_label("⚙");
    plus.set_tooltip_text(Some("Settings"));
    plus.set_can_focus(false);
    plus.style_context().add_class("slot-plus");
    slot.pack_start(&plus, false, false, 0);
    top.pack_start(&slot, false, false, 0);

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
    revealer.set_transition_duration(190);
    let choices = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    choices.style_context().add_class("widget-choices");
    let system = picker_toggle("SYSTEM");
    let timer = picker_toggle("TIMER");
    let mode = picker_button(initial_color_mode.label());
    let lock = picker_button("LOCK");
    let new_note = picker_button("＋  NOTE");
    let quit = picker_button("QUIT");
    choices.pack_start(&system, false, false, 0);
    choices.pack_start(&timer, false, false, 0);
    choices.pack_start(&mode, false, false, 0);
    choices.pack_start(&lock, false, false, 0);
    choices.pack_start(&new_note, false, false, 0);
    choices.pack_start(&quit, false, false, 0);
    revealer.add(&choices);
    top.pack_start(&revealer, false, false, 0);

    WidgetPicker {
        card,
        drag,
        color_mode,
        plus,
        revealer,
        system,
        timer,
        mode,
        lock,
        new_note,
        quit,
    }
}

fn history_row_budget(card_height: i32) -> usize {
    let list_height = (card_height - HISTORY_CHROME_HEIGHT).max(HISTORY_ROW_HEIGHT);
    let visible = (list_height / HISTORY_ROW_HEIGHT) as usize;
    // Render a screenful plus some headroom: the extra rows are what the
    // scrollbar scrolls through, and the total shrinks with the window so a
    // small history window stays cheap to rebuild per keystroke.
    (visible + 10).clamp(14, 240)
}

fn build_history_window(initial_color_mode: ColorMode) -> HistoryWindow {
    let (card, body, _drag, color_mode, resize) = card_shell("", "", initial_color_mode);
    // Share the pinned-note look (transparent card, faded scrollbar thumb) so
    // the history window reads as one of the notes.
    card.style_context().add_class("pinned-note");
    card.style_context().add_class("history-window");
    // Own a GdkWindow so the hover tracker can test pointer containment.
    card.set_visible_window(true);

    let header = gtk::EventBox::new();
    header.set_visible_window(true);
    header.set_hexpand(true);
    header.style_context().add_class("note-header");
    header.style_context().add_class("history-header");
    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    bar.set_hexpand(true);
    let hide = small_button("\u{2212}");
    hide.style_context().add_class("note-window-button");
    hide.style_context().add_class("note-hide");
    hide.set_tooltip_text(Some("Hide History"));
    let title = gtk::Label::new(Some("HISTORY"));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.style_context().add_class("history-title");
    let search = gtk::Entry::new();
    search.set_placeholder_text(Some("SEARCH NOTES\u{2026}"));
    search.set_has_frame(false);
    // GtkEntry's default width-chars is a hard minimum that GtkFixed would
    // honour, pinning the window open at ~150px; one char lets it shrink with
    // the card while still expanding to fill the header.
    search.set_width_chars(1);
    search.set_max_width_chars(1);
    search.set_hexpand(true);
    search.style_context().add_class("history-search");
    let open_search = icon_button("edit-find-symbolic", "Search notes");
    let close_search = small_button("\u{00d7}");
    close_search.style_context().add_class("note-window-button");
    close_search.style_context().add_class("note-close");
    close_search.set_tooltip_text(Some("Close search"));
    bar.pack_start(&hide, false, false, 0);
    bar.pack_start(&title, true, true, 0);
    bar.pack_start(&search, true, true, 0);
    // Packed end-first, so the cross sits at the very edge and the magnifier
    // takes the same spot when it is the visible one.
    bar.pack_end(&close_search, false, false, 0);
    bar.pack_end(&open_search, false, false, 0);
    header.add(&bar);
    body.pack_start(&header, false, false, 0);

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    // External (not Never) horizontally: a Never-policy scrolled window
    // propagates its child's minimum width, so the widest row would stop the
    // window from ever being dragged narrower again.
    scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Automatic);
    // The indicator floats over the rows, so rows use the full width and the
    // thumb fades in while the pointer is over the list.
    scroller.set_overlay_scrolling(true);
    scroller.set_shadow_type(gtk::ShadowType::None);
    scroller.set_propagate_natural_width(false);
    scroller.set_propagate_natural_height(false);
    scroller.set_size_request(1, 1);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.style_context().add_class("history-scroller");
    let list = gtk::Box::new(gtk::Orientation::Vertical, 2);
    list.style_context().add_class("history-list");
    scroller.add(&list);
    body.pack_start(&scroller, true, true, 0);

    HistoryWindow {
        card,
        header,
        bar: HistoryHeader {
            title,
            search,
            open_search,
            close_search,
        },
        hide,
        list,
        scroller,
        color_mode,
        resize,
    }
}

fn picker_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.set_can_focus(false);
    button.style_context().add_class("picker-choice");
    button
}

fn picker_toggle(label: &str) -> gtk::ToggleButton {
    let button = gtk::ToggleButton::with_label(label);
    button.set_can_focus(false);
    button.style_context().add_class("picker-choice");
    button.style_context().add_class("picker-toggle");
    button
}

fn take_panel_action() -> Option<String> {
    let path = crate::state::cache_dir().join("panel-action");
    let action = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(path);
    Some(action.trim().to_owned())
}

fn card_shell(
    title: &str,
    kicker: &str,
    initial_color_mode: ColorMode,
) -> (
    gtk::EventBox,
    gtk::Box,
    gtk::EventBox,
    Rc<Cell<ColorMode>>,
    ResizeHandle,
) {
    let event = gtk::EventBox::new();
    event.set_visible_window(false);
    let color_mode = Rc::new(Cell::new(initial_color_mode));
    let shell = gtk::Overlay::new();
    shell.set_hexpand(true);
    shell.set_vexpand(true);
    let card = gtk::Box::new(gtk::Orientation::Vertical, 5);
    card.set_hexpand(true);
    card.set_vexpand(true);
    card.style_context().add_class("card");
    shell.add(&card);

    let resize_hitbox = gtk::EventBox::new();
    resize_hitbox.set_visible_window(true);
    resize_hitbox.set_above_child(true);
    resize_hitbox.set_halign(gtk::Align::End);
    resize_hitbox.set_valign(gtk::Align::End);
    resize_hitbox.set_size_request(RESIZE_HIT_SIZE, RESIZE_HIT_SIZE);
    resize_hitbox.style_context().add_class("resize-handle");
    resize_hitbox.add_events(
        gdk::EventMask::ENTER_NOTIFY_MASK
            | gdk::EventMask::LEAVE_NOTIFY_MASK
            | gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK,
    );
    shell.add_overlay(&resize_hitbox);
    event.add(&shell);

    let drag = event.clone();
    if !title.is_empty() || !kicker.is_empty() {
        let header = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        let titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let title_label = gtk::Label::new(Some(title));
        title_label.set_xalign(0.0);
        title_label.style_context().add_class("card-title");
        let kicker_label = gtk::Label::new(Some(kicker));
        kicker_label.set_xalign(0.0);
        kicker_label.style_context().add_class("card-kicker");
        titles.pack_start(&title_label, false, false, 0);
        titles.pack_start(&kicker_label, false, false, 0);
        header.pack_start(&titles, true, true, 0);
        card.pack_start(&header, false, false, 0);
    }

    if !title.is_empty() || !kicker.is_empty() {
        let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
        card.pack_start(&separator, false, false, 0);
    }
    let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
    body.set_hexpand(true);
    body.set_vexpand(true);
    body.style_context().add_class("card-body");
    card.pack_start(&body, true, true, 0);
    (
        event,
        body,
        drag,
        color_mode.clone(),
        ResizeHandle {
            hitbox: resize_hitbox,
            color_mode,
        },
    )
}

fn register(
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    key: &str,
    widget: &gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
) {
    widget
        .style_context()
        .add_class(color_mode.get().css_class());
    registry.borrow_mut().push(RegisteredWidget {
        key: key.into(),
        widget: widget.clone(),
        color_mode,
        edit_only: None,
        editor: None,
    });
}

fn saved_color_mode(state: &AppState, key: &str) -> ColorMode {
    state
        .widget_color_modes
        .get(key)
        .copied()
        .unwrap_or(state.settings.color_mode)
}

fn set_edit_chrome_visibility(registry: &Rc<RefCell<Vec<RegisteredWidget>>>, visible: bool) {
    for item in registry.borrow().iter() {
        if let Some(edit_only) = &item.edit_only {
            edit_only.set_visible(visible);
        }
        // Notes stay scrollable in lock mode (their rectangle stays in the
        // input shape), so their editor turns read-only instead of editable.
        if let Some(editor) = &item.editor {
            editor.set_editable(visible);
        }
    }
}

fn attach_color_mode_menu(
    widget: &gtk::EventBox,
    key: String,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
    timer_style: Option<TimerStylePreview>,
    system_details: Option<SystemDetailsPreview>,
) {
    let menu = gtk::Menu::new();
    for mode in [ColorMode::Light, ColorMode::Gray, ColorMode::Dark] {
        let item = gtk::MenuItem::with_label(mode.label());
        item.connect_activate({
            let key = key.clone();
            let state = state.clone();
            let registry = registry.clone();
            move |_| {
                state
                    .borrow_mut()
                    .widget_color_modes
                    .insert(key.clone(), mode);
                let _ = state.borrow().save();
                apply_widget_color_mode(&registry, &key, mode);
            }
        });
        menu.append(&item);
    }

    if let Some(timer_style) = timer_style {
        let parent = gtk::MenuItem::with_label("STYLE");
        let submenu = gtk::Menu::new();
        for style in TimerStyle::ALL {
            let item = gtk::MenuItem::with_label(style.label());
            item.connect_select({
                let timer_style = timer_style.clone();
                move |_| apply_timer_style(&timer_style, style)
            });
            item.connect_activate({
                let timer_style = timer_style.clone();
                let state = state.clone();
                move |_| {
                    apply_timer_style(&timer_style, style);
                    let mut data = state.borrow_mut();
                    data.timer_style = style;
                    data.sizes.insert("timer".into(), timer_style.size.get());
                    let _ = data.save();
                }
            });
            submenu.append(&item);
        }
        parent.set_submenu(Some(&submenu));
        menu.append(&parent);
        menu.connect_selection_done({
            let timer_style = timer_style.clone();
            let state = state.clone();
            move |_| apply_timer_style(&timer_style, state.borrow().timer_style)
        });

        let edit_time = gtk::MenuItem::with_label("EDIT TIME");
        edit_time.connect_activate({
            let open_edit = timer_style.open_edit.clone();
            move |_| {
                glib::timeout_add_local_once(Duration::from_millis(80), {
                    let open_edit = open_edit.clone();
                    move || open_edit()
                });
            }
        });
        menu.append(&edit_time);
    }
    if let Some(system_details) = system_details {
        menu.append(&gtk::SeparatorMenuItem::new());
        let cpu = gtk::CheckMenuItem::with_label("CPU");
        cpu.set_active(system_details.details.get().cpu);
        cpu.connect_toggled({
            let preview = system_details.clone();
            let state = state.clone();
            move |item| {
                let mut details = preview.details.get();
                details.cpu = item.is_active();
                apply_system_details(&preview, &state, details);
            }
        });
        menu.append(&cpu);

        let ram = gtk::CheckMenuItem::with_label("RAM");
        ram.set_active(system_details.details.get().ram);
        ram.connect_toggled({
            let preview = system_details.clone();
            let state = state.clone();
            move |item| {
                let mut details = preview.details.get();
                details.ram = item.is_active();
                apply_system_details(&preview, &state, details);
            }
        });
        menu.append(&ram);

        let processes = gtk::CheckMenuItem::with_label("TOP PROCESSES");
        processes.set_active(system_details.details.get().processes);
        processes.connect_toggled({
            let preview = system_details.clone();
            let state = state.clone();
            move |item| {
                let mut details = preview.details.get();
                details.processes = item.is_active();
                apply_system_details(&preview, &state, details);
            }
        });
        menu.append(&processes);

        let cores = gtk::CheckMenuItem::with_label("CPU CORES");
        cores.set_active(system_details.details.get().cores);
        cores.connect_toggled({
            let preview = system_details.clone();
            let state = state.clone();
            move |item| {
                let mut details = preview.details.get();
                details.cores = item.is_active();
                apply_system_details(&preview, &state, details);
            }
        });
        menu.append(&cores);
    }
    menu.show_all();

    let gesture = gtk::GestureMultiPress::new(widget);
    gesture.set_button(3);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    gesture.connect_pressed(move |gesture, _, _, _| {
        if !interactive.get() {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        gesture.set_state(gtk::EventSequenceState::Claimed);
        menu.popup_easy(3, gtk::current_event_time());
    });

    // GTK detaches event controllers when their final strong reference is dropped.
    unsafe {
        widget.set_data("sysi-color-menu-gesture", gesture);
    }
}

fn apply_system_details(
    preview: &SystemDetailsPreview,
    state: &Rc<RefCell<AppState>>,
    details: SystemDetails,
) {
    preview.details.set(details);
    let size = system_content_size(details, preview.values.borrow().cores.len());
    preview.card.set_size_request(size.width, size.height);
    preview.card.queue_resize();
    preview.canvas.queue_draw();
    let mut data = state.borrow_mut();
    data.settings.system_details = details;
    // Drop any stored size instead of pinning this one. Turning CPU CORES on
    // computes the height from the cores read so far — none, because the
    // reader was not collecting them — so it would lock the card to a single
    // row and, because a stored size means "the user resized this", the
    // periodic update would never grow it and the core grid stayed clipped.
    // Handing the card back to the layout lets that auto-grow run; a real
    // resize-handle drag still stores a size and still wins.
    data.sizes.remove("system");
    let _ = data.save();
}

fn place_card(root: &gtk::Fixed, card: &gtk::EventBox, point: Point) {
    root.put(card, point.x, point.y);
}

fn logical_screen_rects(scale: i32, fallback_width: i32, fallback_height: i32) -> Vec<ScreenRect> {
    let fallback = ScreenRect {
        x: 0,
        y: 0,
        width: fallback_width.max(1),
        height: fallback_height.max(1),
    };
    let Some(display) = gdk::Display::default() else {
        return vec![fallback];
    };
    let mut raw_screens = Vec::new();
    for index in 0..display.n_monitors() {
        if let Some(monitor) = display.monitor(index) {
            let geometry = monitor.geometry();
            raw_screens.push(ScreenRect {
                x: geometry.x(),
                y: geometry.y(),
                width: geometry.width(),
                height: geometry.height(),
            });
        }
    }
    if raw_screens.is_empty() {
        return vec![fallback];
    }
    let divisor = monitor_coordinate_divisor(&raw_screens, scale, fallback);
    let root_bounds = monitor_root_bounds(&raw_screens, divisor, fallback);
    let screens: Vec<_> = raw_screens
        .into_iter()
        .filter_map(|screen| normalize_monitor_rect(screen, divisor, root_bounds))
        .collect();
    if screens.is_empty() {
        vec![fallback]
    } else {
        screens
    }
}

// The overlay window must cover every monitor, including Xinerama screens
// with negative origins; the X root window's (0, 0) corner is not the
// bounding box's corner once a screen sits left of the primary.
fn monitor_root_bounds(
    raw_screens: &[ScreenRect],
    divisor: i32,
    fallback: ScreenRect,
) -> ScreenRect {
    let divisor = divisor.max(1);
    let min_x = raw_screens.iter().map(|screen| screen.x).min().unwrap_or(0);
    let min_y = raw_screens.iter().map(|screen| screen.y).min().unwrap_or(0);
    let max_x = raw_screens
        .iter()
        .map(|screen| screen.x.saturating_add(screen.width))
        .max()
        .unwrap_or(fallback.width);
    let max_y = raw_screens
        .iter()
        .map(|screen| screen.y.saturating_add(screen.height))
        .max()
        .unwrap_or(fallback.height);
    ScreenRect {
        x: (f64::from(min_x) / f64::from(divisor)).round() as i32,
        y: (f64::from(min_y) / f64::from(divisor)).round() as i32,
        width: (f64::from(max_x.saturating_sub(min_x)) / f64::from(divisor))
            .round()
            .max(1.0) as i32,
        height: (f64::from(max_y.saturating_sub(min_y)) / f64::from(divisor))
            .round()
            .max(1.0) as i32,
    }
}

fn logical_primary_screen(
    scale: i32,
    fallback_width: i32,
    fallback_height: i32,
) -> Option<ScreenRect> {
    let fallback = ScreenRect {
        x: 0,
        y: 0,
        width: fallback_width.max(1),
        height: fallback_height.max(1),
    };
    let display = gdk::Display::default()?;
    let monitor = display.primary_monitor()?;
    let primary_geometry = monitor.geometry();
    let primary = ScreenRect {
        x: primary_geometry.x(),
        y: primary_geometry.y(),
        width: primary_geometry.width(),
        height: primary_geometry.height(),
    };
    let raw_screens: Vec<_> = (0..display.n_monitors())
        .filter_map(|index| display.monitor(index))
        .map(|monitor| {
            let geometry = monitor.geometry();
            ScreenRect {
                x: geometry.x(),
                y: geometry.y(),
                width: geometry.width(),
                height: geometry.height(),
            }
        })
        .collect();
    if raw_screens.is_empty() {
        return Some(fallback);
    }
    let divisor = monitor_coordinate_divisor(&raw_screens, scale, fallback);
    let root_bounds = monitor_root_bounds(&raw_screens, divisor, fallback);
    normalize_monitor_rect(primary, divisor, root_bounds)
}

fn monitor_coordinate_divisor(
    raw_screens: &[ScreenRect],
    scale: i32,
    root_bounds: ScreenRect,
) -> i32 {
    if scale <= 1 || raw_screens.is_empty() {
        return 1;
    }
    let min_x = raw_screens.iter().map(|screen| screen.x).min().unwrap_or(0);
    let min_y = raw_screens.iter().map(|screen| screen.y).min().unwrap_or(0);
    let max_x = raw_screens
        .iter()
        .map(|screen| screen.x.saturating_add(screen.width))
        .max()
        .unwrap_or(root_bounds.width);
    let max_y = raw_screens
        .iter()
        .map(|screen| screen.y.saturating_add(screen.height))
        .max()
        .unwrap_or(root_bounds.height);
    if max_x.saturating_sub(min_x) > root_bounds.width
        || max_y.saturating_sub(min_y) > root_bounds.height
    {
        scale
    } else {
        1
    }
}

fn normalize_monitor_rect(
    screen: ScreenRect,
    divisor: i32,
    root_bounds: ScreenRect,
) -> Option<ScreenRect> {
    let divisor = divisor.max(1);
    let x = (f64::from(screen.x) / f64::from(divisor)).round() as i32;
    let y = (f64::from(screen.y) / f64::from(divisor)).round() as i32;
    let width = (f64::from(screen.width) / f64::from(divisor)).round() as i32;
    let height = (f64::from(screen.height) / f64::from(divisor)).round() as i32;
    let left = x.clamp(root_bounds.x, root_bounds.x + root_bounds.width);
    let top = y.clamp(root_bounds.y, root_bounds.y + root_bounds.height);
    let right = x
        .saturating_add(width)
        .clamp(root_bounds.x, root_bounds.x + root_bounds.width);
    let bottom = y
        .saturating_add(height)
        .clamp(root_bounds.y, root_bounds.y + root_bounds.height);
    (right > left && bottom > top).then_some(ScreenRect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn clamp_to_screens(point: Point, width: i32, height: i32, screens: &[ScreenRect]) -> Point {
    screens
        .iter()
        .map(|screen| {
            let max_x = (screen.x + screen.width - width).max(screen.x);
            let max_y = (screen.y + screen.height - height).max(screen.y);
            let candidate = Point {
                x: point.x.clamp(screen.x, max_x),
                y: point.y.clamp(screen.y, max_y),
            };
            let dx = i64::from(candidate.x - point.x);
            let dy = i64::from(candidate.y - point.y);
            (dx * dx + dy * dy, candidate)
        })
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, point)| point)
        .unwrap_or(point)
}

// Where a widget lands when it is switched back on. A widget hidden while it
// sat half off-screen — or one that grew past the screen edge while hidden —
// came back with its drag header out of reach, leaving nothing to grab it by.
// Reopening it under the pointer (the panel entry or picker button that was
// just clicked) keeps it where the user is already looking and always fully on
// a monitor; with no pointer it centres on the primary screen.
fn reopen_point(
    pointer: Option<(f64, f64)>,
    size: Size,
    screens: &[ScreenRect],
    primary: ScreenRect,
    avoid: Option<ScreenRect>,
) -> Point {
    let mut desired = pointer
        .map(|(x, y)| Point {
            x: x as i32 - size.width / 2,
            y: y as i32 + 24,
        })
        .unwrap_or(Point {
            x: primary.x + (primary.width - size.width).max(0) / 2,
            y: primary.y + (primary.height - size.height).max(0) / 2,
        });
    // The click that reopens a widget usually happens on the picker bar, which
    // draws over whatever lands underneath it — that is how the history header
    // became invisible and undraggable. Drop below the bar when the landing
    // rectangle would slide under it.
    if let Some(bar) = avoid {
        let overlaps = desired.x < bar.x + bar.width
            && desired.x + size.width > bar.x
            && desired.y < bar.y + bar.height
            && desired.y + size.height > bar.y;
        if overlaps {
            desired.y = bar.y + bar.height + 10;
        }
    }
    clamp_to_screens(desired, size.width, size.height, screens)
}

// Two widgets reopened from the same click would land on the same spot, one
// drawn straight over the other. Step off the occupied rectangles the way a
// new note cascades, keeping every candidate on a monitor.
fn cascade_point(
    point: Point,
    size: Size,
    occupied: &[ScreenRect],
    screens: &[ScreenRect],
) -> Point {
    let mut result = point;
    for _ in 0..12 {
        let clash = occupied.iter().any(|taken| {
            result.x < taken.x + taken.width
                && result.x + size.width > taken.x
                && result.y < taken.y + taken.height
                && result.y + size.height > taken.y
        });
        if !clash {
            break;
        }
        let next = clamp_to_screens(
            Point {
                x: result.x + 26,
                y: result.y + 26,
            },
            size.width,
            size.height,
            screens,
        );
        if next == result {
            break;
        }
        result = next;
    }
    result
}

// The card's own size request, which every resize and style change keeps
// current, so a hidden card — whose allocation is stale or never happened —
// still reopens with its real footprint accounted for.
fn card_size(card: &gtk::EventBox, fallback: Size) -> Size {
    let (width, height) = card.size_request();
    Size {
        width: if width > 1 { width } else { fallback.width },
        height: if height > 1 { height } else { fallback.height },
    }
}

#[allow(clippy::too_many_arguments)]
fn reopen_widget(
    card: &gtk::EventBox,
    key: &str,
    root: &gtk::Fixed,
    state: &Rc<RefCell<AppState>>,
    screens: &[ScreenRect],
    primary: ScreenRect,
    fallback: Size,
    avoid: Option<&gtk::EventBox>,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
) {
    let size = card_size(card, fallback);
    let point = reopen_point(
        pointer_position(),
        size,
        screens,
        primary,
        avoid.and_then(widget_rect),
    );
    let occupied: Vec<ScreenRect> = registry
        .borrow()
        .iter()
        .filter(|item| item.key != key && item.widget.is_visible())
        .filter_map(|item| widget_rect(&item.widget))
        .collect();
    let point = cascade_point(point, size, &occupied, screens);
    root.move_(card, point.x, point.y);
    state.borrow_mut().positions.insert(key.to_owned(), point);
}

fn widget_rect(widget: &gtk::EventBox) -> Option<ScreenRect> {
    let allocation = widget.allocation();
    (allocation.width() > 1 && allocation.height() > 1).then_some(ScreenRect {
        x: allocation.x(),
        y: allocation.y(),
        width: allocation.width(),
        height: allocation.height(),
    })
}

fn clamp_registered_widgets(
    root: &gtk::Fixed,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    screens: &[ScreenRect],
    state: &Rc<RefCell<AppState>>,
) {
    let mut data = state.borrow_mut();
    for item in registry.borrow().iter() {
        let allocation = item.widget.allocation();
        if allocation.width() <= 1 || allocation.height() <= 1 {
            continue;
        }
        let point = clamp_to_screens(
            Point {
                x: allocation.x(),
                y: allocation.y(),
            },
            allocation.width(),
            allocation.height(),
            screens,
        );
        if point.x != allocation.x() || point.y != allocation.y() {
            root.move_(&item.widget, point.x, point.y);
        }
        if let Some(id) = item
            .key
            .strip_prefix("note:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            if let Some(note) = data.notes.iter_mut().find(|note| note.id == id) {
                note.position = point;
            }
        } else {
            data.positions.insert(item.key.clone(), point);
        }
    }
    let _ = data.save();
    root.queue_draw();
}

fn apply_widget_size(
    card: &gtk::EventBox,
    key: &str,
    state: &Rc<RefCell<AppState>>,
    fallback: Size,
) {
    let saved = state.borrow().sizes.get(key).copied().unwrap_or(fallback);
    let width = if saved.width > 0 {
        saved.width
    } else {
        fallback.width
    };
    let height = if saved.height > 0 {
        saved.height
    } else {
        fallback.height
    };
    card.set_size_request(width, height);
}

#[allow(clippy::too_many_arguments)]
fn attach_resize(
    handle: &ResizeHandle,
    card: &gtk::EventBox,
    root: &gtk::Fixed,
    key: String,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
    window: gtk::ApplicationWindow,
    bounds: ResizeBounds,
) {
    handle.hitbox.set_tooltip_text(Some("Drag to resize"));
    handle.hitbox.connect_enter_notify_event({
        let interactive = interactive.clone();
        move |widget, _| {
            if interactive.get() {
                if let Some(window) = widget.window() {
                    let cursor = gdk::Cursor::for_display(
                        &window.display(),
                        gdk::CursorType::BottomRightCorner,
                    );
                    window.set_cursor(cursor.as_ref());
                }
            }
            glib::Propagation::Proceed
        }
    });
    handle.hitbox.connect_leave_notify_event(|widget, _| {
        if let Some(window) = widget.window() {
            window.set_cursor(None);
        }
        glib::Propagation::Proceed
    });
    handle.hitbox.connect_draw({
        let interactive = interactive.clone();
        let color_mode = handle.color_mode.clone();
        move |area, ctx| {
            if !interactive.get() {
                return glib::Propagation::Proceed;
            }
            let allocation = area.allocation();
            let width = f64::from(allocation.width());
            let height = f64::from(allocation.height());
            let gray = match color_mode.get() {
                ColorMode::Light => 0.9,
                ColorMode::Gray => 0.6,
                ColorMode::Dark => 0.13,
            };
            ctx.set_source_rgba(gray, gray, gray, 0.82);
            ctx.set_line_width(1.35);
            ctx.set_line_cap(cairo::LineCap::Round);
            ctx.new_sub_path();
            ctx.arc(width - 8.5, height - 8.5, 6.5, 0.0, PI / 2.0);
            let _ = ctx.stroke();
            glib::Propagation::Proceed
        }
    });

    let start = Rc::new(Cell::new(None::<(i32, i32, f64, f64)>));
    let latest = Rc::new(Cell::new(Size::default()));
    handle.hitbox.connect_button_press_event({
        let start = start.clone();
        let latest = latest.clone();
        let card = card.clone();
        let interactive = interactive.clone();
        move |_, event| {
            if !interactive.get() || event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            let allocation = card.allocation();
            let (pointer_x, pointer_y) = event.root();
            latest.set(Size {
                width: allocation.width(),
                height: allocation.height(),
            });
            start.set(Some((
                allocation.width(),
                allocation.height(),
                pointer_x,
                pointer_y,
            )));
            glib::Propagation::Stop
        }
    });
    handle.hitbox.connect_motion_notify_event({
        let start = start.clone();
        let latest = latest.clone();
        let card = card.clone();
        let root = root.clone();
        move |_, event| {
            let Some((start_width, start_height, pointer_start_x, pointer_start_y)) = start.get()
            else {
                return glib::Propagation::Proceed;
            };
            // If the release happened outside the overlay (broken grab), the
            // press state goes stale and hovering would keep resizing.
            if !event.state().contains(gdk::ModifierType::BUTTON1_MASK) {
                start.set(None);
                return glib::Propagation::Proceed;
            }
            let (pointer_x, pointer_y) = event.root();
            let delta_x = pointer_x - pointer_start_x;
            let delta_y = pointer_y - pointer_start_y;

            let allocation = card.allocation();
            let root_allocation = root.allocation();
            let screens = logical_screen_rects(
                card.scale_factor(),
                root_allocation.width(),
                root_allocation.height(),
            );
            let screen_limit = screens
                .iter()
                .find(|screen| {
                    allocation.x() >= screen.x
                        && allocation.x() < screen.x + screen.width
                        && allocation.y() >= screen.y
                        && allocation.y() < screen.y + screen.height
                })
                .copied();
            let available_width = screen_limit
                .map(|screen| screen.x + screen.width - allocation.x())
                .unwrap_or(bounds.max_width);
            let available_height = screen_limit
                .map(|screen| screen.y + screen.height - allocation.y())
                .unwrap_or(bounds.max_height);
            let max_width = bounds.max_width.min(available_width).max(bounds.min_width);
            let max_height = bounds
                .max_height
                .min(available_height)
                .max(bounds.min_height);

            let aspect_ratio = if bounds.preserve_current_aspect {
                Some(f64::from(start_width) / f64::from(start_height.max(1)))
            } else {
                bounds.aspect_ratio
            };
            let next = if let Some(ratio) = aspect_ratio {
                let width_factor = (f64::from(start_width) + delta_x) / f64::from(start_width);
                let height_factor = (f64::from(start_height) + delta_y) / f64::from(start_height);
                let desired_factor = if delta_x.abs() >= delta_y.abs() {
                    width_factor
                } else {
                    height_factor
                };
                let min_factor = (f64::from(bounds.min_width) / f64::from(start_width))
                    .max(f64::from(bounds.min_height) / f64::from(start_height));
                let max_factor = (f64::from(max_width) / f64::from(start_width))
                    .min(f64::from(max_height) / f64::from(start_height));
                let factor = desired_factor.clamp(min_factor, max_factor.max(min_factor));
                let width = (f64::from(start_width) * factor).round() as i32;
                Size {
                    width,
                    height: (f64::from(width) / ratio).round() as i32,
                }
            } else {
                Size {
                    width: (f64::from(start_width) + delta_x).round() as i32,
                    height: (f64::from(start_height) + delta_y).round() as i32,
                }
            };
            let next = Size {
                width: next.width.clamp(bounds.min_width, max_width),
                height: next.height.clamp(bounds.min_height, max_height),
            };
            latest.set(next);
            card.set_size_request(next.width, next.height);
            card.queue_resize();
            root.queue_draw();
            glib::Propagation::Stop
        }
    });
    handle.hitbox.connect_button_release_event({
        let start = start.clone();
        let latest = latest.clone();
        let card = card.clone();
        let handle_hitbox = handle.hitbox.clone();
        let window = window.clone();
        let registry = registry.clone();
        move |_, event| {
            if event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            if start.replace(None).is_none() {
                return glib::Propagation::Proceed;
            }
            let size = latest.get();
            state.borrow_mut().sizes.insert(key.clone(), size);
            let _ = state.borrow().save();
            card.queue_resize();
            handle_hitbox.queue_draw();
            let window = window.clone();
            let registry = registry.clone();
            let enabled = interactive.get();
            glib::idle_add_local_once(move || {
                refresh_input_shape(&window, &registry, enabled);
            });
            glib::Propagation::Stop
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn attach_drag(
    handle: &gtk::EventBox,
    card: &gtk::EventBox,
    root: &gtk::Fixed,
    key: String,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
    window: gtk::ApplicationWindow,
) {
    let gesture = gtk::GestureDrag::new(handle);
    gesture.set_button(1);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    // GestureDrag's local deltas are rounded around HiDPI scale boundaries.
    // Follow the root pointer instead, so moving the Fixed child never feeds
    // back into the next drag delta.
    let start = Rc::new(Cell::new(None::<(i32, i32, f64, f64)>));
    gesture.connect_drag_begin({
        let start = start.clone();
        let card = card.clone();
        let interactive = interactive.clone();
        move |gesture, local_x, local_y| {
            if interactive.get() {
                let allocation = card.allocation();
                if local_x >= f64::from(allocation.width() - RESIZE_HIT_SIZE)
                    && local_y >= f64::from(allocation.height() - RESIZE_HIT_SIZE)
                {
                    gesture.set_state(gtk::EventSequenceState::Denied);
                    return;
                }
                let (pointer_x, pointer_y) = pointer_position().unwrap_or((
                    f64::from(allocation.x()) + local_x,
                    f64::from(allocation.y()) + local_y,
                ));
                start.set(Some((allocation.x(), allocation.y(), pointer_x, pointer_y)));
            } else {
                gesture.set_state(gtk::EventSequenceState::Denied);
            }
        }
    });
    gesture.connect_drag_update({
        let start = start.clone();
        let card = card.clone();
        let root = root.clone();
        move |gesture, fallback_x, fallback_y| {
            if let Some((ox, oy, pointer_start_x, pointer_start_y)) = start.get() {
                let (pointer_x, pointer_y) = pointer_position()
                    .unwrap_or((pointer_start_x + fallback_x, pointer_start_y + fallback_y));
                let offset_x = pointer_x - pointer_start_x;
                let offset_y = pointer_y - pointer_start_y;
                if offset_x.abs() + offset_y.abs() > 4.0 {
                    gesture.set_state(gtk::EventSequenceState::Claimed);
                }
                let allocation = card.allocation();
                let root_allocation = root.allocation();
                let screens = logical_screen_rects(
                    card.scale_factor(),
                    root_allocation.width(),
                    root_allocation.height(),
                );
                let point = clamp_to_screens(
                    Point {
                        x: ox + offset_x as i32,
                        y: oy + offset_y as i32,
                    },
                    allocation.width(),
                    allocation.height(),
                    &screens,
                );
                if point.x != allocation.x() || point.y != allocation.y() {
                    root.queue_draw_area(
                        allocation.x() - 3,
                        allocation.y() - 3,
                        allocation.width() + 6,
                        allocation.height() + 6,
                    );
                    root.move_(&card, point.x, point.y);
                    root.queue_draw_area(
                        point.x - 3,
                        point.y - 3,
                        allocation.width() + 6,
                        allocation.height() + 6,
                    );
                }
            }
        }
    });
    gesture.connect_drag_end({
        let start = start.clone();
        let card = card.clone();
        let registry = registry.clone();
        let window = window.clone();
        let interactive = interactive.clone();
        move |_, _, _| {
            if start.get().is_none() {
                return;
            }
            start.set(None);
            let allocation = card.allocation();
            let point = Point {
                x: allocation.x(),
                y: allocation.y(),
            };
            let mut data = state.borrow_mut();
            if let Some(id) = key
                .strip_prefix("note:")
                .and_then(|id| id.parse::<u64>().ok())
            {
                if let Some(note) = data.notes.iter_mut().find(|note| note.id == id) {
                    note.position = point;
                }
            } else {
                data.positions.insert(key.clone(), point);
            }
            let _ = data.save();
            refresh_input_shape(&window, &registry, interactive.get());
        }
    });

    // A GTK gesture is detached when its final strong reference is dropped.
    // Keep it with the handle so dragging remains active for the widget's lifetime.
    unsafe {
        handle.set_data("sysi-drag-gesture", gesture);
    }
}

fn pointer_position() -> Option<(f64, f64)> {
    let display = gdk::Display::default()?;
    let seat = display.default_seat()?;
    let pointer = seat.pointer()?;
    let (_, x, y) = pointer.position_double();
    Some((x, y))
}
fn present_overlay(window: &gtk::ApplicationWindow) {
    // present() with GDK_CURRENT_TIME makes GNOME's WM log "Buggy client sent
    // a _NET_ACTIVE_WINDOW message with a timestamp of 0" and lets the overlay
    // steal focus from the app the user is typing in. Use the X server's last
    // user-interaction timestamp when one is available.
    let timestamp = gdk::Display::default()
        .and_then(|display| {
            display
                .downcast_ref::<gdkx11::X11Display>()
                .map(|display| display.user_time())
        })
        .unwrap_or(0);
    if timestamp != 0 {
        window.present_with_time(timestamp);
    } else {
        window.present();
    }
}

// One widget's contribution to the overlay's input shape. Compared against
// the previous refresh so an unchanged shape costs no X round-trip, which is
// what makes it safe to recompute on every re-layout.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ShapePart {
    Rect(i32, i32, i32, i32),
    Circle(i32, i32, i32, i32),
}

thread_local! {
    static LAST_INPUT_SHAPE: RefCell<Option<Vec<ShapePart>>> = const { RefCell::new(None) };
}

// The cache tracks what was last pushed to *this* GdkWindow. A remap gives the
// overlay a fresh, unshaped X window, so the cache has to be dropped or the
// next refresh would decide there was nothing to do and leave every widget
// click-through.
fn invalidate_input_shape_cache() {
    LAST_INPUT_SHAPE.with(|last| *last.borrow_mut() = None);
}

fn refresh_input_shape(
    window: &gtk::ApplicationWindow,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: bool,
) {
    let Some(gdk_window) = window.window() else {
        // No X window to shape yet; forget the cache so the refresh that runs
        // once it exists actually pushes a region.
        invalidate_input_shape_cache();
        return;
    };
    let mut parts: Vec<ShapePart> = Vec::new();
    for item in registry.borrow().iter() {
        let lock_timer = item.key == "timer";
        let settings = item.key == "picker";
        // Notes and the history list keep receiving input in lock mode so
        // their content can still be scrolled; editing is disabled separately
        // via the read-only editor, and their headers hide as edit chrome.
        let lock_note = item.key.starts_with("note:") || item.key == "history";
        if interactive
            || settings
            || lock_timer
            || lock_note
            || item.widget.style_context().has_class("alarm")
        {
            if !item.widget.is_visible() || !item.widget.is_mapped() {
                continue;
            }
            let allocation = item.widget.allocation();
            if allocation.width() > 1 && allocation.height() > 1 {
                let part = if !interactive && lock_timer {
                    ShapePart::Circle
                } else {
                    ShapePart::Rect
                };
                parts.push(part(
                    allocation.x(),
                    allocation.y(),
                    allocation.width(),
                    allocation.height(),
                ));
            }
        }
    }
    if LAST_INPUT_SHAPE.with(|last| last.borrow().as_deref() == Some(parts.as_slice())) {
        return;
    }
    let region = Region::create();
    for part in &parts {
        match *part {
            ShapePart::Rect(x, y, width, height) => {
                let _ = region.union_rectangle(&RectangleInt::new(x, y, width, height));
            }
            ShapePart::Circle(x, y, width, height) => {
                union_circle_region(&region, x, y, width, height);
            }
        }
    }
    gdk_window.input_shape_combine_region(&region, 0, 0);
    LAST_INPUT_SHAPE.with(|last| *last.borrow_mut() = Some(parts));
}

fn union_circle_region(region: &Region, x: i32, y: i32, width: i32, height: i32) {
    let radius = (width.min(height) / 2 - 10).clamp(1, 62);
    let cx = x + width / 2;
    let cy = y + height / 2;
    for dy in (-radius..=radius).step_by(2) {
        let half = ((radius * radius - dy * dy) as f64).sqrt() as i32;
        let _ = region.union_rectangle(&RectangleInt::new(cx - half, cy + dy, half * 2 + 1, 2));
    }
}

fn apply_color_mode(registry: &Rc<RefCell<Vec<RegisteredWidget>>>, mode: ColorMode) {
    for item in registry.borrow().iter() {
        set_registered_color_mode(item, mode);
    }
}

fn apply_widget_color_mode(
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    key: &str,
    mode: ColorMode,
) {
    if let Some(item) = registry.borrow().iter().find(|item| item.key == key) {
        set_registered_color_mode(item, mode);
    }
}

fn set_registered_color_mode(item: &RegisteredWidget, mode: ColorMode) {
    set_event_box_color_mode(&item.widget, &item.color_mode, mode);
}

fn set_event_box_color_mode(
    widget: &gtk::EventBox,
    color_mode: &Rc<Cell<ColorMode>>,
    mode: ColorMode,
) {
    color_mode.set(mode);
    let context = widget.style_context();
    context.remove_class("mode-light");
    context.remove_class("mode-gray");
    context.remove_class("mode-dark");
    context.add_class(mode.css_class());
    widget.queue_draw();
}

fn apply_timer_style(preview: &TimerStylePreview, style: TimerStyle) {
    let previous = preview.style.get();
    if previous != style {
        let size = timer_style_size(preview.size.get(), previous, style);
        preview.size.set(size);
        preview.card.set_size_request(size.width, size.height);
        preview.card.queue_resize();
    }
    preview.style.set(style);
    let context = preview.card.style_context();
    for style in TimerStyle::ALL {
        context.remove_class(style.css_class());
    }
    context.add_class(style.css_class());
    apply_timer_typography(&preview.typography, style, preview.size.get());
    preview.canvas.queue_draw();
    glib::idle_add_local_once({
        let window = preview.window.clone();
        let registry = preview.registry.clone();
        let interactive = preview.interactive.clone();
        move || refresh_input_shape(&window, &registry, interactive.get())
    });
}

fn apply_timer_typography(provider: &gtk::CssProvider, style: TimerStyle, size: Size) {
    let reference = style.default_size();
    let scale = (f64::from(size.width.max(1)) / f64::from(reference.width))
        .min(f64::from(size.height.max(1)) / f64::from(reference.height))
        .clamp(0.45, 3.2);
    let (value_base, action_base, editor_base) = match style {
        TimerStyle::Digital => (25.0, 9.0, 17.0),
        TimerStyle::Ring | TimerStyle::Ticks | TimerStyle::Arc => (25.0, 11.0, 18.0),
    };
    let value = (value_base * scale).round().max(10.0);
    let action = (action_base * scale).round().max(7.0);
    let editor = (editor_base * scale).round().max(10.0);
    // The alarm message has many more glyphs than a time value, but must
    // still track resizing. The action scale keeps it inside every style's
    // default footprint and grows proportionally with the widget.
    let alarm = action;
    let css = format!(
        ".timer-value {{ font-size: {value}px; }}\n.timer-alarm-value {{ font-size: {alarm}px; }}\n.timer-action {{ font-size: {action}px; }}\n.timer-editor {{ font-size: {editor}px; }}"
    );
    let _ = provider.load_from_data(css.as_bytes());
}

fn timer_style_size(size: Size, from: TimerStyle, to: TimerStyle) -> Size {
    let from_default = from.default_size();
    let to_default = to.default_size();
    let factor = (f64::from(size.width.max(1)) / f64::from(from_default.width))
        .min(f64::from(size.height.max(1)) / f64::from(from_default.height));
    Size {
        width: (f64::from(to_default.width) * factor).round().max(1.0) as i32,
        height: (f64::from(to_default.height) * factor).round().max(1.0) as i32,
    }
}

fn small_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.style_context().add_class("tiny-button");
    button.set_can_focus(false);
    button
}

fn icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_can_focus(false);
    button.set_tooltip_text(Some(tooltip));
    button.style_context().add_class("tiny-button");
    button.style_context().add_class("note-window-button");
    // A symbolic icon recolours to the button's CSS colour, so it follows the
    // header text in every colour mode; a text glyph would depend on the
    // system font having it.
    let icon = gtk::Image::from_icon_name(Some(icon_name), gtk::IconSize::Menu);
    icon.set_pixel_size(11);
    button.add(&icon);
    button
}

fn format_duration(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}

fn format_duration_ceil(duration: Duration) -> String {
    let millis = duration.as_millis();
    format_duration(millis.div_ceil(1000) as i64)
}

fn truncate_chars(text: &str, max: usize) -> String {
    let mut chars = text.chars();
    let mut result: String = chars.by_ref().take(max).collect();
    if chars.next().is_some() {
        result.push('…');
    }
    result
}

fn center_text(
    ctx: &Context,
    x: f64,
    y: f64,
    text: &str,
    size: f64,
    weight: FontWeight,
    color: (f64, f64, f64),
) {
    ctx.select_font_face("Sans", FontSlant::Normal, weight);
    ctx.set_font_size(size);
    let mut origin = x;
    if let Ok(extents) = ctx.text_extents(text) {
        origin -= extents.width() / 2.0 + extents.x_bearing();
    }
    ctx.set_source_rgb(color.0, color.1, color.2);
    ctx.move_to(origin, y);
    let _ = ctx.show_text(text);
}

fn install_css(screen: &gdk::Screen) {
    let css = include_str!("style.css");
    let provider = gtk::CssProvider::new();
    if let Err(error) = provider.load_from_data(css.as_bytes()) {
        eprintln!("Sysi CSS error: {error}");
    }
    gtk::StyleContext::add_provider_for_screen(
        screen,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

#[cfg(test)]
mod timer_input_tests {
    use super::{
        cascade_point, history_row_budget, monitor_coordinate_divisor, monitor_root_bounds,
        normalize_monitor_rect, parse_timer_input, reopen_point, system_content_size,
        timer_style_size, ScreenRect, HISTORY_HEIGHT, HISTORY_WIDTH,
    };
    use crate::state::{Point, Size, SystemDetails, TimerStyle};

    const SCREEN: ScreenRect = ScreenRect {
        x: 0,
        y: 0,
        width: 1280,
        height: 800,
    };
    const HISTORY: Size = Size {
        width: HISTORY_WIDTH,
        height: HISTORY_HEIGHT,
    };

    #[test]
    fn a_reopened_window_lands_near_the_click_and_fully_on_screen() {
        let point = reopen_point(Some((900.0, 300.0)), HISTORY, &[SCREEN], SCREEN, None);
        assert_eq!(point, Point { x: 782, y: 324 });
        // A click at the far edge still yields a window whose header — its only
        // drag handle — is on screen.
        let edge = reopen_point(Some((1279.0, 795.0)), HISTORY, &[SCREEN], SCREEN, None);
        assert!(edge.x >= SCREEN.x && edge.x + HISTORY.width <= SCREEN.width);
        assert!(edge.y >= SCREEN.y && edge.y + HISTORY.height <= SCREEN.height);
    }

    #[test]
    fn a_reopened_window_without_a_pointer_centres_on_the_primary_screen() {
        let point = reopen_point(None, HISTORY, &[SCREEN], SCREEN, None);
        assert_eq!(
            point,
            Point {
                x: (SCREEN.width - HISTORY.width) / 2,
                y: (SCREEN.height - HISTORY.height) / 2,
            }
        );
    }

    #[test]
    fn two_widgets_reopened_from_one_click_do_not_stack_on_each_other() {
        let taken = ScreenRect {
            x: 600,
            y: 300,
            width: 200,
            height: 120,
        };
        let stacked = Point { x: 600, y: 300 };
        let point = cascade_point(stacked, HISTORY, &[taken], &[SCREEN]);
        assert_ne!(point, stacked);
        assert!(
            point.x >= taken.x + taken.width || point.y >= taken.y + taken.height,
            "the second widget must step clear of the first: {point:?}"
        );
        // Nothing in the way leaves the landing spot exactly where it was.
        assert_eq!(cascade_point(stacked, HISTORY, &[], &[SCREEN]), stacked);
    }

    #[test]
    fn a_reopened_window_never_lands_under_the_picker_bar() {
        let bar = ScreenRect {
            x: 12,
            y: 30,
            width: 940,
            height: 40,
        };
        // Clicking "history" on the bar itself: the window drops clear of the
        // bar instead of sliding under it, where its header is unreachable.
        let point = reopen_point(Some((850.0, 48.0)), HISTORY, &[SCREEN], SCREEN, Some(bar));
        assert!(
            point.y >= bar.y + bar.height,
            "reopened window must clear the picker bar: {point:?}"
        );
        // A click well below the bar is left alone.
        let clear = reopen_point(Some((850.0, 400.0)), HISTORY, &[SCREEN], SCREEN, Some(bar));
        assert_eq!(clear.y, 424);
    }

    #[test]
    fn a_taller_history_window_renders_more_rows_than_a_shorter_one() {
        let short = history_row_budget(120);
        let tall = history_row_budget(600);
        assert!(
            tall > short,
            "dragging the history window taller must render more rows: {short} -> {tall}"
        );
        // Even a window dragged to its minimum keeps a scrollable buffer, and
        // a huge one stays bounded so each keystroke rebuild stays cheap.
        assert_eq!(history_row_budget(0), 14);
        assert!(history_row_budget(100_000) <= 240);
        assert!(history_row_budget(HISTORY_HEIGHT) >= 14);
    }

    #[test]
    fn parses_supported_timer_formats() {
        assert_eq!(parse_timer_input("10"), Some(600));
        assert_eq!(parse_timer_input("10:50"), Some(650));
        assert_eq!(parse_timer_input("1:02:03"), Some(3_723));
    }

    #[test]
    fn rejects_invalid_timer_formats() {
        assert_eq!(parse_timer_input("0"), None);
        assert_eq!(parse_timer_input("10:99"), None);
        assert_eq!(parse_timer_input("hello"), None);
    }

    #[test]
    fn physical_monitor_geometry_is_normalized_for_hidpi_overlay() {
        let root = ScreenRect {
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
        };
        let physical = ScreenRect {
            x: 0,
            y: 0,
            width: 2560,
            height: 1440,
        };
        let divisor = monitor_coordinate_divisor(&[physical], 2, root);
        assert_eq!(divisor, 2);
        assert_eq!(normalize_monitor_rect(physical, divisor, root), Some(root));
    }
    #[test]
    fn monitors_left_of_the_primary_keep_negative_coordinates() {
        let fallback = ScreenRect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let raw = [
            ScreenRect {
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
            ScreenRect {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        let bounds = monitor_root_bounds(&raw, 1, fallback);
        assert_eq!(
            bounds,
            ScreenRect {
                x: -1920,
                y: 0,
                width: 3840,
                height: 1080,
            }
        );
    }

    #[test]
    fn digital_timer_style_uses_a_compact_rectangular_container() {
        let ring = TimerStyle::Ring.default_size();
        let digital = timer_style_size(ring, TimerStyle::Ring, TimerStyle::Digital);
        assert_eq!(
            digital,
            Size {
                width: 84,
                height: 36
            }
        );
        assert_eq!(
            timer_style_size(digital, TimerStyle::Digital, TimerStyle::Ring),
            ring
        );
    }

    #[test]
    fn a_single_system_meter_uses_a_tight_square_container() {
        let size = system_content_size(
            SystemDetails {
                cpu: true,
                ram: false,
                processes: false,
                cores: false,
            },
            0,
        );
        assert_eq!(
            size,
            Size {
                width: 76,
                height: 76
            }
        );
    }
}
