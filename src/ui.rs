use crate::{
    platform,
    translate,
    state::{
        AppState, ColorMode, Note, NoteImage, Point, Size, SystemDetails, TimerStyle,
        IMAGE_PLACEHOLDER,
    },
    system::{SystemReader, SystemSnapshot},
};
use cairo::{Context, FontSlant, FontWeight, RectangleInt, Region};
use gdk::prelude::*;
use gdk_pixbuf::{InterpType, Pixbuf};
use gtk::prelude::*;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
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
const NOTE_MAX_WIDTH: i32 = 540;
const NOTE_MAX_HEIGHT: i32 = 440;
const HISTORY_WIDTH: i32 = 236;
const HISTORY_HEIGHT: i32 = 252;
// One rendered row (.note-preview padding + the inherited note font) plus the
// list spacing, and the header + list padding above it. Used to scale how many
// rows the window renders to how tall the user dragged it.
const HISTORY_ROW_HEIGHT: i32 = 30;
const HISTORY_CHROME_HEIGHT: i32 = 32;
const TRANSLATE_WIDTH: i32 = 272;
const TRANSLATE_HEIGHT: i32 = 320;
// Long enough that a completion list does not chase the caret on every
// keystroke, short enough that it still feels like type-ahead.
const TRANSLATE_SUGGEST_DELAY: Duration = Duration::from_millis(280);
// The width a result line asks for, in characters. Comfortably narrower than
// the window's minimum, so the answer never widens the card on its own.
const TRANSLATE_WRAP_CHARS: i32 = 16;
// The search box starts one line tall and grows with the text to about four,
// after which it scrolls rather than eating the results below it.
const TRANSLATE_INPUT_MIN_HEIGHT: i32 = 22;
const TRANSLATE_INPUT_MAX_HEIGHT: i32 = 92;
// How many past queries the window offers back.
const TRANSLATE_RECENT_LIMIT: usize = 10;
const RESIZE_HIT_SIZE: i32 = 18;

type CallbackSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
// The dictionary lookup, handed to context menus that are built before it
// exists. Filled in once during startup, like `CallbackSlot`.
type LookupSlot = Rc<RefCell<Option<Rc<dyn Fn(&str)>>>>;
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
    // Image files left behind by a note deleted while Sysi was not running, or
    // by an image backspaced out of a note, are reclaimed once per launch.
    state.borrow().prune_orphan_images();
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
    // Context menus are attached while their windows are built, well before the
    // dictionary lookup they call into exists; the slot is filled once both do.
    let lookup_slot: LookupSlot = Rc::new(RefCell::new(None));
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
        None,
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
        Some(lookup_slot.clone()),
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

    // Shared rather than moved piecemeal into each closure: the query panel,
    // the completions and the results all have to be reachable together.
    let translate = Rc::new(build_translate_window(saved_color_mode(
        &state.borrow(),
        "translate",
    )));
    let translate_position = state
        .borrow()
        .positions
        .get("translate")
        .copied()
        .unwrap_or(Point {
            x: primary_screen.x + 292,
            y: primary_screen.y + 186,
        });
    apply_widget_size(
        &translate.card,
        "translate",
        &state,
        Size {
            width: TRANSLATE_WIDTH,
            height: TRANSLATE_HEIGHT,
        },
    );
    place_card(&root, &translate.card, translate_position);
    register(
        &registry,
        "translate",
        &translate.card,
        translate.color_mode.clone(),
    );
    if let Some(item) = registry
        .borrow_mut()
        .iter_mut()
        .find(|item| item.key == "translate")
    {
        item.edit_only = Some(translate.chrome.clone());
    }
    attach_color_mode_menu(
        &translate.card,
        "translate".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        None,
        None,
        Some(lookup_slot.clone()),
    );
    attach_drag(
        &translate.header,
        &translate.card,
        &root,
        "translate".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
    );
    attach_resize(
        &translate.resize,
        &translate.card,
        &root,
        "translate".into(),
        state.clone(),
        registry.clone(),
        interactive.clone(),
        window.clone(),
        ResizeBounds {
            min_width: 196,
            min_height: 120,
            max_width: 680,
            max_height: 860,
            aspect_ratio: None,
            preserve_current_aspect: false,
        },
    );

    // Lookups and completions run on worker threads and report back here. One
    // counter numbers every request so a reply that a newer keystroke has
    // already superseded can be dropped instead of overwriting the answer the
    // user is reading.
    let (translate_tx, translate_rx) = async_channel::unbounded::<translate::TranslateEvent>();
    // Two counters, not one: a lookup can be in flight for seconds while the
    // user types the next query, and a shared counter would let those
    // keystrokes retire the lookup — leaving "Looking it up…" on screen with
    // nothing left to replace it.
    let translate_lookup_generation = Rc::new(Cell::new(0u64));
    let translate_suggest_generation = Rc::new(Cell::new(0u64));
    // The play buttons currently on screen, keyed by the clip they are waiting
    // for. Cleared on every rebuild, so a download that outlives its button
    // finds no one waiting and is neither re-enabled nor played.
    let translate_audio: Rc<RefCell<HashMap<String, gtk::Button>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let translate_pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    // Whether the query panel is dropped down. Tracked explicitly because
    // show_all() on the card would otherwise reveal it along with everything
    // else, exactly like the history window's search mode.
    let translate_search_open = Rc::new(Cell::new(false));
    // The recents start folded away behind their arrow: opening the panel is a
    // move to type something, not to read the last ten things typed.
    let translate_recents_open = Rc::new(Cell::new(false));

    // Run a query: used by Enter, by the completion and recent rows, by the
    // "did you mean" chips, and by the right-click lookup.
    let translate_lookup: Rc<dyn Fn(&str)> = {
        let translate = translate.clone();
        let search_open = translate_search_open.clone();
        let lookup_generation = translate_lookup_generation.clone();
        let suggest_generation = translate_suggest_generation.clone();
        let audio_buttons = translate_audio.clone();
        let pending = translate_pending.clone();
        let tx = translate_tx.clone();
        Rc::new(move |query: &str| {
            let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
            if query.is_empty() {
                return;
            }
            // A completion that is still in flight would land on top of the
            // answer; cancel its timer and retire its generation.
            if let Some(source) = pending.borrow_mut().take() {
                source.remove();
            }
            suggest_generation.set(suggest_generation.get() + 1);
            // The answer is what the user wants to see now, so the panel gets
            // out of the way until they ask for it again.
            search_open.set(false);
            translate.set_search_visible(false);
            clear_children(&translate.suggestions);
            audio_buttons.borrow_mut().clear();
            lookup_generation.set(lookup_generation.get() + 1);
            clear_children(&translate.results);
            translate.results.pack_start(
                &translate_line("Looking it up\u{2026}", "translate-status"),
                false,
                false,
                0,
            );
            translate.results.show_all();
            translate::spawn_lookup(query, lookup_generation.get(), tx.clone());
        })
    };

    // Opening and closing the query panel, including the focus grab that lets
    // the user start typing the moment it appears.
    let set_translate_search: Rc<dyn Fn(bool)> = {
        let translate = translate.clone();
        let search_open = translate_search_open.clone();
        let state = state.clone();
        let window = window.clone();
        let lookup = translate_lookup.clone();
        let suggest_generation = translate_suggest_generation.clone();
        let pending = translate_pending.clone();
        let recents_open = translate_recents_open.clone();
        Rc::new(move |open: bool| {
            search_open.set(open);
            translate.set_search_visible(open);
            // Whichever way the panel moves, a completion request that has not
            // fired yet is no longer wanted.
            if let Some(source) = pending.borrow_mut().take() {
                source.remove();
            }
            suggest_generation.set(suggest_generation.get() + 1);
            if !open {
                clear_children(&translate.suggestions);
                return;
            }
            // Start empty rather than pre-selecting the last query: selecting
            // text in a GtkTextView hands it the X11 primary selection, which
            // would clobber whatever the user had highlighted elsewhere — the
            // very thing the "LOOK UP" menu item reads.
            translate.set_query("");
            render_translate_recents(&translate.suggestions, &state, &lookup, &recents_open);
            glib::idle_add_local_once({
                let window = window.clone();
                let input = translate.input.clone();
                let search_open = search_open.clone();
                move || {
                    // The panel can be closed again before this runs, e.g. when
                    // a right-click lookup opens the window and immediately
                    // shows a result.
                    if !search_open.get() {
                        return;
                    }
                    present_overlay(&window);
                    input.grab_focus();
                }
            });
        })
    };

    translate.open_search.connect_clicked({
        let set_translate_search = set_translate_search.clone();
        move |_| set_translate_search(true)
    });
    translate.close_search.connect_clicked({
        let set_translate_search = set_translate_search.clone();
        move |_| set_translate_search(false)
    });

    // Enter runs the query; Shift+Enter is left alone so a multi-line paste can
    // still be edited by hand.
    translate.input.connect_key_press_event({
        let translate = translate.clone();
        let translate_lookup = translate_lookup.clone();
        move |_, event| {
            let enter = matches!(
                event.keyval(),
                gdk::keys::constants::Return | gdk::keys::constants::KP_Enter
            );
            if enter && !event.state().contains(gdk::ModifierType::SHIFT_MASK) {
                translate_lookup(&translate.query());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    if let Some(buffer) = translate.input.buffer() {
        buffer.connect_changed({
            let translate = translate.clone();
            let state = state.clone();
            let generation = translate_suggest_generation.clone();
            let pending = translate_pending.clone();
            let tx = translate_tx.clone();
            let lookup = translate_lookup.clone();
            let recents_open = translate_recents_open.clone();
            move |_| {
                if let Some(source) = pending.borrow_mut().take() {
                    source.remove();
                }
                let text = translate.query();
                // Prose has no completions worth offering; an empty box goes
                // back to showing what was searched before.
                if text.is_empty() {
                    generation.set(generation.get() + 1);
                    render_translate_recents(&translate.suggestions, &state, &lookup, &recents_open);
                    return;
                }
                clear_children(&translate.suggestions);
                if translate::is_sentence(&text) {
                    generation.set(generation.get() + 1);
                    return;
                }
                let generation_for_timer = generation.clone();
                let pending_for_timer = pending.clone();
                let tx = tx.clone();
                let source = glib::timeout_add_local_once(TRANSLATE_SUGGEST_DELAY, move || {
                    pending_for_timer.borrow_mut().take();
                    generation_for_timer.set(generation_for_timer.get() + 1);
                    translate::spawn_suggest(text, generation_for_timer.get(), tx);
                });
                *pending.borrow_mut() = Some(source);
            }
        });
    }

    glib::MainContext::default().spawn_local({
        let suggestions = translate.suggestions.clone();
        let results = translate.results.clone();
        let lookup_generation = translate_lookup_generation.clone();
        let suggest_generation = translate_suggest_generation.clone();
        let audio_buttons = translate_audio.clone();
        let lookup = translate_lookup.clone();
        let state = state.clone();
        let tx = translate_tx.clone();
        async move {
            while let Ok(event) = translate_rx.recv().await {
                match event {
                    translate::TranslateEvent::Suggestions { generation: at, items }
                        if at == suggest_generation.get() =>
                    {
                        render_translate_suggestions(&suggestions, &items, &lookup);
                    }
                    translate::TranslateEvent::Lookup { generation: at, result }
                        if at == lookup_generation.get() =>
                    {
                        // Remembered here rather than when the query was sent,
                        // so a typo that found nothing never enters the list.
                        if matches!(
                            result.kind,
                            translate::ResultKind::Word(_) | translate::ResultKind::Sentence(_)
                        ) {
                            remember_search(&state, &result.query);
                        }
                        render_translate_result(
                            &results,
                            &result,
                            &audio_buttons,
                            &tx,
                            &lookup,
                        );
                    }
                    translate::TranslateEvent::AudioReady { url, path } => {
                        // Only play for a button that is still on screen: a clip
                        // that finished downloading after the user moved on to
                        // another word would otherwise speak over the new one.
                        if let Some(button) = audio_buttons.borrow_mut().remove(&url) {
                            button.set_sensitive(true);
                            translate::play_audio(&path);
                        }
                    }
                    translate::TranslateEvent::AudioFailed { url } => {
                        if let Some(button) = audio_buttons.borrow_mut().remove(&url) {
                            button.set_sensitive(true);
                            button.set_tooltip_text(Some("Audio unavailable"));
                        }
                    }
                    // A reply for a query the user has already moved on from.
                    _ => {}
                }
            }
        }
    });

    let toggle_translate: Rc<dyn Fn()> = {
        let translate = translate.clone();
        let card = translate.card.clone();
        let chrome = translate.chrome.clone();
        let search_open = translate_search_open.clone();
        let set_translate_search = set_translate_search.clone();
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
                reopen_widget(
                    &card,
                    "translate",
                    &root,
                    &state,
                    &screens,
                    primary_screen,
                    Size {
                        width: TRANSLATE_WIDTH,
                        height: TRANSLATE_HEIGHT,
                    },
                    Some(&picker),
                    &registry,
                );
                card.show_all();
                // show_all() reveals the chrome and the query panel regardless
                // of lock mode; restore both rules.
                chrome.set_visible(interactive.get());
                if interactive.get() {
                    // Opening the window is an intent to look something up, so
                    // the query panel comes down ready to type into.
                    set_translate_search(true);
                } else {
                    search_open.set(false);
                    translate.set_search_visible(false);
                }
            } else {
                card.hide();
            }
            state.borrow_mut().settings.translate_open = open;
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
    translate.hide.connect_clicked({
        let toggle_translate = toggle_translate.clone();
        move |_| toggle_translate()
    });

    // Now that both halves exist, the "LOOK UP" context-menu item can bring the
    // dictionary up and run the query in one go.
    *lookup_slot.borrow_mut() = Some({
        let card = translate.card.clone();
        let toggle_translate = toggle_translate.clone();
        let translate_lookup = translate_lookup.clone();
        let interactive = interactive.clone();
        let lock = widget_picker.lock.clone();
        Rc::new(move |query: &str| {
            if !interactive.get() {
                lock.clicked();
            }
            if !card.is_visible() {
                toggle_translate();
            }
            translate_lookup(query);
        }) as Rc<dyn Fn(&str)>
    });

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
                lookup_slot.clone(),
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
                images: Vec::new(),
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
        let toggle_translate = toggle_translate.clone();
        let translate_card = translate.card.clone();
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
            Some("toggle-translate") => {
                // The entry is edit chrome, so a translate window opened while
                // locked would have nothing to type into; unlock first, the way
                // a new note does.
                if !translate_card.is_visible() && !interactive.get() {
                    lock.clicked();
                }
                toggle_translate();
            }
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
        let translate_card = translate.card.clone();
        let translate_search_open = translate_search_open.clone();
        let set_translate_search = set_translate_search.clone();
        move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                // The overlay sees key events before the focused widget, so
                // Escape while searching must close the search box rather than
                // lock the whole overlay out from under the user.
                if searching.get() && history_card.is_visible() {
                    close_history_search();
                    return glib::Propagation::Stop;
                }
                // Same for the dictionary's query panel: the first Escape puts
                // it away, and only a second one locks the overlay. In lock
                // mode the panel is already hidden as edit chrome, so there is
                // nothing to put away.
                if interactive.get() && translate_card.is_visible() && translate_search_open.get() {
                    set_translate_search(false);
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
    // Likewise the dictionary: show_all() dropped its query panel down, but a
    // window restored from the last session was not asked for just now.
    translate.set_search_visible(false);
    if !state.borrow().settings.translate_open {
        translate.card.hide();
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
        let headline = note_headline(&note.text);
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

// ---------------------------------------------------------------- note images
//
// A pasted image is a real GdkPixbuf inside the note's GtkTextBuffer, so it
// sits in the text flow, wraps with it, and is deleted by Backspace like any
// other character. The buffer keeps a U+FFFC placeholder for it in the saved
// text, the bytes live in their own file, and the rendered size is stored per
// placeholder — that is all it takes to rebuild the note exactly on restart.

// Pasted screenshots are usually far wider than a note, so scale to fit on
// paste. Small images keep their natural size instead of being blown up.
const NOTE_IMAGE_DEFAULT_MAX: i32 = 240;
const NOTE_IMAGE_MIN: i32 = 40;
const NOTE_IMAGE_MAX: i32 = 1400;
// A hover-only corner handle, matching the note card resize affordance.
const NOTE_IMAGE_HANDLE_HIT: i32 = 18;
// A note whose editor has not been allocated yet reports a 1x1 text area, which
// would make its chrome look like the entire card and balloon the note on the
// first paste. Bound the measurement to what a note plausibly spends on its
// header, padding and scrollbar.
const NOTE_IMAGE_CHROME_MAX: i32 = 90;
const NOTE_IMAGE_BORDER_RADIUS: f64 = 6.0;
const NOTE_UNDO_LIMIT: usize = 100;
// Slack under a pasted image so its frame is not flush with the note edge.
const NOTE_IMAGE_ROOM: i32 = 10;
const NOTE_IMAGE_DATA_KEY: &str = "sysi-image-file";

#[derive(Clone, Copy, Debug)]
struct FocusedImage {
    offset: i32,
    width: i32,
    height: i32,
}

#[derive(Clone, Debug)]
struct ImageResize {
    offset: i32,
    start_x: f64,
    start_y: f64,
    start_width: i32,
    aspect: f64,
    before: NoteSnapshot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct NoteSnapshot {
    text: String,
    images: Vec<NoteImage>,
    cursor: i32,
    size: Size,
}

#[derive(Default)]
struct NoteUndo {
    undo: Vec<NoteSnapshot>,
    redo: Vec<NoteSnapshot>,
    pending: Option<NoteSnapshot>,
    applying: bool,
}

type ImageFocus = Rc<RefCell<Option<FocusedImage>>>;
type ImageResizeState = Rc<RefCell<Option<ImageResize>>>;
type ImageOriginals = Rc<RefCell<HashMap<String, Pixbuf>>>;
type NoteUndoState = Rc<RefCell<NoteUndo>>;

// How wide a pasted image may be drawn: it has to fit the note it lands in —
// an image wider than the note is clipped, taking its resize edge out of reach
// with it — but a big note should not get a wall-sized paste either.
fn note_image_cap(available_width: i32) -> i32 {
    available_width.clamp(NOTE_IMAGE_MIN, NOTE_IMAGE_DEFAULT_MAX)
}

// What the note spends on everything that is not text: its header, the card
// padding around the editor, and the editor's own CSS padding — which lives
// inside the text window, so comparing allocations alone misses it. The extra
// slack leaves room for the note's scrollbar.
fn note_image_chrome(editor: &gtk::TextView, card: &gtk::EventBox) -> Size {
    let allocation = card.allocation();
    let editor_allocation = editor.allocation();
    let padding = editor.style_context().padding(gtk::StateFlags::NORMAL);
    Size {
        width: ((allocation.width() - editor_allocation.width()).max(0)
            + i32::from(padding.left)
            + i32::from(padding.right)
            + NOTE_IMAGE_ROOM)
            .min(NOTE_IMAGE_CHROME_MAX),
        height: ((allocation.height() - editor_allocation.height()).max(0)
            + i32::from(padding.top)
            + i32::from(padding.bottom))
        .min(NOTE_IMAGE_CHROME_MAX),
    }
}

// How far a note may grow to fit a pasted image: never past what a resize drag
// allows, and never past the edge of the monitor it sits on, so growing the
// note cannot push its own resize handle off-screen.
fn note_growth_limit(card: &gtk::EventBox) -> Size {
    let allocation = card.allocation();
    let root = card
        .parent()
        .map(|parent| parent.allocation())
        .unwrap_or_else(|| card.allocation());
    let screens = logical_screen_rects(card.scale_factor(), root.width(), root.height());
    let screen = screens.iter().find(|screen| {
        allocation.x() >= screen.x
            && allocation.x() < screen.x + screen.width
            && allocation.y() >= screen.y
            && allocation.y() < screen.y + screen.height
    });
    let (available_width, available_height) = screen
        .map(|screen| {
            (
                screen.x + screen.width - allocation.x(),
                screen.y + screen.height - allocation.y(),
            )
        })
        .unwrap_or((NOTE_MAX_WIDTH, NOTE_MAX_HEIGHT));
    Size {
        width: NOTE_MAX_WIDTH
            .min(available_width)
            .max(allocation.width().max(1)),
        height: NOTE_MAX_HEIGHT
            .min(available_height)
            .max(allocation.height().max(1)),
    }
}

// The size a note needs so a freshly pasted image is fully visible inside it,
// image edge included. A note is only ever grown here, never shrunk.
fn note_size_for_image(current: Size, chrome: Size, image: Size, limit: Size) -> Size {
    Size {
        width: (image.width + chrome.width).clamp(current.width, limit.width.max(current.width)),
        height: (image.height + chrome.height + NOTE_IMAGE_ROOM)
            .clamp(current.height, limit.height.max(current.height)),
    }
}

// Grow the note so the image inside it stays fully visible, edge included, and
// remember the new size the way a resize drag does.
fn grow_note_for_image(
    editor: &gtk::TextView,
    target: &NoteImageTarget,
    state: &Rc<RefCell<AppState>>,
    image: Size,
) {
    let allocation = target.card.allocation();
    let current = Size {
        width: allocation.width(),
        height: allocation.height(),
    };
    let chrome = note_image_chrome(editor, &target.card);
    let limit = note_growth_limit(&target.card);
    let grown = note_size_for_image(current, chrome, image, limit);
    if grown == current {
        return;
    }
    target.card.set_size_request(grown.width, grown.height);
    target.card.queue_resize();
    state.borrow_mut().sizes.insert(target.key.clone(), grown);
    let _ = state.borrow().save();
    let window = target.window.clone();
    let registry = target.registry.clone();
    let interactive = target.interactive.clone();
    glib::idle_add_local_once(move || {
        refresh_input_shape(&window, &registry, interactive.get());
    });
}

fn fit_within_bounds(width: i32, height: i32, max_width: i32, max_height: i32) -> (i32, i32) {
    let width = width.max(1);
    let height = height.max(1);
    let max_width = max_width.max(1);
    let max_height = max_height.max(1);
    if width <= max_width && height <= max_height {
        return (width, height);
    }
    let scale =
        (f64::from(max_width) / f64::from(width)).min(f64::from(max_height) / f64::from(height));
    (
        ((f64::from(width) * scale).round() as i32).max(1),
        ((f64::from(height) * scale).round() as i32).max(1),
    )
}

fn image_room(limit: Size, chrome: Size) -> Size {
    Size {
        width: (limit.width - chrome.width).max(1),
        height: (limit.height - chrome.height - NOTE_IMAGE_ROOM).max(1),
    }
}

fn image_room_after_y(room: Size, layout_y: i32) -> Size {
    Size {
        width: room.width,
        height: (room.height - layout_y.max(0)).max(1),
    }
}

fn resize_width_limit(room: Size, aspect: f64) -> i32 {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    room.width
        .min((f64::from(room.height) * aspect).floor() as i32)
        .clamp(NOTE_IMAGE_MIN, NOTE_IMAGE_MAX)
}

// The size an edge drag lands on. The larger of the two deltas drives a corner,
// dragging down enlarges as readily as dragging right, and the aspect ratio of
// the pasted image is never distorted.
fn resized_image_size(
    start_width: i32,
    aspect: f64,
    dx: f64,
    dy: f64,
    max_width: i32,
) -> (i32, i32) {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect
    } else {
        1.0
    };
    let delta = if dx.abs() >= dy.abs() {
        dx
    } else {
        dy * aspect
    };
    let max_width = max_width.clamp(NOTE_IMAGE_MIN, NOTE_IMAGE_MAX);
    let width = ((f64::from(start_width) + delta).round() as i32).clamp(NOTE_IMAGE_MIN, max_width);
    let height = ((f64::from(width) / aspect).round() as i32).clamp(1, NOTE_IMAGE_MAX);
    (width, height)
}

// The history row shows one line of the note, and a placeholder would render
// as an empty box there; say the note holds an image instead.
fn note_headline(text: &str) -> String {
    let line = text
        .lines()
        .map(|line| {
            line.chars()
                .filter(|value| *value != IMAGE_PLACEHOLDER)
                .collect::<String>()
        })
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default();
    let line = line.trim().to_owned();
    if !line.is_empty() {
        line
    } else if text.contains(IMAGE_PLACEHOLDER) {
        "Image".to_owned()
    } else {
        "Untitled note".to_owned()
    }
}

fn tag_image_source(pixbuf: &Pixbuf, file: &str) {
    unsafe { pixbuf.set_data(NOTE_IMAGE_DATA_KEY, file.to_owned()) };
}

fn image_source(pixbuf: &Pixbuf) -> Option<String> {
    unsafe {
        pixbuf
            .data::<String>(NOTE_IMAGE_DATA_KEY)
            .map(|value| value.as_ref().clone())
    }
}

// The full-resolution bytes, cached per note card: growing an image back after
// shrinking it rescales from the original instead of from the shrunk copy.
fn original_image(file: &str, originals: &ImageOriginals) -> Option<Pixbuf> {
    if let Some(pixbuf) = originals.borrow().get(file) {
        return Some(pixbuf.clone());
    }
    let pixbuf = Pixbuf::from_file(crate::state::images_dir().join(file)).ok()?;
    originals
        .borrow_mut()
        .insert(file.to_owned(), pixbuf.clone());
    Some(pixbuf)
}

fn scaled_image(file: &str, width: i32, height: i32, originals: &ImageOriginals) -> Option<Pixbuf> {
    let original = original_image(file, originals)?;
    let width = width.clamp(NOTE_IMAGE_MIN, NOTE_IMAGE_MAX);
    let height = height.clamp(1, NOTE_IMAGE_MAX);
    let pixbuf = if original.width() == width && original.height() == height {
        original
    } else {
        original.scale_simple(width, height, InterpType::Bilinear)?
    };
    // The corners are rounded on the copy that goes into the note, not on the
    // stored original: a square corner would poke out past the focus outline,
    // which is drawn after the text and cannot erase what is under it.
    let pixbuf = round_pixbuf_corners(&pixbuf, NOTE_IMAGE_BORDER_RADIUS).unwrap_or(pixbuf);
    tag_image_source(&pixbuf, file);
    Some(pixbuf)
}

// Cut the corners out of the alpha channel directly rather than through a
// cairo clip: gdk_pixbuf_get_from_surface needs an initialised GDK, so a clip
// would make this untestable and display-bound for no gain.
fn round_pixbuf_corners(pixbuf: &Pixbuf, radius: f64) -> Option<Pixbuf> {
    let target = if pixbuf.has_alpha() {
        pixbuf.copy()?
    } else {
        pixbuf.add_alpha(false, 0, 0, 0).ok()?
    };
    let width = target.width();
    let height = target.height();
    let radius = radius
        .min(f64::from(width) / 2.0)
        .min(f64::from(height) / 2.0);
    let channels = target.n_channels() as usize;
    if radius <= 0.0 || channels < 4 {
        return Some(target);
    }
    let stride = target.rowstride() as usize;
    let limit = (radius.ceil() as i32).min(width).min(height);
    // SAFETY: `target` was just created here, so no other reference to its
    // pixel buffer exists while the alpha channel is rewritten.
    let pixels = unsafe { target.pixels() };
    for corner_y in 0..limit {
        for corner_x in 0..limit {
            // How much of this pixel the rounded corner still covers. The
            // half-pixel ramp keeps the curve from looking like a staircase.
            let dx = (radius - (f64::from(corner_x) + 0.5)).max(0.0);
            let dy = (radius - (f64::from(corner_y) + 0.5)).max(0.0);
            let coverage = (radius - (dx * dx + dy * dy).sqrt() + 0.5).clamp(0.0, 1.0);
            if coverage >= 1.0 {
                continue;
            }
            for (x, y) in [
                (corner_x, corner_y),
                (width - 1 - corner_x, corner_y),
                (corner_x, height - 1 - corner_y),
                (width - 1 - corner_x, height - 1 - corner_y),
            ] {
                let index = y as usize * stride + x as usize * channels + 3;
                if let Some(alpha) = pixels.get_mut(index) {
                    *alpha = (f64::from(*alpha) * coverage).round() as u8;
                }
            }
        }
    }
    Some(target)
}

fn store_note_image(pixbuf: &Pixbuf, state: &Rc<RefCell<AppState>>) -> Option<String> {
    let dir = crate::state::images_dir();
    fs::create_dir_all(&dir).ok()?;
    let file = {
        let mut data = state.borrow_mut();
        let id = data.next_image_id;
        data.next_image_id = id.saturating_add(1);
        format!("{id}.png")
    };
    pixbuf.savev(dir.join(&file), "png", &[]).ok()?;
    Some(file)
}

// The saved form of a note: the text with a placeholder standing in for every
// image. `text()` would silently drop the images out of the flow.
fn note_buffer_text(buffer: &gtk::TextBuffer) -> String {
    buffer
        .slice(&buffer.start_iter(), &buffer.end_iter(), true)
        .map(|value| value.to_string())
        .unwrap_or_default()
}

// The image metadata behind those placeholders, in the same order. A pixbuf
// with no file behind it — an image copied from another note, or pasted by a
// path that bypassed the paste handler — is written out here, so it survives
// the restart instead of vanishing from the note.
fn note_buffer_images(
    buffer: &gtk::TextBuffer,
    text: &str,
    state: &Rc<RefCell<AppState>>,
) -> Vec<NoteImage> {
    let mut images = Vec::new();
    for (offset, character) in text.chars().enumerate() {
        if character != IMAGE_PLACEHOLDER {
            continue;
        }
        let Some(pixbuf) = buffer.iter_at_offset(offset as i32).pixbuf() else {
            continue;
        };
        let file = match image_source(&pixbuf) {
            Some(file) => file,
            None => {
                let Some(file) = store_note_image(&pixbuf, state) else {
                    continue;
                };
                tag_image_source(&pixbuf, &file);
                file
            }
        };
        images.push(NoteImage {
            file,
            width: pixbuf.width(),
            height: pixbuf.height(),
        });
    }
    images
}

fn note_snapshot(
    buffer: &gtk::TextBuffer,
    target: &NoteImageTarget,
    state: &Rc<RefCell<AppState>>,
) -> NoteSnapshot {
    let text = note_buffer_text(buffer);
    let images = note_buffer_images(buffer, &text, state);
    let cursor = buffer
        .get_insert()
        .map(|mark| buffer.iter_at_mark(&mark).offset())
        .unwrap_or_else(|| buffer.char_count());
    let allocation = target.card.allocation();
    let size = state
        .borrow()
        .sizes
        .get(&target.key)
        .copied()
        .unwrap_or(Size {
            width: allocation.width(),
            height: allocation.height(),
        });
    NoteSnapshot {
        text,
        images,
        cursor,
        size,
    }
}

fn record_note_undo(history: &NoteUndoState, before: NoteSnapshot, after: &NoteSnapshot) {
    // Moving the caret is navigation, not an edit. The saved cursor still
    // matters when the snapshot is restored, but does not create an undo step.
    if before.text == after.text && before.images == after.images && before.size == after.size {
        return;
    }
    let mut history = history.borrow_mut();
    history.undo.push(before);
    if history.undo.len() > NOTE_UNDO_LIMIT {
        history.undo.remove(0);
    }
    history.redo.clear();
}

fn fill_note_content(
    buffer: &gtk::TextBuffer,
    text: &str,
    images: &[NoteImage],
    originals: &ImageOriginals,
) {
    buffer.set_text("");
    let mut images = images.iter();
    for (index, chunk) in text.split(IMAGE_PLACEHOLDER).enumerate() {
        if index > 0 {
            // Every chunk after the first is preceded by one placeholder. An
            // image whose file went missing simply drops out of the note.
            if let Some(pixbuf) = images
                .next()
                .and_then(|image| scaled_image(&image.file, image.width, image.height, originals))
            {
                let mut end = buffer.end_iter();
                buffer.insert_pixbuf(&mut end, &pixbuf);
            }
        }
        let mut end = buffer.end_iter();
        buffer.insert(&mut end, chunk);
    }
}

fn fill_note_buffer(buffer: &gtk::TextBuffer, note: &Note, originals: &ImageOriginals) {
    fill_note_content(buffer, &note.text, &note.images, originals);
}

// Where the image at `offset` is drawn, in widget coordinates — the same space
// button events and the draw context use, since a note view has no border
// windows.
fn image_rect(editor: &gtk::TextView, offset: i32) -> Option<(f64, f64, f64, f64)> {
    let buffer = editor.buffer()?;
    if offset < 0 || offset >= buffer.char_count() {
        return None;
    }
    let iter = buffer.iter_at_offset(offset);
    iter.pixbuf()?;
    let location = editor.iter_location(&iter);
    let (x, y) =
        editor.buffer_to_window_coords(gtk::TextWindowType::Widget, location.x(), location.y());
    Some((
        f64::from(x),
        f64::from(y),
        f64::from(location.width()),
        f64::from(location.height()),
    ))
}

// The image's bottom in the complete text layout, not merely in the currently
// scrolled viewport. This accounts for text and earlier images above a newly
// pasted image when deciding how far the note should grow.
fn image_layout_extent(editor: &gtk::TextView, offset: i32, fallback: Size) -> Size {
    let Some(buffer) = editor.buffer() else {
        return fallback;
    };
    let iter = buffer.iter_at_offset(offset);
    if iter.pixbuf().is_none() {
        return fallback;
    }
    let location = editor.iter_location(&iter);
    Size {
        width: fallback.width,
        height: (location.y() + location.height()).max(fallback.height),
    }
}

fn image_at_pointer(editor: &gtk::TextView, x: f64, y: f64) -> Option<FocusedImage> {
    let (buffer_x, buffer_y) =
        editor.window_to_buffer_coords(gtk::TextWindowType::Widget, x as i32, y as i32);
    let iter = editor.iter_at_location(buffer_x, buffer_y)?;
    let buffer = editor.buffer()?;
    // On the trailing half of an inline object GTK may return the insertion
    // position *after* it. Probe that previous offset as well, otherwise the
    // right-edge resize strip is impossible to acquire.
    for offset in [iter.offset(), iter.offset() - 1] {
        if offset < 0 {
            continue;
        }
        let Some(pixbuf) = buffer.iter_at_offset(offset).pixbuf() else {
            continue;
        };
        let Some((rect_x, rect_y, width, height)) = image_rect(editor, offset) else {
            continue;
        };
        if x >= rect_x && x <= rect_x + width && y >= rect_y && y <= rect_y + height {
            return Some(FocusedImage {
                offset,
                width: pixbuf.width(),
                height: pixbuf.height(),
            });
        }
    }
    None
}

fn image_resize_zone(editor: &gtk::TextView, x: f64, y: f64) -> Option<FocusedImage> {
    let image = image_at_pointer(editor, x, y)?;
    let (rect_x, rect_y, width, height) = image_rect(editor, image.offset)?;
    let handle = f64::from(NOTE_IMAGE_HANDLE_HIT);
    (x >= rect_x + width - handle
        && x <= rect_x + width
        && y >= rect_y + height - handle
        && y <= rect_y + height)
        .then_some(image)
}

// The image body is not text, so it must not keep the caret cursor; only the
// little corner arc advertises a resize.
fn image_pointer_cursor(editor: &gtk::TextView, x: f64, y: f64) -> gdk::CursorType {
    if editor.is_editable() {
        if image_resize_zone(editor, x, y).is_some() {
            return gdk::CursorType::BottomRightCorner;
        }
        if image_at_pointer(editor, x, y).is_some() {
            return gdk::CursorType::Arrow;
        }
    }
    gdk::CursorType::Xterm
}

// GtkTextView owns the I-beam on its text window — a child of the widget
// window — and re-asserts it there, so a cursor set on the widget itself is
// never the one the pointer shows. Set it on the window the pointer is over.
fn set_editor_cursor(editor: &gtk::TextView, cursor: gdk::CursorType) {
    let Some(window) = TextViewExt::window(editor, gtk::TextWindowType::Text) else {
        return;
    };
    let cursor = gdk::Cursor::for_display(&window.display(), cursor);
    window.set_cursor(cursor.as_ref());
}

fn rounded_rectangle(ctx: &Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0).max(0.0);
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

fn replace_note_image(
    editor: &gtk::TextView,
    offset: i32,
    width: i32,
    height: i32,
    originals: &ImageOriginals,
) -> Option<()> {
    let buffer = editor.buffer()?;
    let pixbuf = buffer.iter_at_offset(offset).pixbuf()?;
    let file = image_source(&pixbuf)?;
    let scaled = scaled_image(&file, width, height, originals).or_else(|| {
        // The source file is gone; rescale what is on screen so the drag still
        // does something instead of freezing.
        let scaled = pixbuf.scale_simple(width, height, InterpType::Bilinear)?;
        tag_image_source(&scaled, &file);
        Some(scaled)
    })?;
    let mut start = buffer.iter_at_offset(offset);
    let mut end = buffer.iter_at_offset(offset + 1);
    buffer.delete(&mut start, &mut end);
    let mut at = buffer.iter_at_offset(offset);
    buffer.insert_pixbuf(&mut at, &scaled);
    buffer.place_cursor(&buffer.iter_at_offset(offset + 1));
    Some(())
}

fn copy_focused_image(
    editor: &gtk::TextView,
    focus: &ImageFocus,
    originals: &ImageOriginals,
) -> bool {
    let Some(image) = *focus.borrow() else {
        return false;
    };
    let Some(buffer) = editor.buffer() else {
        return false;
    };
    // An explicit text selection wins; Ctrl+C must keep behaving like a normal
    // editor even if an old image focus outline is still present.
    if buffer.has_selection() {
        return false;
    }
    let Some(displayed) = buffer.iter_at_offset(image.offset).pixbuf() else {
        return false;
    };
    let pixbuf = image_source(&displayed)
        .and_then(|file| original_image(&file, originals))
        .unwrap_or(displayed);
    editor
        .clipboard(&gdk::SELECTION_CLIPBOARD)
        .set_image(&pixbuf);
    true
}

fn paste_note_image(
    editor: &gtk::TextView,
    target: &NoteImageTarget,
    state: &Rc<RefCell<AppState>>,
    originals: &ImageOriginals,
) -> bool {
    if !editor.is_editable() {
        return false;
    }
    let clipboard = editor.clipboard(&gdk::SELECTION_CLIPBOARD);
    if !clipboard.wait_is_image_available() {
        return false;
    }
    let Some(pixbuf) = clipboard.wait_for_image() else {
        return false;
    };
    let Some(buffer) = editor.buffer() else {
        return false;
    };
    let Some(insert) = buffer.get_insert() else {
        return false;
    };
    // Text and earlier images above the insertion point already consume some
    // of the note's vertical growth budget.
    let insertion_y = editor.iter_location(&buffer.iter_at_mark(&insert)).y();
    let Some(file) = store_note_image(&pixbuf, state) else {
        return false;
    };
    originals.borrow_mut().insert(file.clone(), pixbuf.clone());
    let room = image_room_after_y(
        image_room(
            note_growth_limit(&target.card),
            note_image_chrome(editor, &target.card),
        ),
        insertion_y,
    );
    let (width, height) = fit_within_bounds(
        pixbuf.width(),
        pixbuf.height(),
        note_image_cap(room.width),
        NOTE_IMAGE_DEFAULT_MAX.min(room.height),
    );
    let Some(scaled) = scaled_image(&file, width, height, originals) else {
        originals.borrow_mut().remove(&file);
        let _ = fs::remove_file(crate::state::images_dir().join(&file));
        return false;
    };
    // Pasting over a selection replaces it, the way a text paste does.
    buffer.delete_selection(true, true);
    let Some(insert) = buffer.get_insert() else {
        originals.borrow_mut().remove(&file);
        let _ = fs::remove_file(crate::state::images_dir().join(&file));
        return false;
    };
    let mut at = buffer.iter_at_mark(&insert);
    let offset = at.offset();
    buffer.insert_pixbuf(&mut at, &scaled);
    buffer.place_cursor(&buffer.iter_at_offset(offset + 1));
    // Include any text/images already above this one. Growing for the pixbuf's
    // height alone clips a paste made after a few lines of text.
    let extent = image_layout_extent(editor, offset, Size { width, height });
    grow_note_for_image(editor, target, state, extent);
    // Saved to disk already, so keeping the full-resolution paste in memory
    // buys nothing until the image is actually resized.
    originals.borrow_mut().remove(&file);
    true
}

// What a pasted image needs to know about the note around it: the card to grow
// and the shape to refresh once it has.
struct NoteImageTarget {
    card: gtk::EventBox,
    key: String,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    window: gtk::ApplicationWindow,
    interactive: Rc<Cell<bool>>,
}

fn apply_note_snapshot(
    editor: &gtk::TextView,
    target: &NoteImageTarget,
    state: &Rc<RefCell<AppState>>,
    originals: &ImageOriginals,
    snapshot: &NoteSnapshot,
) {
    let Some(buffer) = editor.buffer() else {
        return;
    };
    target
        .card
        .set_size_request(snapshot.size.width, snapshot.size.height);
    target.card.queue_resize();
    state
        .borrow_mut()
        .sizes
        .insert(target.key.clone(), snapshot.size);
    fill_note_content(&buffer, &snapshot.text, &snapshot.images, originals);
    let cursor = snapshot.cursor.clamp(0, buffer.char_count());
    buffer.place_cursor(&buffer.iter_at_offset(cursor));
    // Buffer change handlers have synchronously copied the restored content
    // into AppState by this point; persist both content and note geometry now.
    let _ = state.borrow().save();
    editor.queue_draw();
    let window = target.window.clone();
    let registry = target.registry.clone();
    let interactive = target.interactive.clone();
    glib::idle_add_local_once(move || {
        refresh_input_shape(&window, &registry, interactive.get());
    });
}

fn undo_note_edit(
    editor: &gtk::TextView,
    target: &NoteImageTarget,
    state: &Rc<RefCell<AppState>>,
    originals: &ImageOriginals,
    history: &NoteUndoState,
    redo: bool,
) -> bool {
    let Some(buffer) = editor.buffer() else {
        return false;
    };
    let current = note_snapshot(&buffer, target, state);
    let restore = {
        let mut history = history.borrow_mut();
        let restore = if redo {
            history.redo.pop()
        } else {
            history.undo.pop()
        };
        let Some(restore) = restore else {
            return false;
        };
        if redo {
            history.undo.push(current);
        } else {
            history.redo.push(current);
        }
        history.applying = true;
        history.pending = None;
        restore
    };
    apply_note_snapshot(editor, target, state, originals, &restore);
    history.borrow_mut().applying = false;
    true
}

// Hover the right/bottom image edge to resize it directly. Clicking the image
// still focuses it and parks the cursor immediately after it, so Backspace
// deletes it and typing continues after it.
fn attach_note_images(
    editor: &gtk::TextView,
    target: NoteImageTarget,
    state: &Rc<RefCell<AppState>>,
) -> ImageOriginals {
    let target = Rc::new(target);
    let originals: ImageOriginals = Rc::new(RefCell::new(HashMap::new()));
    let focus: ImageFocus = Rc::new(RefCell::new(None));
    let hover: ImageFocus = Rc::new(RefCell::new(None));
    let resize: ImageResizeState = Rc::new(RefCell::new(None));
    let undo: NoteUndoState = Rc::new(RefCell::new(NoteUndo::default()));

    editor.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::POINTER_MOTION_MASK
            | gdk::EventMask::LEAVE_NOTIFY_MASK,
    );

    editor.connect_paste_clipboard({
        let state = state.clone();
        let originals = originals.clone();
        let target = target.clone();
        move |editor| {
            let Some(buffer) = editor.buffer() else {
                return;
            };
            buffer.begin_user_action();
            let pasted = paste_note_image(editor, &target, &state, &originals);
            buffer.end_user_action();
            if pasted {
                // The default handler would paste the clipboard's text form of
                // the same image next to it — a file URL, or an HTML img tag.
                glib::signal_stop_emission_by_name(editor, "paste-clipboard");
            }
        }
    });

    // The focused image is held as a buffer offset, so any edit before it
    // shifts the image out from under that offset: the outline would trace
    // whatever now sits there and Ctrl+C would copy it. A live resize edits the
    // buffer itself and keeps its focus; every other edit drops it.
    if let Some(buffer) = editor.buffer() {
        buffer.connect_changed({
            let focus = focus.clone();
            let resize = resize.clone();
            let editor = editor.clone();
            move |_| {
                if resize.borrow().is_some() {
                    return;
                }
                if focus.borrow_mut().take().is_some() {
                    editor.queue_draw();
                }
            }
        });
    }

    // GtkTextBuffer tells us where a logical user edit starts and ends. Keep a
    // full mixed text/image snapshot at that boundary so one Ctrl+Z reverses
    // one edit (including a paste or deleting an inline image).
    if let Some(buffer) = editor.buffer() {
        buffer.connect_begin_user_action({
            let undo = undo.clone();
            let target = target.clone();
            let state = state.clone();
            move |buffer| {
                let mut history = undo.borrow_mut();
                if !history.applying && history.pending.is_none() {
                    history.pending = Some(note_snapshot(buffer, &target, &state));
                }
            }
        });
        buffer.connect_end_user_action({
            let undo = undo.clone();
            let target = target.clone();
            let state = state.clone();
            move |buffer| {
                if undo.borrow().applying {
                    return;
                }
                let before = undo.borrow_mut().pending.take();
                if let Some(before) = before {
                    let after = note_snapshot(buffer, &target, &state);
                    record_note_undo(&undo, before, &after);
                }
            }
        });
    }

    editor.connect_key_press_event({
        let undo = undo.clone();
        let target = target.clone();
        let state = state.clone();
        let originals = originals.clone();
        let focus = focus.clone();
        move |editor, event| {
            if !editor.is_editable() {
                return glib::Propagation::Proceed;
            }
            if !event.state().contains(gdk::ModifierType::CONTROL_MASK) {
                return glib::Propagation::Proceed;
            }
            let key = event.keyval();
            let c = key == gdk::keys::constants::c || key == gdk::keys::constants::C;
            if c {
                return if copy_focused_image(editor, &focus, &originals) {
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                };
            }
            let z = key == gdk::keys::constants::z || key == gdk::keys::constants::Z;
            let y = key == gdk::keys::constants::y || key == gdk::keys::constants::Y;
            if !z && !y {
                return glib::Propagation::Proceed;
            }
            let redo = y || event.state().contains(gdk::ModifierType::SHIFT_MASK);
            if undo_note_edit(editor, &target, &state, &originals, &undo, redo) {
                *focus.borrow_mut() = None;
                editor.queue_draw();
                glib::Propagation::Stop
            } else {
                // Consume an empty undo/redo too; otherwise GTK emits a bell.
                glib::Propagation::Stop
            }
        }
    });

    editor.connect_button_press_event({
        let focus = focus.clone();
        let resize = resize.clone();
        let target = target.clone();
        let state = state.clone();
        move |editor, event| {
            if event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            let (x, y) = event.position();
            if editor.is_editable() {
                if let Some(image) = image_resize_zone(editor, x, y) {
                    let Some(buffer) = editor.buffer() else {
                        return glib::Propagation::Proceed;
                    };
                    *focus.borrow_mut() = Some(image);
                    editor.queue_draw();
                    *resize.borrow_mut() = Some(ImageResize {
                        offset: image.offset,
                        start_x: x,
                        start_y: y,
                        start_width: image.width,
                        aspect: f64::from(image.width) / f64::from(image.height.max(1)),
                        before: note_snapshot(&buffer, &target, &state),
                    });
                    return glib::Propagation::Stop;
                }
            }
            // Lock mode is read-only: no frame and no edge resize affordance.
            let hit = editor
                .is_editable()
                .then(|| image_at_pointer(editor, x, y))
                .flatten();
            let changed = hit.map(|image| image.offset) != focus.borrow().map(|image| image.offset);
            *focus.borrow_mut() = hit;
            if changed {
                editor.queue_draw();
            }
            match hit {
                Some(image) => {
                    // Park the cursor after the image instead of letting the
                    // click drop it inside the selection GTK would start.
                    if let Some(buffer) = editor.buffer() {
                        buffer.place_cursor(&buffer.iter_at_offset(image.offset + 1));
                    }
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        }
    });

    editor.connect_motion_notify_event({
        let focus = focus.clone();
        let hover = hover.clone();
        let resize = resize.clone();
        let originals = originals.clone();
        let target = target.clone();
        let state = state.clone();
        move |editor, event| {
            let (x, y) = event.position();
            let Some(drag) = resize.borrow().clone() else {
                let hovered = editor
                    .is_editable()
                    .then(|| image_at_pointer(editor, x, y))
                    .flatten();
                let hover_changed =
                    hovered.map(|image| image.offset) != hover.borrow().map(|image| image.offset);
                *hover.borrow_mut() = hovered;
                if hover_changed {
                    editor.queue_draw();
                }
                set_editor_cursor(editor, image_pointer_cursor(editor, x, y));
                return glib::Propagation::Proceed;
            };
            // Never wider than the grown note can show: an image dragged past
            // the note edge takes its own resize edge out of reach.
            let layout_y = editor
                .buffer()
                .map(|buffer| {
                    editor
                        .iter_location(&buffer.iter_at_offset(drag.offset))
                        .y()
                })
                .unwrap_or(0);
            let room = image_room_after_y(
                image_room(
                    note_growth_limit(&target.card),
                    note_image_chrome(editor, &target.card),
                ),
                layout_y,
            );
            set_editor_cursor(editor, gdk::CursorType::BottomRightCorner);
            let max_width = resize_width_limit(room, drag.aspect);
            let (width, height) = resized_image_size(
                drag.start_width,
                drag.aspect,
                x - drag.start_x,
                y - drag.start_y,
                max_width,
            );
            if focus.borrow().map(|image| image.width) == Some(width) {
                return glib::Propagation::Stop;
            }
            if replace_note_image(editor, drag.offset, width, height, &originals).is_some() {
                *focus.borrow_mut() = Some(FocusedImage {
                    offset: drag.offset,
                    width,
                    height,
                });
                let extent = image_layout_extent(editor, drag.offset, Size { width, height });
                grow_note_for_image(editor, &target, &state, extent);
                editor.queue_draw();
            }
            glib::Propagation::Stop
        }
    });

    editor.connect_button_release_event({
        let resize = resize.clone();
        let hover = hover.clone();
        let originals = originals.clone();
        let undo = undo.clone();
        let target = target.clone();
        let state = state.clone();
        move |editor, event| {
            if let Some(drag) = resize.borrow_mut().take() {
                if let Some(buffer) = editor.buffer() {
                    let after = note_snapshot(&buffer, &target, &state);
                    record_note_undo(&undo, drag.before, &after);
                }
                // The cache exists so one drag rescales from full-resolution
                // bytes instead of from its own output. Holding a 4K
                // screenshot in memory after the drag has nothing left to
                // serve, and the file is one cheap load away.
                originals.borrow_mut().clear();
                let (x, y) = event.position();
                *hover.borrow_mut() = image_at_pointer(editor, x, y);
                set_editor_cursor(editor, image_pointer_cursor(editor, x, y));
                editor.queue_draw();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    editor.connect_leave_notify_event({
        let resize = resize.clone();
        let hover = hover.clone();
        move |editor, _| {
            if resize.borrow().is_none() {
                if hover.borrow_mut().take().is_some() {
                    editor.queue_draw();
                }
                set_editor_cursor(editor, gdk::CursorType::Xterm);
            }
            glib::Propagation::Proceed
        }
    });

    // Focus gets a rounded gray outline. Hover reveals the same little corner
    // arc used by note cards; only that arc is draggable.
    editor.connect_local("draw", true, {
        let focus = focus.clone();
        let hover = hover.clone();
        move |values| {
            let editor = values
                .first()
                .and_then(|value| value.get::<gtk::TextView>().ok());
            let ctx = values
                .get(1)
                .and_then(|value| value.get::<cairo::Context>().ok());
            if let (Some(editor), Some(ctx)) = (editor, ctx) {
                // Lock mode is read-only, so it shows no resize affordance.
                if !editor.is_editable() {
                    return Some(false.to_value());
                }
                if let Some((x, y, width, height)) = focus
                    .borrow()
                    .map(|image| image.offset)
                    .and_then(|offset| image_rect(&editor, offset))
                {
                    ctx.set_line_width(1.25);
                    ctx.set_source_rgba(0.55, 0.55, 0.55, 0.9);
                    rounded_rectangle(
                        &ctx,
                        x - 0.5,
                        y - 0.5,
                        width + 1.0,
                        height + 1.0,
                        NOTE_IMAGE_BORDER_RADIUS + 0.5,
                    );
                    let _ = ctx.stroke();
                }
                if let Some((x, y, width, height)) = hover
                    .borrow()
                    .map(|image| image.offset)
                    .and_then(|offset| image_rect(&editor, offset))
                {
                    ctx.set_source_rgba(0.55, 0.55, 0.55, 0.95);
                    ctx.set_line_width(1.6);
                    ctx.set_line_cap(cairo::LineCap::Round);
                    ctx.new_sub_path();
                    ctx.arc(x + width - 8.5, y + height - 8.5, 6.5, 0.0, PI / 2.0);
                    let _ = ctx.stroke();
                }
            }
            Some(false.to_value())
        }
    });

    originals
}

fn rebuild_pinned_notes(
    root: &gtk::Fixed,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    refresh: CallbackSlot,
    interactive: Rc<Cell<bool>>,
    window: gtk::ApplicationWindow,
    lookup: LookupSlot,
) {
    let old: Vec<gtk::EventBox> = registry
        .borrow()
        .iter()
        .filter(|item| item.key.starts_with("note:"))
        .map(|item| item.widget.clone())
        .collect();
    for widget in old {
        root.remove(&widget);
        // Removing a card only drops the container's reference to it. Its own
        // handlers hold the card (through the image target they need to grow
        // and resize it), so the card, its editor and every pixbuf in that
        // editor's buffer would outlive the rebuild. Destroying it disposes
        // the object, which disconnects those handlers and breaks the cycle.
        // SAFETY: the card has just been unparented and nothing reads it
        // again; the loop owns the only remaining reference.
        unsafe { widget.destroy() };
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
            Some(lookup.clone()),
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
                max_width: NOTE_MAX_WIDTH,
                max_height: NOTE_MAX_HEIGHT,
                aspect_ratio: None,
                preserve_current_aspect: false,
            },
        );

        // Attached after the card is registered and sized, so a paste can grow
        // the note and refresh the input shape for it. Filling the buffer here,
        // before the change handler below, keeps the load out of the save path.
        let image_originals = attach_note_images(
            &editor,
            NoteImageTarget {
                card: card.clone(),
                key: format!("note:{}", note.id),
                registry: registry.clone(),
                window: window.clone(),
                interactive: interactive.clone(),
            },
            &state,
        );
        fill_note_buffer(
            &editor.buffer().expect("note buffer"),
            &note,
            &image_originals,
        );

        let pending_save: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
        editor.buffer().expect("note buffer").connect_changed({
            let state = state.clone();
            let pending_save = pending_save.clone();
            let id = note.id;
            move |buffer| {
                let text = note_buffer_text(buffer);
                // Resolved before the note is borrowed: adopting an untagged
                // pixbuf writes a file and bumps the image counter in state.
                let images = note_buffer_images(buffer, &text, &state);
                if let Some(note) = state.borrow_mut().notes.iter_mut().find(|n| n.id == id) {
                    note.text = text;
                    note.images = images;
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
                state.borrow().prune_orphan_images();
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

// The dictionary window is the history window's twin: the same note chrome and
// resize grip, a plain title bar to grab it by, and a scrolling column of
// results. The query box is a drop-down panel rather than part of the header,
// so the header stays easy to grab and the box is free to grow with a paste.
struct TranslateWindow {
    card: gtk::EventBox,
    /// The header and the query panel together. Registered as the window's edit
    /// chrome, so locking the overlay takes the whole search affordance away
    /// instead of leaving a headless card with a live text box in it.
    chrome: gtk::EventBox,
    header: gtk::EventBox,
    hide: gtk::Button,
    open_search: gtk::Button,
    close_search: gtk::Button,
    search_panel: gtk::Box,
    input: gtk::TextView,
    suggestions: gtk::Box,
    results: gtk::Box,
    color_mode: Rc<Cell<ColorMode>>,
    resize: ResizeHandle,
}

impl TranslateWindow {
    /// Drop the query panel down or put it away. The magnifier and the cross
    /// share the header's trailing slot, so only one of them ever shows.
    fn set_search_visible(&self, open: bool) {
        self.search_panel.set_visible(open);
        self.open_search.set_visible(!open);
        self.close_search.set_visible(open);
    }

    fn query(&self) -> String {
        let Some(buffer) = self.input.buffer() else {
            return String::new();
        };
        let (start, end) = buffer.bounds();
        buffer
            .text(&start, &end, false)
            .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    }

    fn set_query(&self, text: &str) {
        if let Some(buffer) = self.input.buffer() {
            buffer.set_text(text);
        }
    }
}

fn build_translate_window(initial_color_mode: ColorMode) -> TranslateWindow {
    let (card, body, _drag, color_mode, resize) = card_shell("", "", initial_color_mode);
    card.style_context().add_class("pinned-note");
    card.style_context().add_class("translate-window");
    card.set_visible_window(true);

    // The title bar and the query panel move together in and out of lock mode,
    // so they share one container that the registry can hide as edit chrome.
    let chrome = gtk::EventBox::new();
    chrome.set_visible_window(false);
    chrome.set_hexpand(true);
    let chrome_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    chrome.add(&chrome_box);
    body.pack_start(&chrome, false, false, 0);

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
    hide.set_tooltip_text(Some("Hide Dictionary"));
    // The title fills the row so almost all of the header is drag surface.
    let title = gtk::Label::new(Some("DICTIONARY"));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.style_context().add_class("history-title");
    let open_search = icon_button("edit-find-symbolic", "Search");
    let close_search = small_button("\u{00d7}");
    close_search.style_context().add_class("note-window-button");
    close_search.style_context().add_class("note-close");
    close_search.set_tooltip_text(Some("Close search"));
    bar.pack_start(&hide, false, false, 0);
    bar.pack_start(&title, true, true, 0);
    // Packed end-first so both occupants of the trailing slot land in the same
    // spot, whichever one is showing.
    bar.pack_end(&close_search, false, false, 0);
    bar.pack_end(&open_search, false, false, 0);
    header.add(&bar);
    chrome_box.pack_start(&header, false, false, 0);

    // The search panel drops down under the header: a text view that grows with
    // what is typed or pasted, and the completions right beneath it.
    let search_panel = gtk::Box::new(gtk::Orientation::Vertical, 1);
    search_panel.style_context().add_class("translate-search-panel");
    let input_scroller =
        gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    input_scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Automatic);
    input_scroller.set_shadow_type(gtk::ShadowType::None);
    input_scroller.set_overlay_scrolling(true);
    // Height follows the content, capped at a few lines: a pasted paragraph
    // makes the box taller instead of scrolling a single line sideways, but it
    // can never crowd out the results below it.
    input_scroller.set_propagate_natural_height(true);
    input_scroller.set_min_content_height(TRANSLATE_INPUT_MIN_HEIGHT);
    input_scroller.set_max_content_height(TRANSLATE_INPUT_MAX_HEIGHT);
    input_scroller.set_propagate_natural_width(false);
    input_scroller.set_size_request(1, 1);
    input_scroller.set_hexpand(true);
    let input = gtk::TextView::new();
    input.set_wrap_mode(gtk::WrapMode::WordChar);
    input.set_accepts_tab(false);
    // Same width discipline as the note editor: without it the longest line
    // would set a minimum the card could never be dragged below.
    input.set_size_request(1, 1);
    input.style_context().add_class("translate-search-input");
    input_scroller.add(&input);
    search_panel.pack_start(&input_scroller, false, false, 0);

    // The completions live inside the panel so they come and go with it.
    let suggestions = gtk::Box::new(gtk::Orientation::Vertical, 0);
    suggestions.style_context().add_class("translate-suggestions");
    search_panel.pack_start(&suggestions, false, false, 0);
    chrome_box.pack_start(&search_panel, false, false, 0);

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    // External (not Never) horizontally, or the widest definition line would
    // stop the window from ever being dragged narrower again.
    scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Automatic);
    scroller.set_overlay_scrolling(true);
    scroller.set_shadow_type(gtk::ShadowType::None);
    scroller.set_propagate_natural_width(false);
    scroller.set_propagate_natural_height(false);
    scroller.set_size_request(1, 1);
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.style_context().add_class("history-scroller");
    let results = gtk::Box::new(gtk::Orientation::Vertical, 3);
    results.style_context().add_class("translate-results");
    scroller.add(&results);
    body.pack_start(&scroller, true, true, 0);

    TranslateWindow {
        card,
        chrome,
        header,
        hide,
        open_search,
        close_search,
        search_panel,
        input,
        suggestions,
        results,
        color_mode,
        resize,
    }
}

fn clear_children(container: &gtk::Box) {
    for child in container.children() {
        container.remove(&child);
    }
}

// Every line of a result is a wrapped, left-aligned label; only the CSS class
// and the markup differ. WordChar wrapping matters because a long IPA string or
// a URL-like token would otherwise have no break point at all.
fn translate_line(markup: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_markup(markup);
    label.set_xalign(0.0);
    label.set_line_wrap(true);
    label.set_line_wrap_mode(gtk::pango::WrapMode::WordChar);
    // A wrapping label asks for its whole unwrapped text as its natural width,
    // and GtkFixed hands out natural sizes — so one long definition would drag
    // the card wider than the user ever sized it. Capping the request keeps the
    // width the user's to choose and lets the text reflow into it.
    label.set_max_width_chars(TRANSLATE_WRAP_CHARS);
    label.set_selectable(true);
    label.style_context().add_class(class);
    label
}

fn translate_row(spacing: i32) -> gtk::Box {
    gtk::Box::new(gtk::Orientation::Horizontal, spacing)
}

// A phonetic transcription is short and has no sensible break point, so it is
// the one line that stays on one line.
fn translate_inline(markup: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::new(None);
    label.set_markup(markup);
    label.set_xalign(0.0);
    label.set_selectable(true);
    label.style_context().add_class(class);
    label
}

/// A word the user can click to look up: the completions under the entry and
/// the "did you mean" chips both use it.
fn translate_word_button(word: &str, lookup: &Rc<dyn Fn(&str)>) -> gtk::Button {
    let button = gtk::Button::with_label(word);
    button.set_can_focus(false);
    button.style_context().add_class("translate-suggestion");
    if let Some(label) = button.child().and_then(|child| child.downcast::<gtk::Label>().ok()) {
        label.set_xalign(0.0);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        // The completions sit outside the scroller, so a long word would widen
        // the card the same way a definition line would.
        label.set_max_width_chars(TRANSLATE_WRAP_CHARS);
    }
    button.connect_clicked({
        let lookup = lookup.clone();
        let word = word.to_owned();
        move |_| lookup(&word)
    });
    button
}

fn render_translate_suggestions(
    suggestions: &gtk::Box,
    items: &[String],
    lookup: &Rc<dyn Fn(&str)>,
) {
    clear_children(suggestions);
    for word in items {
        suggestions.pack_start(&translate_word_button(word, lookup), false, false, 0);
    }
    suggestions.show_all();
}

/// What an empty search box offers instead of completions: the queries the user
/// ran before, newest first, folded away behind an arrow so that opening the
/// panel shows one line rather than the whole history.
fn render_translate_recents(
    suggestions: &gtk::Box,
    state: &Rc<RefCell<AppState>>,
    lookup: &Rc<dyn Fn(&str)>,
    expanded: &Rc<Cell<bool>>,
) {
    clear_children(suggestions);
    let recents = state.borrow().recent_searches.clone();
    if recents.is_empty() {
        return;
    }

    let toggle = gtk::Button::new();
    toggle.set_can_focus(false);
    toggle.style_context().add_class("translate-recent-toggle");
    let caption = gtk::Label::new(None);
    caption.set_xalign(0.0);
    toggle.add(&caption);

    let list = gtk::Box::new(gtk::Orientation::Vertical, 0);
    for query in &recents {
        list.pack_start(&translate_word_button(query, lookup), false, false, 0);
    }

    suggestions.pack_start(&toggle, false, false, 0);
    suggestions.pack_start(&list, false, false, 0);
    suggestions.show_all();

    // show_all() reveals the list whatever the fold said, so the arrow and the
    // list are put back in step right after it — and again on every click.
    let apply: Rc<dyn Fn(bool)> = {
        let caption = caption.clone();
        let list = list.clone();
        Rc::new(move |open: bool| {
            caption.set_label(if open {
                "\u{25be}  RECENT"
            } else {
                "\u{25b8}  RECENT"
            });
            list.set_visible(open);
        })
    };
    apply(expanded.get());
    toggle.connect_clicked({
        let expanded = expanded.clone();
        let apply = apply.clone();
        move |_| {
            expanded.set(!expanded.get());
            apply(expanded.get());
        }
    });
}

/// Move a query to the front of the recents, keeping the list short and free of
/// duplicates that differ only in case or spacing. Returns whether anything
/// changed, so an unchanged list costs no disk write.
fn push_recent_search(recents: &mut Vec<String>, query: &str, limit: usize) -> bool {
    let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
    if query.is_empty() || limit == 0 {
        return false;
    }
    if recents.first().is_some_and(|first| *first == query) {
        return false;
    }
    // Case-insensitively, so searching "Hello" after "hello" re-ranks the entry
    // rather than listing it twice.
    recents.retain(|past| !past.eq_ignore_ascii_case(&query));
    recents.insert(0, query);
    recents.truncate(limit);
    true
}

/// Record a query and persist it. Called once a lookup has actually produced
/// something, so the unbatched `save()` runs at most once per result.
fn remember_search(state: &Rc<RefCell<AppState>>, query: &str) {
    let mut data = state.borrow_mut();
    if push_recent_search(&mut data.recent_searches, query, TRANSLATE_RECENT_LIMIT) {
        let _ = data.save();
    }
}

/// Replace the answer column wholesale. Rebuilding rather than patching keeps
/// the render a pure function of the result, the way the note list works.
fn render_translate_result(
    results: &gtk::Box,
    result: &crate::translate::LookupResult,
    audio_buttons: &Rc<RefCell<HashMap<String, gtk::Button>>>,
    audio_tx: &async_channel::Sender<crate::translate::TranslateEvent>,
    lookup: &Rc<dyn Fn(&str)>,
) {
    use crate::translate::{escape_markup, ResultKind};

    clear_children(results);
    // The buttons in the column just went away; anything still downloading for
    // them has nothing left to re-enable.
    audio_buttons.borrow_mut().clear();

    match &result.kind {
        ResultKind::Sentence(sentence) => {
            let heading = match sentence.detected.as_deref() {
                Some("vi") => "VIETNAMESE \u{2192} ENGLISH",
                _ => "VIETNAMESE",
            };
            results.pack_start(&translate_line(heading, "translate-pos"), false, false, 0);
            results.pack_start(
                &translate_line(&escape_markup(&sentence.translation), "translate-body"),
                false,
                false,
                0,
            );
            results.pack_start(
                &translate_line("ORIGINAL", "translate-pos"),
                false,
                false,
                0,
            );
            results.pack_start(
                &translate_line(&escape_markup(&sentence.source), "translate-source"),
                false,
                false,
                0,
            );
        }
        ResultKind::Word(word) => {
            results.pack_start(
                &translate_line(
                    &format!("<b>{}</b>", escape_markup(&word.headword)),
                    "translate-headword",
                ),
                false,
                false,
                0,
            );
            if let Some(gloss) = &word.gloss {
                results.pack_start(
                    &translate_line(&escape_markup(gloss), "translate-gloss"),
                    false,
                    false,
                    0,
                );
            }

            if !word.pronunciations.is_empty() {
                let row = translate_row(6);
                for pronunciation in &word.pronunciations {
                    let cell = translate_row(2);
                    cell.pack_start(
                        &translate_inline(
                            &format!(
                                "<b>{}</b> {}",
                                escape_markup(&pronunciation.lang.to_uppercase()),
                                escape_markup(&pronunciation.ipa)
                            ),
                            "translate-ipa",
                        ),
                        false,
                        false,
                        0,
                    );
                    let speaker = small_button("\u{25b6}");
                    speaker.style_context().add_class("translate-speaker");
                    speaker.set_tooltip_text(Some("Play pronunciation"));
                    speaker.connect_clicked({
                        let audio_buttons = audio_buttons.clone();
                        let audio_tx = audio_tx.clone();
                        let url = pronunciation.audio_url.clone();
                        move |button| {
                            // Disabled until the clip lands, so a slow download
                            // cannot be queued up a dozen times.
                            button.set_sensitive(false);
                            audio_buttons
                                .borrow_mut()
                                .insert(url.clone(), button.clone());
                            crate::translate::spawn_audio(url.clone(), audio_tx.clone());
                        }
                    });
                    cell.pack_start(&speaker, false, false, 0);
                    row.pack_start(&cell, false, false, 0);
                }
                results.pack_start(&row, false, false, 0);
            }

            for entry in &word.vi_entries {
                let pos = if entry.pos.is_empty() {
                    "NGHĨA".to_owned()
                } else {
                    entry.pos.to_uppercase()
                };
                results.pack_start(
                    &translate_line(&escape_markup(&pos), "translate-pos"),
                    false,
                    false,
                    0,
                );
                for meaning in &entry.meanings {
                    results.pack_start(
                        &translate_line(
                            &format!("\u{2022} {}", escape_markup(meaning)),
                            "translate-meaning",
                        ),
                        false,
                        false,
                        0,
                    );
                }
            }

            for definition in &word.en_definitions {
                if !definition.pos.is_empty() {
                    results.pack_start(
                        &translate_line(
                            &escape_markup(&definition.pos.to_uppercase()),
                            "translate-pos",
                        ),
                        false,
                        false,
                        0,
                    );
                }
                results.pack_start(
                    &translate_line(&escape_markup(&definition.text), "translate-body"),
                    false,
                    false,
                    0,
                );
                for example in &definition.examples {
                    results.pack_start(
                        &translate_line(
                            &format!("<i>{}</i>", escape_markup(example)),
                            "translate-example",
                        ),
                        false,
                        false,
                        0,
                    );
                }
            }

            if !word.examples.is_empty() {
                results.pack_start(
                    &translate_line("EXAMPLES", "translate-pos"),
                    false,
                    false,
                    0,
                );
                for example in &word.examples {
                    results.pack_start(
                        &translate_line(&example.en_markup, "translate-example-en"),
                        false,
                        false,
                        0,
                    );
                    results.pack_start(
                        &translate_line(&escape_markup(&example.vi), "translate-example-vi"),
                        false,
                        false,
                        0,
                    );
                }
            }
        }
        ResultKind::NotFound { suggestions } => {
            results.pack_start(
                &translate_line(
                    &format!(
                        "No dictionary entry for <b>{}</b>",
                        escape_markup(&result.query)
                    ),
                    "translate-status",
                ),
                false,
                false,
                0,
            );
            if !suggestions.is_empty() {
                results.pack_start(
                    &translate_line("DID YOU MEAN", "translate-pos"),
                    false,
                    false,
                    0,
                );
                for word in suggestions {
                    results.pack_start(&translate_word_button(word, lookup), false, false, 0);
                }
            }
        }
        ResultKind::Error(message) => {
            results.pack_start(
                &translate_line(&escape_markup(message), "translate-status"),
                false,
                false,
                0,
            );
            results.pack_start(
                // The query panel is closed by now, so there is no caret to
                // press Enter in — point at the control that is actually there.
                &translate_line("Open search to try again.", "translate-source"),
                false,
                false,
                0,
            );
        }
    }
    // Freshly built widgets start hidden; without this the card renders empty.
    results.show_all();
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

/// Whatever text is currently selected anywhere on the desktop, flattened to a
/// single line. X11 keeps the primary selection up to date as text is
/// highlighted, so this needs no cooperation from the widget under the pointer.
fn primary_selection() -> String {
    gtk::Clipboard::get(&gdk::SELECTION_PRIMARY)
        .wait_for_text()
        .map(|text| text.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default()
}

/// Shorten a selection for display in a menu label, counting characters rather
/// than bytes so a Vietnamese phrase is never cut mid-codepoint.
fn ellipsize(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{}\u{2026}", kept.trim_end())
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

#[allow(clippy::too_many_arguments)]
fn attach_color_mode_menu(
    widget: &gtk::EventBox,
    key: String,
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
    timer_style: Option<TimerStylePreview>,
    system_details: Option<SystemDetailsPreview>,
    lookup: Option<LookupSlot>,
) {
    let menu = gtk::Menu::new();

    // Looking up the selection sits at the top, above the colour modes: it is
    // the one item that acts on what the user just highlighted rather than on
    // the widget itself. The label is rewritten and the row shown or hidden
    // each time the menu pops up, from whatever is in the X11 primary
    // selection — which is set by selecting text in any application, so this
    // works on a note, on a result, or on text selected in a browser.
    let selected: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let lookup_item = lookup.map(|lookup| {
        let item = gtk::MenuItem::with_label("LOOK UP");
        item.connect_activate({
            let lookup = lookup.clone();
            let selected = selected.clone();
            move |_| {
                let query = selected.borrow().clone();
                if query.is_empty() {
                    return;
                }
                // Cloned out of the borrow: the callback re-enters the UI, and
                // holding the slot borrowed across that would be a trap for
                // whoever next writes to it.
                let run = lookup.borrow().clone();
                if let Some(run) = run {
                    run(&query);
                }
            }
        });
        let separator = gtk::SeparatorMenuItem::new();
        menu.append(&item);
        menu.append(&separator);
        (item, separator)
    });
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
        if let Some((item, separator)) = &lookup_item {
            let query = primary_selection();
            // The separator belongs to the item; leaving it behind would open
            // every menu with a stray rule above the colour modes.
            item.set_visible(!query.is_empty());
            separator.set_visible(!query.is_empty());
            if !query.is_empty() {
                if let Some(label) = item.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                    label.set_label(&format!("LOOK UP  \u{201c}{}\u{201d}", ellipsize(&query, 22)));
                }
            }
            *selected.borrow_mut() = query;
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
        let lock_note = item.key.starts_with("note:")
            || item.key == "history"
            || item.key == "translate";
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
        cascade_point, ellipsize, fit_within_bounds, history_row_budget, image_room,
        image_room_after_y, monitor_coordinate_divisor, monitor_root_bounds,
        normalize_monitor_rect, note_headline, note_image_cap, note_size_for_image,
        parse_timer_input, push_recent_search, record_note_undo, reopen_point, resize_width_limit,
        resized_image_size, round_pixbuf_corners, system_content_size, timer_style_size,
        NoteSnapshot, NoteUndo, NoteUndoState, ScreenRect, HISTORY_HEIGHT,
        HISTORY_WIDTH, NOTE_HEIGHT, NOTE_IMAGE_BORDER_RADIUS, NOTE_IMAGE_DEFAULT_MAX,
        NOTE_IMAGE_MAX, NOTE_IMAGE_MIN, NOTE_MAX_HEIGHT, NOTE_WIDTH,
    };
    use crate::state::{NoteImage, Point, Size, SystemDetails, TimerStyle, IMAGE_PLACEHOLDER};
    use gdk_pixbuf::{Colorspace, Pixbuf};
    use std::{cell::RefCell, rc::Rc};

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
    fn a_pasted_screenshot_is_scaled_down_but_a_small_icon_is_left_alone() {
        // A 1920x1080 screenshot fits the note without distorting its shape.
        let (width, height) =
            fit_within_bounds(1920, 1080, NOTE_IMAGE_DEFAULT_MAX, NOTE_IMAGE_DEFAULT_MAX);
        assert_eq!(width, NOTE_IMAGE_DEFAULT_MAX);
        assert_eq!(height, 135);
        // A tall image is bounded by its height, not its width.
        assert_eq!(fit_within_bounds(200, 1000, 240, 240), (48, 240));
        // Nothing smaller than the cap is ever blown up.
        assert_eq!(fit_within_bounds(60, 40, 240, 240), (60, 40));
    }

    #[test]
    fn a_pasted_image_respects_available_width_and_height_independently() {
        // Near a screen edge, a 16:9 image may use more horizontal than
        // vertical room while remaining completely visible.
        assert_eq!(fit_within_bounds(1920, 1080, 220, 90), (160, 90));
        // A portrait image is bounded by height; it cannot put its bottom edge
        // below the note where the resize cursor would become unreachable.
        assert_eq!(fit_within_bounds(400, 1200, 240, 180), (60, 180));
    }

    #[test]
    fn image_resize_limit_accounts_for_the_notes_vertical_room() {
        let limit = Size {
            width: 540,
            height: 440,
        };
        let chrome = Size {
            width: 24,
            height: 48,
        };
        let room = image_room(limit, chrome);
        assert_eq!(
            room,
            Size {
                width: 516,
                height: 382
            }
        );
        // A 1:3 portrait is height-limited even though lots of width remains.
        assert_eq!(resize_width_limit(room, 1.0 / 3.0), 127);
        // A wide image remains width-limited.
        assert_eq!(resize_width_limit(room, 16.0 / 9.0), 516);
        assert_eq!(
            image_room_after_y(room, 42),
            Size {
                width: 516,
                height: 340
            }
        );
    }

    #[test]
    fn note_undo_records_mixed_text_and_image_edits_and_clears_redo() {
        let history: NoteUndoState = Rc::new(RefCell::new(NoteUndo::default()));
        let before = NoteSnapshot {
            text: "hello".into(),
            images: vec![],
            cursor: 5,
            size: Size {
                width: NOTE_WIDTH,
                height: NOTE_HEIGHT,
            },
        };
        let after = NoteSnapshot {
            text: format!("hello{IMAGE_PLACEHOLDER}"),
            images: vec![NoteImage {
                file: "1.png".into(),
                width: 200,
                height: 100,
            }],
            cursor: 6,
            size: Size {
                width: 224,
                height: 150,
            },
        };
        record_note_undo(&history, before.clone(), &after);
        assert_eq!(history.borrow().undo, vec![before]);
        history.borrow_mut().redo.push(after.clone());

        let typed = NoteSnapshot {
            text: format!("hello{IMAGE_PLACEHOLDER}!"),
            cursor: 7,
            ..after.clone()
        };
        record_note_undo(&history, after.clone(), &typed);
        let history = history.borrow();
        assert_eq!(history.undo.last(), Some(&after));
        assert!(history.redo.is_empty());
    }

    #[test]
    fn a_pasted_image_fits_the_note_it_lands_in() {
        // A default-width note: the image must stay inside it, or its resize
        // resize edge is clipped away with the image edge.
        let cap = note_image_cap(NOTE_WIDTH - 10);
        assert!(cap <= NOTE_WIDTH - 10);
        assert_eq!(fit_within_bounds(640, 360, cap, cap).0, cap);
        // A note dragged wide still pastes at the default size, not wall-sized.
        assert_eq!(note_image_cap(900), NOTE_IMAGE_DEFAULT_MAX);
        // A note dragged to its narrowest still shows something.
        assert_eq!(note_image_cap(4), NOTE_IMAGE_MIN);
    }

    #[test]
    fn a_note_grows_around_a_pasted_image_so_the_edge_stays_reachable() {
        let current = Size {
            width: NOTE_WIDTH,
            height: NOTE_HEIGHT,
        };
        // A default note shows far less than a pasted screenshot's height, so
        // it has to grow or the resize edge is clipped away below the fold.
        let chrome = Size {
            width: 24,
            height: 48,
        };
        let image = Size {
            width: 208,
            height: 117,
        };
        let limit = Size {
            width: NOTE_MAX_HEIGHT,
            height: NOTE_MAX_HEIGHT,
        };
        let grown = note_size_for_image(current, chrome, image, limit);
        assert!(grown.height >= image.height + chrome.height);
        assert!(grown.width >= current.width);

        // A note already big enough is left exactly as the user sized it.
        let roomy = Size {
            width: 400,
            height: 400,
        };
        assert_eq!(note_size_for_image(roomy, chrome, image, limit), roomy);

        // Growth never exceeds what the note is allowed to be, even for an
        // image bigger than the cap.
        let tight = Size {
            width: 260,
            height: 200,
        };
        let grown = note_size_for_image(current, chrome, image, tight);
        assert!(grown.width <= tight.width && grown.height <= tight.height);
    }

    #[test]
    fn dragging_the_image_corner_keeps_the_aspect_and_stays_within_bounds() {
        let aspect = 240.0 / 135.0;
        let room = NOTE_IMAGE_MAX;
        let (width, height) = resized_image_size(240, aspect, 60.0, 0.0, room);
        assert_eq!(width, 300);
        assert_eq!(height, 169);
        // Dragging down enlarges just as readily as dragging right.
        let (tall_width, _) = resized_image_size(240, aspect, 0.0, 60.0, room);
        assert!(tall_width > 240);
        // The image can never be dragged away to nothing or past the cap.
        assert_eq!(
            resized_image_size(240, aspect, -5000.0, 0.0, room).0,
            NOTE_IMAGE_MIN
        );
        assert_eq!(
            resized_image_size(240, aspect, 9000.0, 0.0, room).0,
            NOTE_IMAGE_MAX
        );
        // It also cannot be dragged wider than the note can show, which is what
        // keeps the resize edge reachable.
        assert_eq!(resized_image_size(240, aspect, 9000.0, 0.0, 300).0, 300);
        // A degenerate aspect ratio must not produce a zero or NaN size.
        assert_eq!(resized_image_size(120, 0.0, 10.0, 0.0, room), (130, 130));
    }

    #[test]
    fn a_note_image_loses_its_sharp_corners_on_every_side() {
        // The focus outline is drawn after the text and cannot erase what is
        // under it, so the corners have to be gone from the image itself.
        let pixbuf = Pixbuf::new(Colorspace::Rgb, true, 8, 60, 40).expect("test pixbuf");
        pixbuf.fill(0xff_00_00_ff);
        let rounded = round_pixbuf_corners(&pixbuf, NOTE_IMAGE_BORDER_RADIUS)
            .expect("rounding a pixbuf must succeed");
        assert_eq!((rounded.width(), rounded.height()), (60, 40));

        let bytes = rounded.read_pixel_bytes();
        let stride = rounded.rowstride() as usize;
        let channels = rounded.n_channels() as usize;
        let alpha =
            |x: i32, y: i32| -> u8 { bytes[y as usize * stride + x as usize * channels + 3] };
        for (x, y) in [(0, 0), (59, 0), (0, 39), (59, 39)] {
            assert_eq!(alpha(x, y), 0, "corner {x},{y} must be cut away");
        }
        // Only the corners: the edges and the middle stay fully opaque.
        assert_eq!(alpha(30, 0), 255);
        assert_eq!(alpha(30, 39), 255);
        assert_eq!(alpha(0, 20), 255);
        assert_eq!(alpha(59, 20), 255);
        assert_eq!(alpha(30, 20), 255);
    }

    #[test]
    fn a_note_headline_hides_image_placeholders() {
        // The placeholder is invisible in a label, so it must not become the
        // headline on its own.
        assert_eq!(note_headline("\u{fffc}"), "Image");
        assert_eq!(note_headline("\u{fffc}\nshopping list"), "shopping list");
        assert_eq!(note_headline("plan \u{fffc} b"), "plan  b");
        assert_eq!(note_headline(""), "Untitled note");
        assert_eq!(note_headline("  \n first line "), "first line");
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

    #[test]
    fn a_repeated_search_moves_to_the_front_instead_of_being_listed_twice() {
        let mut recents = vec!["beta".to_owned(), "alpha".to_owned()];
        assert!(push_recent_search(&mut recents, "alpha", 10));
        assert_eq!(recents, ["alpha", "beta"]);
        // Case and spacing differences are the same search.
        assert!(push_recent_search(&mut recents, "  BETA  ", 10));
        assert_eq!(recents, ["BETA", "alpha"]);
    }

    #[test]
    fn searching_the_same_thing_twice_running_changes_nothing() {
        // The caller only writes to disk when this reports a change.
        let mut recents = vec!["alpha".to_owned()];
        assert!(!push_recent_search(&mut recents, "alpha", 10));
        assert!(!push_recent_search(&mut recents, "   ", 10));
        assert!(!push_recent_search(&mut recents, "beta", 0));
        assert_eq!(recents, ["alpha"]);
    }

    #[test]
    fn the_recent_list_stops_growing_at_its_limit() {
        let mut recents = Vec::new();
        for index in 0..12 {
            assert!(push_recent_search(&mut recents, &format!("word{index}"), 10));
        }
        assert_eq!(recents.len(), 10);
        // Newest first, oldest dropped off the end.
        assert_eq!(recents[0], "word11");
        assert_eq!(recents[9], "word2");
    }

    #[test]
    fn a_menu_label_is_shortened_without_splitting_a_character() {
        assert_eq!(ellipsize("short", 22), "short");
        assert_eq!(ellipsize("abcdefghij", 5), "abcde\u{2026}");
        // Counted in characters, so Vietnamese is never cut mid-codepoint.
        assert_eq!(ellipsize("nền tảng cho khách", 8), "nền tảng\u{2026}");
        assert_eq!(ellipsize("ab cdef", 3), "ab\u{2026}");
    }
}
