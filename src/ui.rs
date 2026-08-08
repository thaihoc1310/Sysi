use crate::{
    platform,
    state::{AppState, ColorMode, Note, Point, Size},
    system::SystemReader,
};
use cairo::{Context, FontSlant, FontWeight, RectangleInt, Region};
use gdk::prelude::*;
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    f64::consts::{PI, TAU},
    rc::Rc,
    time::{Duration, Instant},
};

const SYSTEM_WIDTH: i32 = 224;
const SYSTEM_HEIGHT: i32 = 90;
const TIMER_SIZE: i32 = 132;
const NOTE_WIDTH: i32 = 218;
const NOTE_HEIGHT: i32 = 124;
const RESIZE_HIT_SIZE: i32 = 18;

type CallbackSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
type SystemValues = Rc<Cell<(f64, f64, f64, f64, f64)>>;
type BuiltSystemCard = (
    gtk::EventBox,
    gtk::EventBox,
    Rc<Cell<ColorMode>>,
    gtk::DrawingArea,
    SystemValues,
    ResizeHandle,
);

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
}

#[derive(Clone)]
struct RegisteredWidget {
    key: String,
    widget: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    edit_only: Option<gtk::EventBox>,
}

struct TimerRuntime {
    duration_seconds: i64,
    remaining: Duration,
    target: Option<Instant>,
    started: bool,
    alarm: bool,
    phase: f64,
}

struct MascotRuntime {
    x: f64,
    target: f64,
    target_top: i32,
    phase: f64,
    pause: u16,
    message: String,
    seed: u64,
    reaction: u16,
    sequence: PetSequence,
    sequence_tick: u16,
    target_key: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PetSequence {
    Roam,
    Climb,
    Throw,
}

struct MascotSheets {
    basic: gdk_pixbuf::Pixbuf,
    climb: gdk_pixbuf::Pixbuf,
    mischief: gdk_pixbuf::Pixbuf,
}

#[derive(Clone, Copy, Debug)]
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
    window.set_type_hint(gdk::WindowTypeHint::Utility);

    if let Some(screen) = gtk::prelude::WidgetExt::screen(&window) {
        if let Some(visual) = screen.rgba_visual() {
            window.set_visual(Some(&visual));
        }
    }

    let screen = gdk::Screen::default().expect("Sysi requires a graphical display");
    let root_window = screen.root_window().expect("display root window");
    let scale = window.scale_factor().max(1);
    let screen_width = root_window.width() / scale;
    let screen_height = root_window.height() / scale;
    let screens = logical_screen_rects(scale, screen_width, screen_height);
    let primary_screen = logical_primary_screen(scale).unwrap_or(screens[0]);
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
    }
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
    let interactive = Rc::new(Cell::new(false));
    let (system_color_mode, timer_color_mode, picker_color_mode) = {
        let data = state.borrow();
        (
            saved_color_mode(&data, "system"),
            saved_color_mode(&data, "timer"),
            saved_color_mode(&data, "picker"),
        )
    };

    let system_card = build_system_card(system_color_mode);
    apply_widget_size(
        &system_card.0,
        "system",
        &state,
        Size {
            width: SYSTEM_WIDTH,
            height: SYSTEM_HEIGHT,
        },
    );
    place_card(
        &root,
        &system_card.0,
        state
            .borrow()
            .positions
            .get("system")
            .copied()
            .unwrap_or(Point { x: 34, y: 52 }),
    );
    register(&registry, "system", &system_card.0, system_card.2.clone());
    attach_color_mode_menu(
        &system_card.0,
        "system".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
    );
    attach_drag(
        &system_card.1,
        &system_card.0,
        &root,
        "system".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
    );
    attach_resize(
        &system_card.5,
        &system_card.0,
        &root,
        "system".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
        ResizeBounds {
            min_width: 160,
            min_height: 64,
            max_width: 520,
            max_height: 210,
            aspect_ratio: Some(f64::from(SYSTEM_WIDTH) / f64::from(SYSTEM_HEIGHT)),
        },
    );

    let timer_card = build_timer_card(state.clone(), interactive.clone(), timer_color_mode);
    let timer_default = Point {
        x: (primary_screen.x + primary_screen.width - TIMER_SIZE - 34).max(primary_screen.x + 8),
        y: primary_screen.y + 34,
    };
    let timer_position = state
        .borrow()
        .positions
        .get("timer")
        .copied()
        .unwrap_or(timer_default);
    apply_widget_size(
        &timer_card.card,
        "timer",
        &state,
        Size {
            width: TIMER_SIZE,
            height: TIMER_SIZE,
        },
    );
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
            min_width: 92,
            min_height: 92,
            max_width: 320,
            max_height: 320,
            aspect_ratio: Some(1.0),
        },
    );

    let widget_picker = build_widget_picker(picker_color_mode);
    let picker_position = state
        .borrow()
        .positions
        .get("picker")
        .copied()
        .unwrap_or(Point {
            x: primary_screen.x + 360,
            y: primary_screen.y + 54,
        });
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
    let note_refresh: CallbackSlot = Rc::new(RefCell::new(None));
    let refresh_closure: Rc<dyn Fn()> = {
        let root = root.clone();
        let state = state.clone();
        let registry = registry.clone();
        let list_box = widget_picker.history_list.clone();
        let note_refresh = note_refresh.clone();
        let interactive = interactive.clone();
        let window = window.clone();
        Rc::new(move || {
            rebuild_note_list(&list_box, &root, state.clone(), note_refresh.clone());
            rebuild_pinned_notes(
                &root,
                state.clone(),
                registry.clone(),
                note_refresh.clone(),
                interactive.clone(),
                window.clone(),
            );
            refresh_input_shape(&window, &registry, interactive.get());
        })
    };
    *note_refresh.borrow_mut() = Some(refresh_closure.clone());
    refresh_closure();

    let (system_enabled, timer_enabled, settings_enabled, color_mode) = {
        let settings = &state.borrow().settings;
        (
            settings.system,
            settings.timer,
            settings.settings_button,
            settings.color_mode,
        )
    };
    widget_picker.system.set_active(system_enabled);
    widget_picker.timer.set_active(timer_enabled);
    widget_picker.mode.set_label(color_mode.label());

    widget_picker.system.connect_toggled({
        let target = system_card.0.clone();
        let state = state.clone();
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        move |button| {
            let enabled = button.is_active();
            if enabled {
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
        move |button| {
            let enabled = button.is_active();
            if enabled {
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

    widget_picker.history.connect_clicked({
        let revealer = widget_picker.history_revealer.clone();
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        let root = root.clone();
        let screens = screens.clone();
        let state = state.clone();
        move |_| {
            revealer.set_reveal_child(!revealer.reveals_child());
            refresh_shape_during_transition(&window, &registry, interactive.clone());
            let root = root.clone();
            let registry = registry.clone();
            let screens = screens.clone();
            let state = state.clone();
            glib::timeout_add_local(Duration::from_millis(230), move || {
                clamp_registered_widgets(&root, &registry, &screens, &state);
                glib::ControlFlow::Break
            });
        }
    });
    widget_picker.new_note.connect_clicked({
        let root = root.clone();
        let picker = widget_picker.card.clone();
        let revealer = widget_picker.revealer.clone();
        let history_revealer = widget_picker.history_revealer.clone();
        let state = state.clone();
        let refresh = refresh_closure.clone();
        let screens = screens.clone();
        move |_| {
            let allocation = picker.allocation();
            let position = clamp_to_screens(
                Point {
                    x: allocation.x() + 205,
                    y: allocation.y() + 40,
                },
                NOTE_WIDTH,
                NOTE_HEIGHT,
                &screens,
            );
            let mut data = state.borrow_mut();
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
            history_revealer.set_reveal_child(false);
            refresh();
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
        let revealer = widget_picker.revealer.clone();
        let history_revealer = widget_picker.history_revealer.clone();
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
                revealer.set_reveal_child(false);
                history_revealer.set_reveal_child(false);
                window.style_context().remove_class("editing");
            }
            set_edit_chrome_visibility(&registry, enabled);
            for item in registry.borrow().iter() {
                item.widget.queue_draw();
            }
            refresh_input_shape(&window, &registry, enabled);
        })
    };

    let toggle_settings_action: Rc<dyn Fn()> = {
        let picker = widget_picker.card.clone();
        let revealer = widget_picker.revealer.clone();
        let history_revealer = widget_picker.history_revealer.clone();
        let state = state.clone();
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        Rc::new(move || {
            let visible = !picker.is_visible();
            picker.set_visible(visible);
            if !visible {
                revealer.set_reveal_child(false);
                history_revealer.set_reveal_child(false);
            }
            state.borrow_mut().settings.settings_button = visible;
            let _ = state.borrow().save();
            refresh_input_shape(&window, &registry, interactive.get());
        })
    };

    widget_picker.plus.connect_clicked({
        let revealer = widget_picker.revealer.clone();
        let history_revealer = widget_picker.history_revealer.clone();
        let window = window.clone();
        let registry = registry.clone();
        let interactive = interactive.clone();
        move |_| {
            let open = !revealer.reveals_child();
            revealer.set_reveal_child(open);
            if !open {
                history_revealer.set_reveal_child(false);
            }
            refresh_shape_during_transition(&window, &registry, interactive.clone());
        }
    });

    window.connect_key_press_event({
        let toggle_action = toggle_action.clone();
        let interactive = interactive.clone();
        move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape && interactive.get() {
                toggle_action();
                return glib::Propagation::Stop;
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

    window.show_all();
    set_edit_chrome_visibility(&registry, false);
    widget_picker.revealer.set_reveal_child(false);
    widget_picker.history_revealer.set_reveal_child(false);
    if !system_enabled {
        system_card.0.hide();
    }
    if !timer_enabled {
        timer_card.card.hide();
    }
    if !settings_enabled {
        widget_picker.card.hide();
    }
    window.present();
    window.move_(0, 0);

    glib::idle_add_local_once({
        let window = window.clone();
        let registry = registry.clone();
        let state = state.clone();
        let root = root.clone();
        let screens = screens.clone();
        move || {
            clamp_registered_widgets(&root, &registry, &screens, &state);
            refresh_input_shape(&window, &registry, false);
        }
    });

    let (hotkey_tx, hotkey_rx) = async_channel::unbounded();
    platform::spawn_global_hotkey(hotkey_tx);
    glib::MainContext::default().spawn_local({
        let toggle_action = toggle_action.clone();
        let toggle_settings_action = toggle_settings_action.clone();
        async move {
            while let Ok(action) = hotkey_rx.recv().await {
                match action {
                    platform::HotkeyAction::ToggleInteraction => toggle_action(),
                    platform::HotkeyAction::ToggleSettings => toggle_settings_action(),
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
    glib::source::unix_signal_add_local(libc::SIGUSR2, {
        let toggle_settings_action = toggle_settings_action.clone();
        move || {
            toggle_settings_action();
            glib::ControlFlow::Continue
        }
    });

    start_system_updates(system_card.3, system_card.4);
    start_timer_updates(timer_card, state, window, registry, interactive);
}

fn build_system_card(initial_color_mode: ColorMode) -> BuiltSystemCard {
    let (card, body, _, color_mode, resize) = card_shell("", "", initial_color_mode);
    let values = Rc::new(Cell::new((0.0, 0.0, 0.0, 0.0, 0.0)));
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
        move |area, ctx| {
            draw_system(area, ctx, values.get(), color_mode.get());
            glib::Propagation::Proceed
        }
    });
    (card, drag, color_mode, canvas, values, resize)
}

fn start_system_updates(canvas: gtk::DrawingArea, values: SystemValues) {
    let reader = Rc::new(RefCell::new(SystemReader::default()));
    let update: Rc<dyn Fn()> = Rc::new({
        let reader = reader.clone();
        let canvas = canvas.clone();
        let values = values.clone();
        move || {
            let snapshot = reader.borrow_mut().read();
            values.set((
                snapshot.cpu_percent,
                snapshot.memory_percent,
                snapshot.memory_used_gib,
                snapshot.memory_total_gib,
                snapshot.load_one,
            ));
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
    values: (f64, f64, f64, f64, f64),
    color_mode: ColorMode,
) {
    let (cpu, memory, _used, _total, _load) = values;
    let allocation = area.allocation();
    let scale = (f64::from(allocation.width()) / 216.0)
        .min(f64::from(allocation.height()) / 82.0)
        .max(0.1);
    let content_width = 216.0 * scale;
    let content_height = 82.0 * scale;
    let _ = ctx.save();
    ctx.translate(
        (f64::from(allocation.width()) - content_width) / 2.0,
        (f64::from(allocation.height()) - content_height) / 2.0,
    );
    ctx.scale(scale, scale);
    let (ink, muted, accent) = match color_mode {
        ColorMode::Light => ((0.97, 0.97, 0.97), (0.72, 0.72, 0.72), (0.9, 0.9, 0.9)),
        ColorMode::Gray => ((0.7, 0.7, 0.7), (0.5, 0.5, 0.5), (0.64, 0.64, 0.64)),
        ColorMode::Dark => ((0.08, 0.08, 0.08), (0.24, 0.24, 0.24), (0.14, 0.14, 0.14)),
    };
    for (x, value, title) in [(52.0, cpu, "CPU"), (164.0, memory, "RAM")] {
        ctx.set_line_width(7.0);
        ctx.set_line_cap(cairo::LineCap::Round);
        ctx.set_source_rgba(muted.0, muted.1, muted.2, 0.22);
        ctx.new_sub_path();
        ctx.arc(x, 42.0, 30.0, -PI * 0.75, PI * 0.75);
        let _ = ctx.stroke();
        ctx.set_source_rgba(accent.0, accent.1, accent.2, 0.96);
        ctx.new_sub_path();
        ctx.arc(
            x,
            42.0,
            30.0,
            -PI * 0.75,
            -PI * 0.75 + PI * 1.5 * (value / 100.0).clamp(0.0, 1.0),
        );
        let _ = ctx.stroke();
        center_text(
            ctx,
            x,
            40.0,
            &format!("{value:.0}%"),
            18.0,
            FontWeight::Bold,
            ink,
        );
        center_text(ctx, x, 60.0, title, 8.5, FontWeight::Bold, muted);
    }
    let _ = ctx.restore();
}

struct TimerCard {
    card: gtk::EventBox,
    drag: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    canvas: gtk::DrawingArea,
    stack: gtk::Stack,
    label: gtk::Label,
    action: gtk::Label,
    runtime: Rc<RefCell<TimerRuntime>>,
    alarm: Rc<Cell<bool>>,
    hovered: Rc<Cell<bool>>,
    commit_edit: Rc<dyn Fn()>,
    resize: ResizeHandle,
    wake_updates: CallbackSlot,
}

fn build_timer_card(
    state: Rc<RefCell<AppState>>,
    interactive: Rc<Cell<bool>>,
    initial_color_mode: ColorMode,
) -> TimerCard {
    let (card, body, drag, color_mode, resize) = card_shell("", "", initial_color_mode);
    card.style_context().add_class("timer-card");
    card.connect_size_allocate(|widget, allocation| {
        let context = widget.style_context();
        context.remove_class("timer-size-medium");
        context.remove_class("timer-size-large");
        let diameter = allocation.width().min(allocation.height());
        if diameter >= 245 {
            context.add_class("timer-size-large");
        } else if diameter >= 180 {
            context.add_class("timer-size-medium");
        }
    });
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
    stack.set_halign(gtk::Align::Center);
    stack.set_valign(gtk::Align::Center);

    let label = gtk::Label::new(Some(&format_duration(duration)));
    label.style_context().add_class("timer-value");
    let action = gtk::Label::new(Some("START"));
    action.style_context().add_class("timer-action");
    let editor = gtk::Entry::new();
    editor.set_width_chars(8);
    editor.set_max_length(8);
    editor.set_alignment(0.5);
    editor.style_context().add_class("timer-editor");
    stack.add_named(&label, "time");
    stack.add_named(&action, "action");
    stack.add_named(&editor, "editor");
    stack.set_visible_child_name("time");
    interaction.add(&stack);
    overlay.add_overlay(&interaction);
    body.pack_start(&overlay, true, true, 0);

    canvas.connect_draw({
        let runtime = runtime.clone();
        let color_mode = color_mode.clone();
        move |area, ctx| {
            draw_timer_ring(area, ctx, &runtime.borrow(), color_mode.get());
            glib::Propagation::Proceed
        }
    });

    let editing = Rc::new(Cell::new(false));
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
            interaction.set_above_child(true);
            stack.set_visible_child_name("time");
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
        let commit_edit = commit_edit.clone();
        move |_, _| {
            commit_edit();
            glib::Propagation::Proceed
        }
    });
    editor.connect_key_press_event({
        let editing = editing.clone();
        let stack = stack.clone();
        let interaction = interaction.clone();
        move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                editing.set(false);
                interaction.set_above_child(true);
                stack.set_visible_child_name("time");
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    interaction.connect_enter_notify_event({
        let interactive = interactive.clone();
        let hovered = hovered.clone();
        let runtime = runtime.clone();
        let action = action.clone();
        let stack = stack.clone();
        let editing = editing.clone();
        move |_, _| {
            hovered.set(true);
            if !interactive.get() && !editing.get() {
                action.set_text(timer_action_text(&runtime.borrow()));
                stack.set_visible_child_name("action");
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_leave_notify_event({
        let hovered = hovered.clone();
        let stack = stack.clone();
        let editing = editing.clone();
        move |_, _| {
            hovered.set(false);
            if !editing.get() {
                stack.set_visible_child_name("time");
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_motion_notify_event({
        let interactive = interactive.clone();
        let hovered = hovered.clone();
        let runtime = runtime.clone();
        let action = action.clone();
        let stack = stack.clone();
        let editing = editing.clone();
        move |_, _| {
            hovered.set(true);
            if !interactive.get() && !editing.get() {
                action.set_text(timer_action_text(&runtime.borrow()));
                stack.set_visible_child_name("action");
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_button_press_event({
        let interactive = interactive.clone();
        let runtime = runtime.clone();
        let editor = editor.clone();
        let stack = stack.clone();
        let editing = editing.clone();
        let interaction = interaction.clone();
        move |widget, event| {
            if interactive.get()
                && event.button() == 1
                && event.event_type() == gdk::EventType::DoubleButtonPress
            {
                let (x, y) = event.position();
                let allocation = widget.allocation();
                let center_x = f64::from(allocation.width()) / 2.0;
                let center_y = f64::from(allocation.height()) / 2.0;
                let radius = f64::from(allocation.width().min(allocation.height())) * 0.36;
                let dx = x - center_x;
                let dy = y - center_y;
                if dx * dx + dy * dy <= radius * radius {
                    editing.set(true);
                    editor.set_text(&format_duration(runtime.borrow().duration_seconds));
                    stack.set_visible_child_name("editor");
                    interaction.set_above_child(false);
                    editor.grab_focus();
                    editor.select_region(0, -1);
                    return glib::Propagation::Stop;
                }
            }
            glib::Propagation::Proceed
        }
    });
    interaction.connect_button_release_event({
        let interactive = interactive.clone();
        let runtime = runtime.clone();
        let alarm = alarm.clone();
        let label = label.clone();
        let action = action.clone();
        let canvas = canvas.clone();
        let card = card.clone();
        let wake_updates = wake_updates.clone();
        move |_, event| {
            if interactive.get() || event.button() != 1 {
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

    TimerCard {
        card,
        drag,
        color_mode,
        canvas,
        stack,
        label,
        action,
        runtime,
        alarm,
        hovered,
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

fn draw_timer_ring(
    area: &gtk::DrawingArea,
    ctx: &Context,
    timer: &TimerRuntime,
    color_mode: ColorMode,
) {
    let allocation = area.allocation();
    let cx = f64::from(allocation.width()) / 2.0;
    let cy = f64::from(allocation.height()) / 2.0;
    let radius = cx.min(cy) - 13.0;
    let gray = match color_mode {
        ColorMode::Light => 0.91,
        ColorMode::Gray => 0.6,
        ColorMode::Dark => 0.12,
    };
    let ratio =
        (timer.remaining.as_secs_f64() / timer.duration_seconds.max(1) as f64).clamp(0.0, 1.0);
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
                        card.style_context().add_class("alarm");
                    }
                }
                let keep_running = timer.target.is_some() || timer.alarm;
                if keep_running {
                    canvas.queue_draw();
                }
                if hovered.get() && !interactive.get() {
                    action.set_text(timer_action_text(&timer));
                    stack.set_visible_child_name("action");
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
) {
    for child in list.children() {
        list.remove(&child);
    }
    let notes = state.borrow().notes.clone();
    for note in notes.iter().rev().take(8) {
        let preview = note.text.lines().next().unwrap_or("");
        let preview = if preview.trim().is_empty() {
            "Untitled note".into()
        } else {
            truncate_chars(preview, 27)
        };
        let row = draggable_note_preview(
            &format!("{}  {preview}", if note.pinned { "◆" } else { "◇" }),
            note.id,
            root,
            state.clone(),
            refresh.clone(),
        );
        list.pack_start(&row, false, false, 0);
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
            let (x, y) = event.root();
            if ghost.borrow().is_none() && ((x - sx).abs() > 5.0 || (y - sy).abs() > 5.0) {
                let floating = gtk::Label::new(Some(&text));
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
            let (x, y) = event.root();
            let desired = if let Some(floating) = floating {
                root.remove(&floating);
                Point {
                    x: (x as i32 - 115).max(0),
                    y: (y as i32 - 55).max(0),
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
                note.pinned = true;
                note.position = point;
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
        editor.set_wrap_mode(gtk::WrapMode::Word);
        editor.set_size_request(1, 1);
        editor.set_hexpand(true);
        editor.set_vexpand(true);
        editor.style_context().add_class("pinned-editor");
        editor.buffer().expect("note buffer").set_text(&note.text);
        let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Never);
        scroller.set_shadow_type(gtk::ShadowType::None);
        scroller.set_overlay_scrolling(false);
        scroller.set_size_request(1, 1);
        scroller.set_hexpand(true);
        scroller.set_vexpand(true);
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
        }
        attach_color_mode_menu(
            &card,
            key.clone(),
            state.clone(),
            registry.clone(),
            interactive.clone(),
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
                min_width: 150,
                min_height: 92,
                max_width: 540,
                max_height: 440,
                aspect_ratio: None,
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

struct WidgetPicker {
    card: gtk::EventBox,
    drag: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    plus: gtk::Button,
    revealer: gtk::Revealer,
    system: gtk::ToggleButton,
    timer: gtk::ToggleButton,
    mode: gtk::Button,
    history: gtk::Button,
    history_revealer: gtk::Revealer,
    history_list: gtk::Box,
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
    plus.set_tooltip_text(Some("Settings · Ctrl+Alt+G to hide"));
    plus.set_can_focus(false);
    plus.style_context().add_class("slot-plus");
    slot.pack_start(&plus, false, false, 0);
    top.pack_start(&slot, false, false, 0);

    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideRight);
    revealer.set_transition_duration(190);
    let choices = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    choices.style_context().add_class("widget-choices");
    let system = picker_toggle("CPU / RAM");
    let timer = picker_toggle("TIMER");
    let mode = picker_button(initial_color_mode.label());
    let history = picker_button("▤  HISTORY");
    let new_note = picker_button("＋  NOTE");
    let quit = picker_button("×  QUIT");
    choices.pack_start(&system, false, false, 0);
    choices.pack_start(&timer, false, false, 0);
    choices.pack_start(&mode, false, false, 0);
    choices.pack_start(&new_note, false, false, 0);
    choices.pack_start(&history, false, false, 0);
    choices.pack_start(&quit, false, false, 0);
    revealer.add(&choices);
    top.pack_start(&revealer, false, false, 0);

    let history_revealer = gtk::Revealer::new();
    history_revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    history_revealer.set_transition_duration(210);
    let history_list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    history_list.set_size_request(224, -1);
    history_list.style_context().add_class("history-list");
    history_revealer.add(&history_list);
    content.pack_start(&history_revealer, false, false, 0);

    WidgetPicker {
        drag,
        card,
        color_mode,
        plus,
        revealer,
        system,
        timer,
        mode,
        history,
        history_revealer,
        history_list,
        new_note,
        quit,
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

#[allow(dead_code, clippy::too_many_arguments)]
fn build_mascot(
    root: &gtk::Fixed,
    walk_x: i32,
    walk_y: i32,
    walk_width: i32,
    walk_height: i32,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    alarm: Rc<Cell<bool>>,
) -> gtk::EventBox {
    let pet_widget = gtk::EventBox::new();
    pet_widget.set_visible_window(false);
    pet_widget.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON1_MOTION_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK,
    );
    let area = gtk::DrawingArea::new();
    area.set_size_request(168, 108);
    pet_widget.add(&area);
    let floor_y = (walk_y + walk_height - 126).max(walk_y);
    root.put(&pet_widget, walk_x + 70, floor_y);
    register(
        &registry,
        "mascot",
        &pet_widget,
        Rc::new(Cell::new(ColorMode::Gray)),
    );
    let runtime = Rc::new(RefCell::new(MascotRuntime {
        x: (walk_x + 150) as f64,
        target: (walk_x + 150) as f64,
        target_top: floor_y,
        phase: 0.0,
        pause: 20,
        message: "Hello there!".into(),
        seed: 0x5A17_D00D,
        reaction: 0,
        sequence: PetSequence::Roam,
        sequence_tick: 0,
        target_key: None,
    }));
    let held = Rc::new(Cell::new(false));
    let sheets = Rc::new(load_mascot_sheets());
    pet_widget.connect_button_press_event({
        let runtime = runtime.clone();
        let held = held.clone();
        move |_, event| {
            if event.button() == 1 {
                let mut pet = runtime.borrow_mut();
                held.set(true);
                pet.sequence = PetSequence::Roam;
                pet.sequence_tick = 0;
                pet.reaction = 42;
                pet.pause = 42;
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    pet_widget.connect_motion_notify_event({
        let held = held.clone();
        let runtime = runtime.clone();
        let root = root.clone();
        let pet_widget = pet_widget.clone();
        move |_, event| {
            if !held.get() {
                return glib::Propagation::Proceed;
            }
            let (x, y) = event.root();
            let next_x = (x as i32 - 84).clamp(walk_x, (walk_x + walk_width - 168).max(walk_x));
            let next_y = (y as i32 - 48).clamp(walk_y, (walk_y + walk_height - 108).max(walk_y));
            runtime.borrow_mut().x = (next_x + 80) as f64;
            root.move_(&pet_widget, next_x, next_y);
            root.queue_draw_area(next_x, next_y, 168, 108);
            glib::Propagation::Stop
        }
    });
    pet_widget.connect_button_release_event({
        let held = held.clone();
        let runtime = runtime.clone();
        move |_, _| {
            held.set(false);
            let mut pet = runtime.borrow_mut();
            pet.target = pet.x;
            pet.reaction = 48;
            pet.pause = 28;
            glib::Propagation::Stop
        }
    });
    area.connect_draw({
        let runtime = runtime.clone();
        let alarm = alarm.clone();
        let held = held.clone();
        let sheets = sheets.clone();
        move |_, ctx| {
            draw_sprite_mascot(ctx, &runtime.borrow(), alarm.get(), held.get(), &sheets);
            glib::Propagation::Proceed
        }
    });

    glib::timeout_add_local(Duration::from_millis(66), {
        let area = area.clone();
        let pet_widget = pet_widget.clone();
        let root = root.clone();
        let held = held.clone();
        let state = state.clone();
        move || {
            if !pet_widget.is_visible() {
                return glib::ControlFlow::Continue;
            }
            let mut pet = runtime.borrow_mut();
            pet.phase += if alarm.get() { 0.58 } else { 0.17 };
            pet.reaction = pet.reaction.saturating_sub(1);
            let mut thrown_key = None;
            if held.get() {
                pet.sequence_tick = pet.sequence_tick.wrapping_add(1);
            } else if alarm.get() {
                pet.pause = 20;
            } else if pet.sequence != PetSequence::Roam {
                pet.sequence_tick += 1;
                if pet.sequence_tick >= 64 {
                    if pet.sequence == PetSequence::Throw {
                        thrown_key = pet.target_key.clone();
                    }
                    pet.sequence = PetSequence::Roam;
                    pet.sequence_tick = 0;
                    pet.target_key = None;
                    pet.pause = 22;
                }
            } else if pet.pause > 0 {
                pet.pause -= 1;
            } else {
                let distance = pet.target - pet.x;
                if distance.abs() > 8.0 {
                    pet.x += distance.signum() * 2.4;
                } else if pet.target_key.is_some() {
                    pet.sequence = if pet.seed.is_multiple_of(5) {
                        PetSequence::Throw
                    } else {
                        PetSequence::Climb
                    };
                    pet.sequence_tick = 0;
                } else {
                    pet.seed = pet.seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let items: Vec<RegisteredWidget> = registry
                        .borrow()
                        .iter()
                        .filter(|item| {
                            item.key != "mascot"
                                && item.key != "settings"
                                && item.key != "picker"
                                && item.widget.is_visible()
                        })
                        .cloned()
                        .collect();
                    if !items.is_empty() && (pet.seed & 3) != 0 {
                        let item = &items[(pet.seed as usize) % items.len()];
                        let allocation = item.widget.allocation();
                        pet.target = (allocation.x() + allocation.width() / 2)
                            .clamp(walk_x + 55, walk_x + walk_width - 55)
                            as f64;
                        pet.target_top = allocation.y();
                        pet.target_key = Some(item.key.clone());
                    } else {
                        pet.target = (walk_x + 55) as f64
                            + (pet.seed % (walk_width.saturating_sub(110).max(1) as u64)) as f64;
                        pet.target_top = floor_y;
                    }
                }
            }
            let old = pet_widget.allocation();
            let next_x = (pet.x as i32 - 80).clamp(walk_x, (walk_x + walk_width - 168).max(walk_x));
            let next_y = if pet.sequence == PetSequence::Climb {
                let top = (pet.target_top - 78).clamp(walk_y, floor_y);
                let progress = f64::from(pet.sequence_tick.min(40)) / 40.0;
                (f64::from(floor_y) + f64::from(top - floor_y) * progress) as i32
            } else {
                floor_y
            };
            root.queue_draw_area(old.x() - 3, old.y() - 3, old.width() + 6, old.height() + 6);
            if !held.get() {
                root.move_(&pet_widget, next_x, next_y);
                root.queue_draw_area(next_x - 3, next_y - 3, old.width() + 6, old.height() + 6);
            }
            drop(pet);
            if let Some(key) = thrown_key {
                nudge_thrown_widget(&key, &root, &registry, &state);
            }
            area.queue_draw();
            glib::ControlFlow::Continue
        }
    });

    pet_widget
}

fn load_mascot_sheets() -> MascotSheets {
    MascotSheets {
        basic: load_pixbuf(include_bytes!("../assets/mascot-basic.png")),
        climb: load_pixbuf(include_bytes!("../assets/mascot-climb.png")),
        mischief: load_pixbuf(include_bytes!("../assets/mascot-mischief.png")),
    }
}

fn load_pixbuf(bytes: &'static [u8]) -> gdk_pixbuf::Pixbuf {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(bytes).expect("embedded mascot sprite data");
    loader.close().expect("complete mascot sprite data");
    loader.pixbuf().expect("decoded mascot sprite sheet")
}

fn draw_sprite_mascot(
    ctx: &Context,
    pet: &MascotRuntime,
    alarm: bool,
    held: bool,
    sheets: &MascotSheets,
) {
    let (sheet, frame) = if held {
        (&sheets.mischief, 1 + ((pet.phase * 2.2) as usize % 7))
    } else if alarm {
        (&sheets.mischief, 2 + ((pet.phase * 2.6) as usize % 6))
    } else {
        match pet.sequence {
            PetSequence::Climb => (&sheets.climb, (pet.sequence_tick as usize / 4).min(15)),
            PetSequence::Throw => (
                &sheets.mischief,
                8 + (pet.sequence_tick as usize / 8).min(7),
            ),
            PetSequence::Roam if pet.pause == 0 && (pet.target - pet.x).abs() > 8.0 => {
                (&sheets.basic, 4 + ((pet.phase * 2.0) as usize % 4))
            }
            PetSequence::Roam => (&sheets.basic, (pet.phase as usize / 2) % 4),
        }
    };
    draw_sprite_frame(ctx, sheet, frame, pet.target < pet.x);
}

fn draw_sprite_frame(ctx: &Context, sheet: &gdk_pixbuf::Pixbuf, frame: usize, mirror: bool) {
    let cell = sheet.width() / 4;
    let display_size = 96.0;
    let _ = ctx.save();
    ctx.translate(36.0, 3.0);
    if mirror {
        ctx.translate(display_size, 0.0);
        ctx.scale(-1.0, 1.0);
    }
    ctx.scale(
        display_size / f64::from(cell),
        display_size / f64::from(cell),
    );
    ctx.rectangle(0.0, 0.0, f64::from(cell), f64::from(cell));
    ctx.clip();
    let column = (frame % 4) as i32;
    let row = (frame / 4) as i32;
    ctx.set_source_pixbuf(sheet, f64::from(-column * cell), f64::from(-row * cell));
    let _ = ctx.paint();
    let _ = ctx.restore();
}

fn nudge_thrown_widget(
    key: &str,
    root: &gtk::Fixed,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    state: &Rc<RefCell<AppState>>,
) {
    let Some(item) = registry
        .borrow()
        .iter()
        .find(|item| item.key == key)
        .cloned()
    else {
        return;
    };
    let allocation = item.widget.allocation();
    let root_allocation = root.allocation();
    let screens = logical_screen_rects(
        item.widget.scale_factor(),
        root_allocation.width(),
        root_allocation.height(),
    );
    let point = clamp_to_screens(
        Point {
            x: allocation.x() + 56,
            y: allocation.y() - 18,
        },
        allocation.width(),
        allocation.height(),
        &screens,
    );
    root.queue_draw_area(
        allocation.x() - 3,
        allocation.y() - 3,
        allocation.width() + 6,
        allocation.height() + 6,
    );
    root.move_(&item.widget, point.x, point.y);
    let mut data = state.borrow_mut();
    if let Some(id) = key
        .strip_prefix("note:")
        .and_then(|value| value.parse::<u64>().ok())
    {
        if let Some(note) = data.notes.iter_mut().find(|note| note.id == id) {
            note.position = point;
        }
    } else {
        data.positions.insert(key.into(), point);
    }
    let _ = data.save();
}

#[allow(dead_code)]
fn draw_legacy_mascot(ctx: &Context, pet: &MascotRuntime, alarm: bool) {
    let bounce = if alarm {
        (pet.phase.sin().abs() * 12.0) + 4.0
    } else if pet.reaction > 0 {
        pet.phase.sin().abs() * 7.0
    } else {
        pet.phase.sin().abs() * 2.5
    };
    let _ = ctx.save();
    ctx.translate(110.0, 113.0 - bounce);

    let _ = ctx.save();
    ctx.set_source_rgba(0.03, 0.04, 0.06, 0.20);
    ctx.scale(1.0, 0.35);
    ctx.new_sub_path();
    ctx.arc(0.0, 34.0, 42.0, 0.0, TAU);
    let _ = ctx.fill();
    let _ = ctx.restore();

    let direction = if pet.target >= pet.x { 1.0 } else { -1.0 };
    ctx.scale(direction, 1.0);
    ctx.set_line_width(8.0);
    ctx.set_line_cap(cairo::LineCap::Round);
    ctx.set_source_rgb(0.47, 0.85, 1.0);
    let tail_wag = (pet.phase * 1.7).sin() * 9.0;
    ctx.move_to(-28.0, 6.0);
    ctx.curve_to(
        -58.0,
        -2.0 + tail_wag,
        -49.0,
        -31.0,
        -37.0,
        -23.0 + tail_wag,
    );
    let _ = ctx.stroke();

    ctx.set_source_rgb(0.98, 0.89, 0.75);
    rounded_rect(ctx, -35.0, -17.0, 70.0, 50.0, 22.0);
    let _ = ctx.fill_preserve();
    ctx.set_source_rgb(0.13, 0.14, 0.17);
    ctx.set_line_width(2.8);
    let _ = ctx.stroke();
    ctx.set_source_rgb(1.0, 0.92, 0.80);
    ctx.arc(16.0, -25.0, 31.0, 0.0, TAU);
    let _ = ctx.fill_preserve();
    ctx.set_source_rgb(0.13, 0.14, 0.17);
    ctx.set_line_width(2.8);
    let _ = ctx.stroke();
    ctx.set_source_rgb(1.0, 0.92, 0.80);
    ctx.move_to(-7.0, -45.0);
    ctx.line_to(1.0, -75.0);
    ctx.line_to(17.0, -51.0);
    ctx.close_path();
    let _ = ctx.fill_preserve();
    ctx.set_source_rgb(0.13, 0.14, 0.17);
    ctx.set_line_width(2.8);
    let _ = ctx.stroke();
    ctx.set_source_rgb(1.0, 0.92, 0.80);
    ctx.move_to(27.0, -51.0);
    ctx.line_to(46.0, -70.0);
    ctx.line_to(44.0, -36.0);
    ctx.close_path();
    let _ = ctx.fill_preserve();
    ctx.set_source_rgb(0.13, 0.14, 0.17);
    let _ = ctx.stroke();

    ctx.set_source_rgb(1.0, 0.55, 0.62);
    ctx.move_to(0.0, -56.0);
    ctx.line_to(3.0, -68.0);
    ctx.line_to(10.0, -55.0);
    ctx.close_path();
    let _ = ctx.fill();
    ctx.move_to(34.0, -54.0);
    ctx.line_to(43.0, -64.0);
    ctx.line_to(42.0, -48.0);
    ctx.close_path();
    let _ = ctx.fill();

    ctx.set_source_rgb(0.12, 0.14, 0.18);
    if pet.phase.rem_euclid(10.0) > 9.25 {
        ctx.set_line_width(2.4);
        for eye_x in [7.0, 27.0] {
            ctx.move_to(eye_x - 3.5, -27.0);
            ctx.curve_to(eye_x - 1.0, -24.5, eye_x + 1.0, -24.5, eye_x + 3.5, -27.0);
            let _ = ctx.stroke();
        }
    } else {
        for eye_x in [7.0, 27.0] {
            ctx.arc(eye_x, -27.0, 3.7, 0.0, TAU);
            let _ = ctx.fill();
        }
    }
    ctx.set_source_rgb(1.0, 0.45, 0.56);
    for cheek_x in [-1.0, 36.0] {
        ctx.arc(cheek_x, -17.0, 4.0, 0.0, TAU);
        let _ = ctx.fill();
    }
    ctx.set_source_rgb(0.13, 0.14, 0.17);
    ctx.set_line_width(2.0);
    ctx.move_to(15.0, -17.0);
    ctx.curve_to(18.0, -13.0, 22.0, -13.0, 25.0, -17.0);
    let _ = ctx.stroke();

    ctx.set_line_width(8.0);
    ctx.set_source_rgb(0.98, 0.89, 0.75);
    let step = pet.phase.sin() * 7.0;
    ctx.move_to(-17.0, 27.0);
    ctx.line_to(-19.0 + step, 39.0);
    ctx.move_to(18.0, 27.0);
    ctx.line_to(19.0 - step, 39.0);
    let _ = ctx.stroke();
    let _ = ctx.restore();

    if pet.reaction > 0 {
        let lift = (42 - pet.reaction) as f64 * 0.65;
        for (x, delay) in [(69.0, 0.0), (151.0, 8.0), (179.0, 17.0)] {
            let local = (lift - delay).max(0.0);
            if local > 0.0 {
                draw_heart(ctx, x, 82.0 - local, (1.0 - local / 42.0).max(0.15));
            }
        }
    }

    if pet.pause > 0 || alarm {
        ctx.set_source_rgba(0.05, 0.07, 0.10, 0.88);
        rounded_rect(ctx, 44.0, 5.0, 148.0, 32.0, 12.0);
        let _ = ctx.fill();
        center_text(
            ctx,
            118.0,
            25.0,
            &pet.message,
            10.5,
            FontWeight::Bold,
            (0.96, 0.98, 1.0),
        );
    }
    if alarm {
        ctx.set_source_rgb(1.0, 0.42, 0.50);
        for x in [48.0, 176.0] {
            ctx.move_to(x, 52.0);
            ctx.line_to(x + 8.0, 41.0);
            ctx.line_to(x + 6.0, 56.0);
            let _ = ctx.stroke();
        }
    }
}

#[allow(dead_code)]
fn draw_heart(ctx: &Context, x: f64, y: f64, alpha: f64) {
    let _ = ctx.save();
    ctx.translate(x, y);
    ctx.scale(0.65, 0.65);
    ctx.set_source_rgba(1.0, 0.34, 0.48, alpha);
    ctx.move_to(0.0, 5.0);
    ctx.curve_to(-18.0, -7.0, -16.0, -20.0, -6.0, -20.0);
    ctx.curve_to(0.0, -20.0, 4.0, -15.0, 5.0, -11.0);
    ctx.curve_to(8.0, -18.0, 22.0, -20.0, 24.0, -8.0);
    ctx.curve_to(25.0, 0.0, 15.0, 8.0, 2.0, 18.0);
    ctx.close_path();
    let _ = ctx.fill();
    let _ = ctx.restore();
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
    }
}

fn attach_color_mode_menu(
    widget: &gtk::EventBox,
    key: String,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
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

fn place_card(root: &gtk::Fixed, card: &gtk::EventBox, point: Point) {
    root.put(card, point.x.max(0), point.y.max(0));
}

fn logical_screen_rects(_scale: i32, fallback_width: i32, fallback_height: i32) -> Vec<ScreenRect> {
    let Some(display) = gdk::Display::default() else {
        return vec![ScreenRect {
            x: 0,
            y: 0,
            width: fallback_width,
            height: fallback_height,
        }];
    };
    let mut screens = Vec::new();
    for index in 0..display.n_monitors() {
        if let Some(monitor) = display.monitor(index) {
            let geometry = monitor.workarea();
            screens.push(ScreenRect {
                x: geometry.x(),
                y: geometry.y(),
                width: geometry.width(),
                height: geometry.height(),
            });
        }
    }
    if screens.is_empty() {
        screens.push(ScreenRect {
            x: 0,
            y: 0,
            width: fallback_width,
            height: fallback_height,
        });
    }
    screens
}

fn logical_primary_screen(_scale: i32) -> Option<ScreenRect> {
    let monitor = gdk::Display::default()?.primary_monitor()?;
    let geometry = monitor.workarea();
    Some(ScreenRect {
        x: geometry.x(),
        y: geometry.y(),
        width: geometry.width(),
        height: geometry.height(),
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

fn clamp_registered_widgets(
    root: &gtk::Fixed,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    screens: &[ScreenRect],
    state: &Rc<RefCell<AppState>>,
) {
    let mut data = state.borrow_mut();
    for item in registry.borrow().iter() {
        if item.key == "mascot" || item.key == "settings" {
            continue;
        }
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

            let next = if let Some(ratio) = bounds.aspect_ratio {
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

fn refresh_input_shape(
    window: &gtk::ApplicationWindow,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: bool,
) {
    let Some(gdk_window) = window.window() else {
        return;
    };
    let region = Region::create();
    for item in registry.borrow().iter() {
        let lock_timer = item.key == "timer";
        if interactive || lock_timer || item.widget.style_context().has_class("alarm") {
            if !item.widget.is_visible() || !item.widget.is_mapped() {
                continue;
            }
            let allocation = item.widget.allocation();
            if allocation.width() > 1 && allocation.height() > 1 {
                if !interactive && lock_timer {
                    union_circle_region(&region, &allocation);
                } else {
                    let _ = region.union_rectangle(&RectangleInt::new(
                        allocation.x(),
                        allocation.y(),
                        allocation.width(),
                        allocation.height(),
                    ));
                }
            }
        }
    }
    gdk_window.input_shape_combine_region(&region, 0, 0);
}

fn union_circle_region(region: &Region, allocation: &gtk::Allocation) {
    let radius = (allocation.width().min(allocation.height()) / 2 - 10).clamp(1, 62);
    let cx = allocation.x() + allocation.width() / 2;
    let cy = allocation.y() + allocation.height() / 2;
    for dy in (-radius..=radius).step_by(2) {
        let half = ((radius * radius - dy * dy) as f64).sqrt() as i32;
        let _ = region.union_rectangle(&RectangleInt::new(cx - half, cy + dy, half * 2 + 1, 2));
    }
}

fn refresh_shape_during_transition(
    window: &gtk::ApplicationWindow,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
) {
    let ticks = Rc::new(Cell::new(0_u8));
    glib::timeout_add_local(Duration::from_millis(28), {
        let window = window.clone();
        let registry = registry.clone();
        move || {
            refresh_input_shape(&window, &registry, interactive.get());
            let next = ticks.get() + 1;
            ticks.set(next);
            if next < 11 {
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        }
    });
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
    item.color_mode.set(mode);
    let context = item.widget.style_context();
    context.remove_class("mode-light");
    context.remove_class("mode-gray");
    context.remove_class("mode-dark");
    context.add_class(mode.css_class());
    item.widget.queue_draw();
}

fn small_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.style_context().add_class("tiny-button");
    button.set_can_focus(false);
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
    if let Ok(extents) = ctx.text_extents(text) {
        ctx.move_to(x - (extents.width() / 2.0 + extents.x_bearing()), y);
    }
    ctx.set_source_rgb(color.0, color.1, color.2);
    let _ = ctx.show_text(text);
}

fn rounded_rect(ctx: &Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    ctx.new_sub_path();
    ctx.arc(x + width - radius, y + radius, radius, -PI / 2.0, 0.0);
    ctx.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        PI / 2.0,
    );
    ctx.arc(x + radius, y + height - radius, radius, PI / 2.0, PI);
    ctx.arc(x + radius, y + radius, radius, PI, PI * 1.5);
    ctx.close_path();
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
    use super::parse_timer_input;

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
}
