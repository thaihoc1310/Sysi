use crate::{
    platform,
    state::{
        AppState, ColorMode, DictionaryWindow, Note, NoteImage, Point, Size, SystemDetails,
        TimerStyle, IMAGE_PLACEHOLDER,
    },
    system::{SystemReadOptions, SystemReader, SystemSnapshot},
    translate,
};
use cairo::{Context, FontSlant, FontWeight, RectangleInt, Region};
use gdk::prelude::*;
use gdk_pixbuf::{InterpType, Pixbuf};
use gtk::prelude::*;
use regex::RegexBuilder;
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    f64::consts::{PI, TAU},
    fs,
    rc::{Rc, Weak},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// How much disk the pronunciation clips may keep between them. Roughly a
/// couple of thousand words, and nothing that is dropped costs more than one
/// re-download.
const AUDIO_CACHE_LIMIT: u64 = 64 * 1024 * 1024;
const SYSTEM_WIDTH: i32 = 196;
const SYSTEM_HEIGHT: i32 = 76;
const SYSTEM_SINGLE_WIDTH: i32 = 76;
/// One meter's share of a row. Two of them make up the classic CPU + RAM card.
const SYSTEM_METER_CELL: f64 = 94.0;
/// The ring inside that cell: where its centre sits, how far the arc is swept
/// from it, and the stroke that straddles the arc. Everything that measures the
/// card is derived from these, so the box can never drift from the paint.
const SYSTEM_METER_RING_CENTER_Y: f64 = 35.0;
const SYSTEM_METER_RING_RADIUS: f64 = 28.0;
const SYSTEM_METER_RING_STROKE: f64 = 6.5;
/// How far the painted ring reaches from its centre: the radius, plus the half
/// of the stroke that falls outside it.
const SYSTEM_METER_RING_EXTENT: f64 = SYSTEM_METER_RING_RADIUS + SYSTEM_METER_RING_STROKE / 2.0;
/// The box one ring paints into, stroke included. This is a fixed size: the
/// card responds to a drag by moving the rings apart, not by growing them.
const SYSTEM_METER_RING: f64 = 2.0 * SYSTEM_METER_RING_EXTENT;
/// The space a card left alone puts between its rings — one cell less one
/// ring, which is exactly how far apart the classic card's rings sit.
const SYSTEM_METER_GAP: f64 = SYSTEM_METER_CELL - SYSTEM_METER_RING;
/// How close together the rings may be squeezed before one of them is moved
/// down to the next row instead.
const SYSTEM_METER_GAP_MIN: f64 = 10.0;
const SYSTEM_METER_INK_TOP: f64 = SYSTEM_METER_RING_CENTER_Y - SYSTEM_METER_RING_EXTENT;
const SYSTEM_METER_INK_BOTTOM: f64 =
    SYSTEM_HEIGHT as f64 - (SYSTEM_METER_RING_CENTER_Y + SYSTEM_METER_RING_EXTENT);
/// Rings stay legible at this size, so a row never holds more than three.
const SYSTEM_METERS_PER_ROW: usize = 3;
/// Where a caption sits inside its ring, measured from the top of the meter.
/// Four above the ring's widest point, which is what buys the caption enough
/// clear room for a word as long as "NVIDIA".
const SYSTEM_METER_LABEL_BASELINE: f64 = 52.0;
/// How wide a caption can be at that baseline before it crosses the stroke:
/// the ring's inner edge is 18 either side of centre there, and a centred
/// caption has to clear both.
const SYSTEM_METER_LABEL_WIDTH: f64 = 34.0;
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
const TRANSLATE_EMPTY_HEIGHT: i32 = 44;
const TRANSLATE_RESULTS_MAX_HEIGHT: i32 = 520;
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
/// How many words one dictionary window remembers for back and forward.
const TRANSLATE_HISTORY_LIMIT: usize = 50;
/// Past this many dictionary windows a new lookup reuses the most recent one
/// rather than burying the desk in cards.
const TRANSLATE_WINDOW_LIMIT: usize = 8;
const RESIZE_HIT_SIZE: i32 = 18;

type CallbackSlot = Rc<RefCell<Option<Rc<dyn Fn()>>>>;
// The history row the pointer is on, or None. The history window's right-click
// menu reads it to know which note a delete applies to.
type HoveredRow = Rc<Cell<Option<u64>>>;
// The dictionary lookup, handed to context menus that are built before it
// exists. Filled in once during startup, like `CallbackSlot`.
type LookupSlot = Rc<RefCell<Option<Rc<dyn Fn(&str)>>>>;
/// A recursive lookup callback must be weak or the callback and its slot keep
/// an already-closed dictionary window alive forever.
type WeakLookupSlot = Rc<RefCell<Option<Weak<dyn Fn(&str)>>>>;
type SystemValues = Rc<RefCell<SystemSnapshot>>;
/// Opens a dictionary window, going straight to a word when one is given.
type SpawnDictionary = Rc<dyn Fn(Option<&str>)>;
/// Runs a query in one window. The flag says whether it joins that window's
/// back/forward history or is a step through it.
type RunLookup = Rc<dyn Fn(&str, bool)>;
/// Holds the closure that opens dictionary windows. The menus that offer it
/// are built while that closure is still being assembled.
type SpawnSlot = Rc<RefCell<Option<SpawnDictionary>>>;
/// One window's remembered answers. Stepping back through words already read
/// should be instant, and should still work once the network is gone.
type TranslateCache = Rc<RefCell<HashMap<String, translate::ContentResult>>>;

/// The two ways the right-click menu can run a lookup: in the window the click
/// came from -- or the most recently used one, for a widget that is not a
/// dictionary -- and in a window of its own.
#[derive(Clone)]
struct LookupActions {
    here: LookupSlot,
    new_window: LookupSlot,
    /// Opens an empty dictionary, with no selection involved.
    open_window: Option<NewWindowAction>,
}

/// "NEW WINDOW" on a dictionary's own menu. It is a title-bar action, so it is
/// offered only when the right-click actually landed on the header -- from the
/// middle of a definition it would just be clutter.
#[derive(Clone)]
struct NewWindowAction {
    spawn: SpawnSlot,
    header: gtk::EventBox,
}

/// One live dictionary window, as its neighbours need to see it.
#[derive(Clone)]
struct TranslateInstance {
    id: u64,
    window: Rc<TranslateWindow>,
    lookup: Rc<dyn Fn(&str)>,
    set_search: Rc<dyn Fn(bool)>,
    search_open: Rc<Cell<bool>>,
    /// Re-applies which arrows this window's history allows. show_all() on the
    /// card reveals both whatever the history says, so every path that shows a
    /// card has to run this afterwards.
    refresh_nav: Rc<dyn Fn()>,
    /// Answers this window has already shown, keyed by query. Emptied when the
    /// window is closed.
    cache: TranslateCache,
    /// Stops timers and the channel receiver, and breaks the one recursive
    /// lookup reference before the GTK widgets are destroyed.
    cleanup: Rc<dyn Fn()>,
}

/// What spawning a dictionary window needs from the rest of the overlay.
#[derive(Clone)]
struct TranslateContext {
    state: Rc<RefCell<AppState>>,
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    interactive: Rc<Cell<bool>>,
    window: gtk::ApplicationWindow,
    root: gtk::Fixed,
    screens: Rc<Vec<ScreenRect>>,
    primary: ScreenRect,
    /// Kept out from under a freshly placed card, like any reopened widget.
    picker: gtk::EventBox,
    instances: Rc<RefCell<Vec<TranslateInstance>>>,
    /// The window a lookup goes to when the click came from somewhere else.
    recent: Rc<Cell<Option<u64>>>,
    /// Every scroller whose thumb the hover poll should fade in.
    scrollers: Rc<RefCell<Vec<gtk::ScrolledWindow>>>,
    lookup_new_window: LookupSlot,
    spawn: SpawnSlot,
}

struct SystemCard {
    card: gtk::EventBox,
    drag: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    canvas: gtk::DrawingArea,
    values: SystemValues,
    details: Rc<Cell<SystemDetails>>,
    /// The last size the layout itself asked for. Compared against rather than
    /// against the allocation, so a card whose natural size never quite matches
    /// its request is not re-requested on every sample.
    auto_size: Rc<Cell<Option<Size>>>,
    /// Asks the sampler for a reading now. Filled in by `start_system_updates`,
    /// which runs after the details menu has already been attached.
    resample: CallbackSlot,
    resize: ResizeHandle,
}

#[derive(Clone)]
struct SystemDetailsPreview {
    card: gtk::EventBox,
    canvas: gtk::DrawingArea,
    values: SystemValues,
    details: Rc<Cell<SystemDetails>>,
    auto_size: Rc<Cell<Option<Size>>>,
    resample: CallbackSlot,
}

#[derive(Clone)]
struct ResizeHandle {
    hitbox: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
}

#[derive(Clone)]
struct ResizeBounds {
    min_width: i32,
    min_height: i32,
    max_width: i32,
    max_height: i32,
    aspect_ratio: Option<f64>,
    preserve_current_aspect: bool,
    /// The height a card of this width must have. Set on cards whose content
    /// reflows — widen the system card until its rings sit on one row and the
    /// rows it no longer needs have to go, rather than staying as blank space
    /// the drag has no way to take back.
    height_for_width: Option<Rc<dyn Fn(i32) -> i32>>,
}

#[derive(Clone)]
struct RegisteredWidget {
    key: String,
    widget: gtk::EventBox,
    color_mode: Rc<Cell<ColorMode>>,
    edit_only: Option<gtk::EventBox>,
    editor: Option<gtk::TextView>,
    note_search: Option<NoteSearchControls>,
}

#[derive(Clone)]
struct NoteSearchControls {
    revealer: gtk::Revealer,
    entry: gtk::Entry,
    close: Rc<dyn Fn()>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct NoteSearchOptions {
    case_sensitive: bool,
    whole_word: bool,
    regular_expression: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NoteSearchMatch {
    start: i32,
    end: i32,
}

#[derive(Default)]
struct NoteSearchState {
    options: NoteSearchOptions,
    matches: Vec<NoteSearchMatch>,
    current: Option<usize>,
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
            // Frozen at what version 6 knew about: CPU and RAM were the only
            // two meters that existed when this migration was written.
            if usize::from(details.cpu) + usize::from(details.ram) == 1
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
        if data.layout_version < 7 {
            // The dictionary used to be a single window under the fixed key
            // "translate". Carry its place, size and colour over to the first
            // of the per-window keys so an upgrade finds it where it was left.
            if data.dictionaries.is_empty() {
                let key = dictionary_key(1);
                if let Some(point) = data.positions.remove("translate") {
                    data.positions.insert(key.clone(), point);
                }
                if let Some(size) = data.sizes.remove("translate") {
                    data.sizes.insert(key.clone(), size);
                }
                if let Some(mode) = data.widget_color_modes.remove("translate") {
                    data.widget_color_modes.insert(key, mode);
                }
                data.dictionaries.push(DictionaryWindow {
                    id: 1,
                    history: Vec::new(),
                    cursor: 0,
                });
                data.next_dictionary_id = 2;
            }
            data.layout_version = 7;
            let _ = data.save();
        }
        if data.layout_version < 8 {
            // The system card used to be measured to the cells its rings sit
            // in, which left a blank margin all the way round: the widget
            // stopped short of the left, right, and bottom edges of the screen
            // even when it was clamped flush against them. It is measured to
            // the rings themselves now, so a width saved under the old measure
            // has to hand those margins back — left alone it would keep the
            // gap it was dragged to.
            if let Some(size) = data.sizes.get("system").copied() {
                let columns = ((f64::from(size.width.max(1)) / SYSTEM_METER_CELL).floor()
                    as usize)
                    .max(1);
                data.sizes.insert(
                    "system".into(),
                    Size {
                        width: system_meter_ink_width(columns).ceil() as i32,
                        // The height follows from the width on its own, so
                        // whatever is here is replaced on the first layout.
                        height: size.height,
                    },
                );
            }
            data.layout_version = 8;
            let _ = data.save();
        }
    }
    // Image files left behind by a note deleted while Sysi was not running, or
    // by an image backspaced out of a note, are reclaimed once per launch. So
    // is anything past the pronunciation cache's ceiling.
    state.borrow().prune_orphan_images();
    translate::prune_audio_cache(AUDIO_CACHE_LIMIT);
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
    publish_panel_state(true, state.borrow().settings.color_mode);
    // Context menus are attached while their windows are built, well before the
    // dictionary lookup they call into exists; the slot is filled once both do.
    let lookup_slot: LookupSlot = Rc::new(RefCell::new(None));
    let lookup_new_slot: LookupSlot = Rc::new(RefCell::new(None));
    let lookup_actions = LookupActions {
        here: lookup_slot.clone(),
        new_window: lookup_new_slot.clone(),
        open_window: None,
    };
    // Grown as dictionary windows come and go, so the hover poll keeps up.
    let translate_scrollers: Rc<RefCell<Vec<gtk::ScrolledWindow>>> =
        Rc::new(RefCell::new(Vec::new()));
    let note_refresh: CallbackSlot = Rc::new(RefCell::new(None));
    // Which history row the pointer is on: the rows set it as they are entered
    // and left, and the history window's right-click menu reads it. Built here,
    // with the menu, because the menu is attached while the window is.
    let hovered_history_row: HoveredRow = Rc::new(Cell::new(None));
    let history_row_menu = build_history_row_menu(
        state.clone(),
        note_refresh.clone(),
        hovered_history_row.clone(),
    );
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
        system_card_size(system_details, &SystemSnapshot::default(), &state.borrow()),
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
            auto_size: system_card.auto_size.clone(),
            resample: system_card.resample.clone(),
        }),
        None,
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
            min_width: 62,
            min_height: 62,
            // Room for eight rings side by side, so a card dragged wide really
            // can put every meter on one row.
            max_width: system_meter_ink_width(8).ceil() as i32,
            max_height: 640,
            aspect_ratio: None,
            preserve_current_aspect: false,
            height_for_width: Some(Rc::new({
                let details = system_card.details.clone();
                let values = system_card.values.clone();
                move |width| {
                    system_content_size(details.get(), &values.borrow(), Some(width)).height
                }
            })),
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
            height_for_width: None,
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
        Some(lookup_actions.clone()),
        Some(history_row_menu),
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
            height_for_width: None,
        },
    );

    // Each dictionary is a window of its own, the way notes are, so several
    // words can sit open side by side. What a window needs from the rest of
    // the overlay is gathered once here instead of being threaded through
    // every spawn.
    let translate_ctx = TranslateContext {
        state: state.clone(),
        registry: registry.clone(),
        interactive: interactive.clone(),
        window: window.clone(),
        root: root.clone(),
        screens: Rc::new(screens.clone()),
        primary: primary_screen,
        picker: widget_picker.card.clone(),
        instances: Rc::new(RefCell::new(Vec::new())),
        recent: Rc::new(Cell::new(None)),
        scrollers: translate_scrollers.clone(),
        lookup_new_window: lookup_new_slot.clone(),
        spawn: Rc::new(RefCell::new(None)),
    };

    // Bring back the windows that were open last time, each showing the word it
    // was left on.
    for saved in state.borrow().dictionaries.clone() {
        spawn_translate_window(&translate_ctx, saved.id, false);
    }

    // Open a brand new dictionary. With a query it goes straight to the answer;
    // without one it comes up with the entry ready to type into.
    let translate_spawn: SpawnDictionary = {
        let ctx = translate_ctx.clone();
        let lock = widget_picker.lock.clone();
        Rc::new(move |query: Option<&str>| {
            if !ctx.interactive.get() {
                lock.clicked();
            }
            // Past the cap, reuse the most recent window rather than burying
            // the desk in cards.
            if ctx.instances.borrow().len() >= TRANSLATE_WINDOW_LIMIT {
                let recent = ctx
                    .recent
                    .get()
                    .and_then(|id| {
                        ctx.instances
                            .borrow()
                            .iter()
                            .find(|instance| instance.id == id)
                            .cloned()
                    })
                    .or_else(|| ctx.instances.borrow().last().cloned());
                if let Some(instance) = recent {
                    instance.window.card.show_all();
                    instance.window.chrome.set_visible(ctx.interactive.get());
                    (instance.refresh_nav)();
                    {
                        let mut data = ctx.state.borrow_mut();
                        data.settings.translate_open = true;
                        let _ = data.save();
                    }
                    match query {
                        Some(query) => (instance.lookup)(query),
                        None => (instance.set_search)(true),
                    }
                    refresh_input_shape(&ctx.window, &ctx.registry, ctx.interactive.get());
                    glib::idle_add_local_once({
                        let ctx = ctx.clone();
                        move || {
                            refresh_input_shape(&ctx.window, &ctx.registry, ctx.interactive.get());
                        }
                    });
                }
                return;
            }
            let id = {
                let mut data = ctx.state.borrow_mut();
                let id = data.next_dictionary_id;
                data.next_dictionary_id += 1;
                data.dictionaries.push(DictionaryWindow {
                    id,
                    history: Vec::new(),
                    cursor: 0,
                });
                data.settings.translate_open = true;
                let _ = data.save();
                id
            };
            spawn_translate_window(&ctx, id, true);
            let spawned = ctx
                .instances
                .borrow()
                .iter()
                .find(|instance| instance.id == id)
                .cloned();
            if let Some(instance) = spawned {
                instance.window.card.show_all();
                instance.window.chrome.set_visible(ctx.interactive.get());
                (instance.refresh_nav)();
                match query {
                    Some(query) => (instance.lookup)(query),
                    None => (instance.set_search)(true),
                }
            }
            refresh_input_shape(&ctx.window, &ctx.registry, ctx.interactive.get());
            glib::idle_add_local_once({
                let ctx = ctx.clone();
                move || {
                    clamp_registered_widgets(&ctx.root, &ctx.registry, &ctx.screens, &ctx.state);
                    refresh_input_shape(&ctx.window, &ctx.registry, ctx.interactive.get());
                }
            });
        })
    };

    *translate_ctx.spawn.borrow_mut() = Some(translate_spawn.clone());

    // "LOOK UP IN NEW WINDOW", offered from every widget's right-click menu.
    *lookup_new_slot.borrow_mut() = Some({
        let translate_spawn = translate_spawn.clone();
        Rc::new(move |query: &str| translate_spawn(Some(query))) as Rc<dyn Fn(&str)>
    });

    // Whether any dictionary is on screen, which is what the panel action and
    // the picker toggle read.
    let translate_any_visible: Rc<dyn Fn() -> bool> = {
        let instances = translate_ctx.instances.clone();
        Rc::new(move || {
            instances
                .borrow()
                .iter()
                .any(|instance| instance.window.card.is_visible())
        })
    };

    let translate_set_visible: Rc<dyn Fn(bool)> = {
        let ctx = translate_ctx.clone();
        Rc::new(move |visible: bool| {
            let instances: Vec<TranslateInstance> = ctx.instances.borrow().clone();
            for instance in &instances {
                if visible {
                    place_translate_near_click(&ctx, instance.id, &instance.window.card);
                    instance.window.card.show_all();
                    // show_all() reveals the chrome and the query panel
                    // regardless of lock mode; restore both rules.
                    instance.window.chrome.set_visible(ctx.interactive.get());
                    instance.search_open.set(false);
                    instance.window.set_search_visible(false);
                    (instance.refresh_nav)();
                } else {
                    instance.window.card.hide();
                }
            }
            {
                let mut data = ctx.state.borrow_mut();
                data.settings.translate_open = visible;
                let _ = data.save();
            }
            refresh_input_shape(&ctx.window, &ctx.registry, ctx.interactive.get());
            glib::idle_add_local_once({
                let ctx = ctx.clone();
                move || {
                    if visible {
                        clamp_registered_widgets(
                            &ctx.root,
                            &ctx.registry,
                            &ctx.screens,
                            &ctx.state,
                        );
                    }
                    refresh_input_shape(&ctx.window, &ctx.registry, ctx.interactive.get());
                }
            });
        })
    };

    // The picker's DICTIONARY button: with nothing open it starts a window,
    // otherwise it puts the whole set away and brings it back.
    let toggle_translate: Rc<dyn Fn()> = {
        let instances = translate_ctx.instances.clone();
        let set_visible = translate_set_visible.clone();
        let spawn = translate_spawn.clone();
        let any_visible = translate_any_visible.clone();
        Rc::new(move || {
            if instances.borrow().is_empty() {
                spawn(None);
                return;
            }
            set_visible(!any_visible());
        })
    };

    // Escape closes the query panel of whichever dictionary has one open,
    // before it is allowed to lock the overlay.
    let translate_close_search: Rc<dyn Fn() -> bool> = {
        let instances = translate_ctx.instances.clone();
        let recent = translate_ctx.recent.clone();
        Rc::new(move || {
            let open: Vec<TranslateInstance> = instances
                .borrow()
                .iter()
                .filter(|instance| instance.window.card.is_visible() && instance.search_open.get())
                .cloned()
                .collect();
            let Some(instance) = open
                .iter()
                .find(|instance| Some(instance.id) == recent.get())
                .or_else(|| open.first())
            else {
                return false;
            };
            (instance.set_search)(false);
            true
        })
    };

    // show_all() on the overlay drops every query panel down; a window restored
    // from the last session was not asked for just now.
    let translate_after_show: Rc<dyn Fn()> = {
        let ctx = translate_ctx.clone();
        Rc::new(move || {
            let open = ctx.state.borrow().settings.translate_open;
            let instances: Vec<TranslateInstance> = ctx.instances.borrow().clone();
            for instance in &instances {
                instance.search_open.set(false);
                instance.window.set_search_visible(false);
                (instance.refresh_nav)();
                if !open {
                    instance.window.card.hide();
                }
            }
        })
    };

    // A bare "LOOK UP" from a widget that is not itself a dictionary goes to
    // the window used most recently, or opens one when none are left.
    *lookup_slot.borrow_mut() = Some({
        let ctx = translate_ctx.clone();
        let spawn = translate_spawn.clone();
        let lock = widget_picker.lock.clone();
        Rc::new(move |query: &str| {
            if !ctx.interactive.get() {
                lock.clicked();
            }
            let target = ctx
                .recent
                .get()
                .and_then(|id| {
                    ctx.instances
                        .borrow()
                        .iter()
                        .find(|instance| instance.id == id)
                        .cloned()
                })
                .or_else(|| ctx.instances.borrow().last().cloned());
            let Some(instance) = target else {
                spawn(Some(query));
                return;
            };
            if !instance.window.card.is_visible() {
                instance.window.card.show_all();
                instance.window.chrome.set_visible(ctx.interactive.get());
                (instance.refresh_nav)();
                let mut data = ctx.state.borrow_mut();
                data.settings.translate_open = true;
                let _ = data.save();
            }
            (instance.lookup)(query);
        }) as Rc<dyn Fn(&str)>
    });

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
        let hovered_history_row = hovered_history_row.clone();
        Rc::new(move || {
            rebuild_note_list(
                &list,
                &root,
                state.clone(),
                note_refresh.clone(),
                &search.text(),
                history_limit.get(),
                hovered_history_row.clone(),
            );
            rebuild_pinned_notes(
                &root,
                state.clone(),
                registry.clone(),
                note_refresh.clone(),
                interactive.clone(),
                window.clone(),
                lookup_actions.clone(),
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
        let hovered_history_row = hovered_history_row.clone();
        Rc::new(move || {
            rebuild_note_list(
                &list,
                &root,
                state.clone(),
                note_refresh.clone(),
                &search.text(),
                history_limit.get(),
                hovered_history_row.clone(),
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
        let interactive = interactive.clone();
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
            publish_panel_state(interactive.get(), next);
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
            let size = Size {
                width: NOTE_WIDTH,
                height: NOTE_HEIGHT,
            };
            let position = match reopen_anchor() {
                Some(pointer) => reopen_point(
                    Some(pointer),
                    size,
                    &screens,
                    primary_screen,
                    widget_rect(&picker),
                ),
                // No pointer to go on, so anchor to the picker instead.
                None => clamp_to_screens(
                    Point {
                        x: allocation.x() + 205,
                        y: allocation.y() + 40,
                    },
                    NOTE_WIDTH,
                    NOTE_HEIGHT,
                    &screens,
                ),
            };
            let mut data = state.borrow_mut();
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
        let state = state.clone();
        let commit_timer_edit = timer_card.commit_edit.clone();
        Rc::new(move || {
            let enabled = !interactive.get();
            interactive.set(enabled);
            if enabled {
                window.set_accept_focus(true);
                window.style_context().add_class("editing");
            } else {
                commit_timer_edit();
                let open_searches: Vec<NoteSearchControls> = registry
                    .borrow()
                    .iter()
                    .filter_map(|item| item.note_search.clone())
                    .filter(|search| search.revealer.reveals_child())
                    .collect();
                for search in open_searches {
                    (search.close)();
                }
                window.set_accept_focus(false);
                window.style_context().remove_class("editing");
            }
            lock.set_label(if enabled { "LOCK" } else { "UNLOCK" });
            publish_panel_state(enabled, state.borrow().settings.color_mode);
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
        let translate_any_visible = translate_any_visible.clone();
        let interactive = interactive.clone();
        Rc::new(move || {
            for action in take_panel_actions() {
                // Held only for as long as the action runs, so a widget opened
                // any other way still goes by the pointer.
                PANEL_ANCHOR.with(|cell| cell.set(action.anchor));
                match action.name.as_str() {
                    "toggle-system" => system.set_active(!system.is_active()),
                    "toggle-timer" => timer.set_active(!timer.is_active()),
                    "next-color-mode" => mode.clicked(),
                    "toggle-lock" => lock.clicked(),
                    "new-note" => {
                        // A note created while locked would be read-only;
                        // unlock so the user can type into it right away.
                        if !interactive.get() {
                            lock.clicked();
                        }
                        new_note.clicked();
                    }
                    "toggle-history" => toggle_history(),
                    "toggle-translate" => {
                        // The entry is edit chrome, so a translate window
                        // opened while locked would have nothing to type into;
                        // unlock first, the way a new note does.
                        if !translate_any_visible() && !interactive.get() {
                            lock.clicked();
                        }
                        toggle_translate();
                    }
                    "quit" => quit.clicked(),
                    _ => {}
                }
                PANEL_ANCHOR.with(|cell| cell.set(None));
            }
        })
    };

    window.connect_key_press_event({
        let toggle_action = toggle_action.clone();
        let interactive = interactive.clone();
        let registry = registry.clone();
        let searching = searching.clone();
        let history_card = history.card.clone();
        let close_history_search = close_history_search.clone();
        let translate_close_search = translate_close_search.clone();
        move |_, event| {
            if event.keyval() == gdk::keys::constants::Escape {
                let open_note_search = registry
                    .borrow()
                    .iter()
                    .filter_map(|item| item.note_search.clone())
                    .find(|search| search.revealer.reveals_child());
                if let Some(search) = open_note_search {
                    (search.close)();
                    return glib::Propagation::Stop;
                }
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
                if interactive.get() && translate_close_search() {
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
    translate_after_show();
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

    translate_scrollers
        .borrow_mut()
        .push(history.scroller.clone());
    track_widget_hover(registry.clone(), translate_scrollers.clone());
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
        auto_size: Rc::new(Cell::new(None)),
        resample: Rc::new(RefCell::new(None)),
        resize,
    }
}

fn start_system_updates(system: SystemCard, state: Rc<RefCell<AppState>>) {
    // /proc is cheap most of the time, but TOP PROCESSES walks every PID and
    // NVIDIA sampling starts a helper process. Keep all of it off GTK's main
    // loop so a slow driver or a machine with many processes cannot freeze the
    // overlay every two seconds.
    let (request_tx, request_rx) = async_channel::bounded::<SystemReadOptions>(1);
    let (snapshot_tx, snapshot_rx) = async_channel::bounded::<SystemSnapshot>(1);
    let _ = std::thread::Builder::new()
        .name("sysi-system-sampler".into())
        .spawn(move || {
            let mut reader = SystemReader::default();
            while let Ok(options) = request_rx.recv_blocking() {
                if snapshot_tx.send_blocking(reader.read(options)).is_err() {
                    break;
                }
            }
        });

    glib::MainContext::default().spawn_local({
        let canvas = system.canvas.clone();
        let values = system.values.clone();
        let card = system.card.clone();
        let state = state.clone();
        let details = system.details.clone();
        let auto_size = system.auto_size.clone();
        async move {
            while let Ok(snapshot) = snapshot_rx.recv().await {
                // Re-fit whenever what the card has to show changes: a core
                // grid that grew a row, or a GPU that only appeared once the
                // first sample came back. The width the user dragged to is
                // kept; the height that follows from it is not theirs to set.
                let desired = system_card_size(details.get(), &snapshot, &state.borrow());
                if auto_size.get() != Some(desired) {
                    auto_size.set(Some(desired));
                    card.set_size_request(desired.width, desired.height);
                    card.queue_resize();
                    // Keep the stored height honest too, or the next launch
                    // lays the card out at a height it will only correct once
                    // the first sample comes back.
                    let mut data = state.borrow_mut();
                    if data
                        .sizes
                        .get("system")
                        .is_some_and(|stored| stored.height != desired.height)
                    {
                        data.sizes.insert("system".into(), desired);
                        let _ = data.save();
                    }
                }
                *values.borrow_mut() = snapshot;
                canvas.queue_draw();
            }
        }
    });

    let request: Rc<dyn Fn()> = Rc::new({
        let details = system.details.clone();
        move || {
            let details = details.get();
            let _ = request_tx.try_send(SystemReadOptions {
                processes: details.processes,
                cores: details.cores,
                gpus: details.gpus,
                root_disk: details.root_disk,
                home_disk: details.home_disk,
            });
        }
    });
    request();
    // Toggling a section on should not leave the card a sample behind, so the
    // details menu gets a way to ask for one straight away.
    *system.resample.borrow_mut() = Some(request.clone());
    glib::timeout_add_local(Duration::from_secs(2), move || {
        request();
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
    let meters = system_meters(details, values);
    let rows = system_meter_rows(meters.len(), system_meter_columns(meters.len(), width));
    let widest = rows.iter().copied().max().unwrap_or(0);
    // The rings keep their size and the room left over becomes the space
    // between them, so dragging the card spreads or tightens the row rather
    // than resizing it. Only a card too narrow for a single ring scales.
    let gap = system_meter_gap(widest, width);
    let content_width = system_meter_row_width(widest, gap);
    let scale = (width / content_width).clamp(0.1, 1.0);
    let (ink, muted, accent) = match color_mode {
        ColorMode::Light => ((0.97, 0.97, 0.97), (0.72, 0.72, 0.72), (0.9, 0.9, 0.9)),
        ColorMode::Gray => ((0.7, 0.7, 0.7), (0.5, 0.5, 0.5), (0.64, 0.64, 0.64)),
        ColorMode::Dark => ((0.08, 0.08, 0.08), (0.24, 0.24, 0.24), (0.14, 0.14, 0.14)),
    };
    if !meters.is_empty() {
        let _ = ctx.save();
        ctx.translate(
            (width - content_width * scale) / 2.0,
            -SYSTEM_METER_INK_TOP * scale,
        );
        ctx.scale(scale, scale);
        let mut meters = meters.iter();
        for (row, count) in rows.iter().copied().enumerate() {
            let y = row as f64 * f64::from(SYSTEM_HEIGHT);
            // Every row is centred on the card, so a last row that came up
            // short sits under the middle of the one above it.
            let left = (content_width - system_meter_row_width(count, gap)) / 2.0;
            for column in 0..count {
                let Some((value, title)) = meters.next() else {
                    break;
                };
                let x = left
                    + column as f64 * (SYSTEM_METER_RING + gap)
                    + SYSTEM_METER_RING / 2.0;
                ctx.set_line_width(SYSTEM_METER_RING_STROKE);
                ctx.set_line_cap(cairo::LineCap::Round);
                ctx.set_source_rgba(muted.0, muted.1, muted.2, 0.22);
                ctx.new_sub_path();
                ctx.arc(
                    x,
                    y + SYSTEM_METER_RING_CENTER_Y,
                    SYSTEM_METER_RING_RADIUS,
                    -PI * 0.75,
                    PI * 0.75,
                );
                let _ = ctx.stroke();
                ctx.set_source_rgba(accent.0, accent.1, accent.2, 0.96);
                ctx.new_sub_path();
                ctx.arc(
                    x,
                    y + SYSTEM_METER_RING_CENTER_Y,
                    SYSTEM_METER_RING_RADIUS,
                    -PI * 0.75,
                    -PI * 0.75 + PI * 1.5 * (value / 100.0).clamp(0.0, 1.0),
                );
                let _ = ctx.stroke();
                center_text(
                    ctx,
                    x,
                    y + 37.0,
                    &format!("{value:.0}%"),
                    18.0,
                    FontWeight::Bold,
                    ink,
                );
                center_text_fitted(
                    ctx,
                    x,
                    y + SYSTEM_METER_LABEL_BASELINE,
                    title,
                    8.5,
                    SYSTEM_METER_LABEL_WIDTH,
                    FontWeight::Bold,
                    muted,
                );
            }
        }
        let _ = ctx.restore();
    }
    let mut cursor_y = if rows.is_empty() {
        2.0
    } else {
        (rows.len() as f64 * f64::from(SYSTEM_HEIGHT) - SYSTEM_METER_INK_TOP) * scale
    };
    if details.processes {
        draw_system_processes(ctx, values, width, cursor_y, ink, muted);
        cursor_y += 108.0;
    }
    if details.cores {
        draw_system_cores(ctx, &values.cores, width, cursor_y, ink, muted, accent);
    }
}

/// Every ring the card shows, in the order they are laid out. Both the drawing
/// and the sizing read this one list, so what is measured is always what ends
/// up on screen — a machine with no NVIDIA card contributes no ring and no row.
fn system_meters(details: SystemDetails, values: &SystemSnapshot) -> Vec<(f64, String)> {
    let mut meters = Vec::new();
    if details.cpu {
        meters.push((values.cpu_percent, "CPU".into()));
    }
    if details.ram {
        meters.push((values.memory_percent, "RAM".into()));
    }
    if details.gpus {
        meters.extend(
            values
                .gpus
                .iter()
                .map(|gpu| (gpu.percent, gpu.label.clone())),
        );
    }
    if details.root_disk {
        if let Some(percent) = values.root_disk_percent {
            meters.push((percent, "ROOT".into()));
        }
    }
    if details.home_disk {
        if let Some(percent) = values.home_disk_percent {
            meters.push((percent, "HOME".into()));
        }
    }
    meters
}

/// How many rings a card this wide can put side by side. The card reflows: drag
/// it out and six meters end up on one row, pull it in and they stack. Never
/// more columns than there are meters, and never fewer than one — below a
/// single cell the rings scale down instead of disappearing.
fn system_meter_columns(count: usize, card_width: f64) -> usize {
    if count == 0 {
        return 0;
    }
    // Fewest rows first, so the widest row the card can still hold wins. A row
    // is held as long as its rings fit at their tightest spacing; squeeze the
    // card past that and the search drops to one more row, which is what moves
    // a ring down. Widen it back and the same test brings the ring up again.
    for rows in 1..=count {
        let widest = count.div_ceil(rows);
        if system_meter_row_width(widest, SYSTEM_METER_GAP_MIN) <= card_width {
            return widest;
        }
    }
    1
}

/// How wide a row of rings is at a given spacing.
fn system_meter_row_width(columns: usize, gap: f64) -> f64 {
    let columns = columns.max(1);
    columns as f64 * SYSTEM_METER_RING + (columns - 1) as f64 * gap
}

/// The spacing the rings take on a card this wide: whatever is left once the
/// rings themselves are accounted for, shared equally between them. This is
/// what lets the card answer a drag smoothly — the row keeps filling the card
/// until it is too tight to hold, and only then does the layout change.
fn system_meter_gap(widest: usize, card_width: f64) -> f64 {
    if widest <= 1 {
        return 0.0;
    }
    ((card_width - widest as f64 * SYSTEM_METER_RING) / (widest - 1) as f64)
        .max(SYSTEM_METER_GAP_MIN)
}

/// How many meters go on each row, given how many fit across. Rows divide as
/// evenly as they can, so four rings over two rows read as two and two rather
/// than three and a lone one.
fn system_meter_rows(count: usize, columns: usize) -> Vec<usize> {
    if count == 0 || columns == 0 {
        return Vec::new();
    }
    let rows = count.div_ceil(columns);
    let per_row = count / rows;
    let leftover = count % rows;
    (0..rows)
        .map(|row| per_row + usize::from(row < leftover))
        .collect()
}

/// The width the rings are laid out in, before the card scales them to fit.
/// `widest` is the busiest row's meter count.
/// The width the card wants when nobody has dragged it: its rings at the
/// spacing they take by default, and no margin beyond the outermost strokes.
fn system_meter_ink_width(widest: usize) -> f64 {
    system_meter_row_width(widest, SYSTEM_METER_GAP).max(1.0)
}

/// The size the card wants.
///
/// `card_width` is the width the user dragged it to, when they have dragged it.
/// The height is always ours: every part of this card stacks at a fixed height,
/// so a card widened until its rings fit on one row has to give the rows it no
/// longer needs back rather than keep them as blank space.
fn system_content_size(
    details: SystemDetails,
    values: &SystemSnapshot,
    card_width: Option<i32>,
) -> Size {
    let meter_count = system_meters(details, values).len();
    let columns = match card_width {
        Some(width) => system_meter_columns(meter_count, f64::from(width.max(1))),
        // Nothing to reflow into yet, so fall back to the default shape.
        None => meter_count.min(SYSTEM_METERS_PER_ROW),
    };
    let rows = system_meter_rows(meter_count, columns);
    let mut height = if rows.is_empty() {
        10
    } else {
        // The margin above the first row is always slack. The one below the
        // last row is only slack when the rings are what the card ends with;
        // with a section under them it is the gap that separates the two.
        let block = rows.len() as f64 * f64::from(SYSTEM_HEIGHT) - SYSTEM_METER_INK_TOP;
        let block = if details.processes || details.cores {
            block
        } else {
            block - SYSTEM_METER_INK_BOTTOM
        };
        block.ceil() as i32
    };
    if details.processes {
        height += 108;
    }
    if details.cores {
        height += 16 + (values.cores.len().max(1).div_ceil(4) as i32 * 17);
    }
    let width = match card_width {
        Some(width) => width,
        None if details.processes || details.cores => 318,
        // No meters at all still leaves a strip to right-click on.
        None if meter_count == 0 => SYSTEM_WIDTH,
        // Exactly the rings and nothing else, so the card can go flush against
        // a screen edge on every side the way it already could against the top.
        None => system_meter_ink_width(rows.iter().copied().max().unwrap_or(1)).ceil() as i32,
    };
    Size { width, height }
}

/// What the card should measure right now: the width the user chose if they
/// chose one, and a height that follows from how the rings reflow into it.
fn system_card_size(details: SystemDetails, values: &SystemSnapshot, state: &AppState) -> Size {
    system_content_size(
        details,
        values,
        state.sizes.get("system").map(|size| size.width),
    )
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

// Dropping a note takes its per-note layout keys with it, and any image file it
// was the last owner of, so nothing is left behind in state.json or on disk.
fn delete_note(state: &Rc<RefCell<AppState>>, id: u64) {
    let mut data = state.borrow_mut();
    data.notes.retain(|note| note.id != id);
    data.sizes.remove(&format!("note:{id}"));
    data.widget_color_modes.remove(&format!("note:{id}"));
    drop(data);
    let _ = state.borrow().save();
    state.borrow().prune_orphan_images();
}

// One menu serves the whole history list: it acts on whichever row the pointer
// is on, so a rebuild does not have to build (and leak) a menu per row. Returns
// a closure that pops it up and reports whether it did — the history window's
// colour-mode menu calls it first and stands down when a row claims the click.
fn build_history_row_menu(
    state: Rc<RefCell<AppState>>,
    refresh: CallbackSlot,
    hovered: HoveredRow,
) -> Rc<dyn Fn() -> bool> {
    let menu = context_menu();
    // The row the menu was opened on, latched at popup time. Reading `hovered`
    // when the item is activated would always come up empty: popping the menu
    // grabs the pointer, which leaves the row and clears it before the click.
    let target: HoveredRow = Rc::new(Cell::new(None));
    let delete = gtk::MenuItem::with_label("DELETE");
    // The one irreversible item in the app; it warms to red on hover.
    delete.style_context().add_class("menu-destructive");
    delete.connect_activate({
        let state = state.clone();
        let refresh = refresh.clone();
        let target = target.clone();
        move |_| {
            let Some(id) = target.take() else {
                return;
            };
            delete_note(&state, id);
            // Cloned out of the borrow: the refresh re-enters the UI and would
            // trip over the slot still being borrowed.
            let callback = refresh.borrow().clone();
            if let Some(callback) = callback {
                callback();
            }
        }
    });
    menu.append(&delete);
    menu.show_all();

    Rc::new(move || {
        let Some(id) = hovered.get() else {
            return false;
        };
        // A stale id (its row rebuilt away under the pointer) must not swallow
        // the click; let the colour-mode menu have it instead.
        if !state.borrow().notes.iter().any(|note| note.id == id) {
            return false;
        }
        target.set(Some(id));
        menu.popup_easy(3, gtk::current_event_time());
        true
    })
}

fn rebuild_note_list(
    list: &gtk::Box,
    root: &gtk::Fixed,
    state: Rc<RefCell<AppState>>,
    refresh: CallbackSlot,
    query: &str,
    limit: usize,
    hovered: HoveredRow,
) {
    for child in list.children() {
        list.remove(&child);
    }
    // Every row the pointer could have been on is gone; the fresh one under it
    // announces itself with its own enter event.
    hovered.set(None);
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
            hovered.clone(),
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
    hovered: HoveredRow,
) -> gtk::EventBox {
    let row = gtk::EventBox::new();
    // A GtkEventBox only paints its CSS background when it owns a window, so
    // the hover tint needs a visible one; the class stays transparent at rest.
    row.set_visible_window(true);
    row.style_context().add_class("note-preview");
    row.set_tooltip_text(Some(
        "Click or drag onto the desktop to pin  ·  right-click to delete",
    ));
    row.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK
            | gdk::EventMask::BUTTON1_MOTION_MASK
            | gdk::EventMask::BUTTON_RELEASE_MASK
            | gdk::EventMask::ENTER_NOTIFY_MASK
            | gdk::EventMask::LEAVE_NOTIFY_MASK,
    );
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    row.add(&label);

    // The pointer's row drives both the hover tint and what the history
    // window's right-click menu deletes, so one pair of handlers owns both.
    row.connect_enter_notify_event({
        let hovered = hovered.clone();
        move |row, _| {
            hovered.set(Some(note_id));
            row.style_context().add_class("note-preview-hover");
            if let Some(window) = row.window() {
                let cursor = gdk::Cursor::from_name(&window.display(), "pointer");
                window.set_cursor(cursor.as_ref());
            }
            glib::Propagation::Proceed
        }
    });
    row.connect_leave_notify_event({
        let hovered = hovered.clone();
        move |row, _| {
            // Guarded: moving between two rows can land the next row's enter
            // before this leave, and clearing then would lose the new row.
            if hovered.get() == Some(note_id) {
                hovered.set(None);
            }
            row.style_context().remove_class("note-preview-hover");
            if let Some(window) = row.window() {
                window.set_cursor(None);
            }
            glib::Propagation::Proceed
        }
    });

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

fn is_note_search_word_char(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn note_search_matches(
    text: &str,
    query: &str,
    options: NoteSearchOptions,
) -> Result<Vec<NoteSearchMatch>, regex::Error> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let pattern = if options.regular_expression {
        query.to_owned()
    } else {
        regex::escape(query)
    };
    let expression = RegexBuilder::new(&pattern)
        .case_insensitive(!options.case_sensitive)
        .unicode(true)
        .build()?;
    Ok(expression
        .find_iter(text)
        .filter(|found| {
            if !options.whole_word {
                return true;
            }
            let before = text[..found.start()].chars().next_back();
            let after = text[found.end()..].chars().next();
            !before.is_some_and(is_note_search_word_char)
                && !after.is_some_and(is_note_search_word_char)
        })
        .map(|found| NoteSearchMatch {
            // GtkTextBuffer offsets count Unicode characters, while regex
            // offsets count UTF-8 bytes. Convert both edges so Vietnamese and
            // every other multi-byte script highlight the right glyphs.
            start: text[..found.start()].chars().count() as i32,
            end: text[..found.end()].chars().count() as i32,
        })
        .collect())
}

fn clear_note_search_tags(
    buffer: &gtk::TextBuffer,
    match_tag: &gtk::TextTag,
    current_tag: &gtk::TextTag,
) {
    let (start, end) = buffer.bounds();
    buffer.remove_tag(match_tag, &start, &end);
    buffer.remove_tag(current_tag, &start, &end);
}

fn paint_note_search(
    editor: &gtk::TextView,
    count: &gtk::Label,
    match_tag: &gtk::TextTag,
    current_tag: &gtk::TextTag,
    state: &NoteSearchState,
    scroll: bool,
) {
    let Some(buffer) = editor.buffer() else {
        return;
    };
    clear_note_search_tags(&buffer, match_tag, current_tag);
    for found in &state.matches {
        buffer.apply_tag(
            match_tag,
            &buffer.iter_at_offset(found.start),
            &buffer.iter_at_offset(found.end),
        );
    }
    let Some(current) = state.current.and_then(|index| state.matches.get(index)) else {
        count.set_text("0/0");
        return;
    };
    let start = buffer.iter_at_offset(current.start);
    let end = buffer.iter_at_offset(current.end);
    buffer.apply_tag(current_tag, &start, &end);
    count.set_text(&format!(
        "{}/{}",
        state.current.unwrap_or_default() + 1,
        state.matches.len()
    ));
    if scroll {
        let mut target = start;
        editor.scroll_to_iter(&mut target, 0.18, false, 0.0, 0.0);
    }
}

fn set_note_search_toggle_open(button: &gtk::Button, open: bool) {
    if let Some(image) = button
        .child()
        .and_then(|child| child.downcast::<gtk::Image>().ok())
    {
        image.set_from_icon_name(
            Some(if open {
                "window-close-symbolic"
            } else {
                "edit-find-symbolic"
            }),
            gtk::IconSize::Menu,
        );
        image.set_pixel_size(11);
    }
    button.set_tooltip_text(Some(if open {
        "Close search (Esc)"
    } else {
        "Find in note (Ctrl+F)"
    }));
}

fn build_note_search(
    editor: &gtk::TextView,
    registry: &Rc<RefCell<Vec<RegisteredWidget>>>,
    toggle: &gtk::Button,
) -> NoteSearchControls {
    let revealer = gtk::Revealer::new();
    revealer.set_transition_type(gtk::RevealerTransitionType::SlideDown);
    revealer.set_transition_duration(120);
    revealer.set_reveal_child(false);

    let panel = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    panel.set_hexpand(true);
    panel.style_context().add_class("note-search-panel");

    let entry = gtk::Entry::new();
    entry.set_placeholder_text(Some("Find in note"));
    entry.set_width_chars(5);
    entry.set_hexpand(true);
    entry.style_context().add_class("note-search-entry");
    panel.pack_start(&entry, true, true, 0);

    let count = gtk::Label::new(Some("0/0"));
    count.set_width_chars(4);
    count.set_xalign(0.5);
    count.set_yalign(0.5);
    count.style_context().add_class("note-search-count");
    panel.pack_start(&count, false, false, 0);

    let previous = icon_button("go-up-symbolic", "Previous match (Shift+Enter)");
    let next = icon_button("go-down-symbolic", "Next match (Enter)");
    let settings = icon_button("emblem-system-symbolic", "Search options");
    // The popover itself explains this control. Leaving the tooltip armed
    // after the click lets it float above and cover the first checkbox.
    settings.set_tooltip_text(None);
    for button in [&previous, &next, &settings] {
        button.style_context().add_class("note-search-button");
    }
    panel.pack_start(&previous, false, false, 0);
    panel.pack_start(&next, false, false, 0);
    panel.pack_start(&settings, false, false, 0);
    revealer.add(&panel);

    let options_popover = gtk::Popover::new(Some(&settings));
    options_popover.set_position(gtk::PositionType::Bottom);
    options_popover
        .style_context()
        .add_class("note-search-popover");
    let options_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    options_box.set_border_width(3);
    let case_sensitive = gtk::CheckButton::with_label("Case sensitive");
    let whole_word = gtk::CheckButton::with_label("Match whole word only");
    let regular_expression = gtk::CheckButton::with_label("Regular expression");
    for checkbox in [&case_sensitive, &whole_word, &regular_expression] {
        checkbox.style_context().add_class("note-search-option");
        options_box.pack_start(checkbox, false, false, 0);
    }
    options_popover.add(&options_box);
    options_popover.show_all();
    options_popover.popdown();
    settings.connect_clicked({
        let options_popover = options_popover.clone();
        move |_| options_popover.popup()
    });

    let buffer = editor.buffer().expect("note search buffer");
    let match_tag = gtk::TextTag::new(Some("sysi-note-search-match"));
    match_tag.set_background_rgba(Some(&gdk::RGBA::new(0.98, 0.78, 0.20, 0.42)));
    let current_tag = gtk::TextTag::new(Some("sysi-note-search-current"));
    current_tag.set_background_rgba(Some(&gdk::RGBA::new(1.0, 0.52, 0.08, 0.78)));
    if let Some(table) = buffer.tag_table() {
        table.add(&match_tag);
        table.add(&current_tag);
    }

    let search_state = Rc::new(RefCell::new(NoteSearchState::default()));
    let refresh: Rc<dyn Fn(bool, bool)> = Rc::new({
        let editor = editor.clone();
        let entry = entry.clone();
        let count = count.clone();
        let match_tag = match_tag.clone();
        let current_tag = current_tag.clone();
        let search_state = search_state.clone();
        move |reset_current, scroll| {
            let Some(buffer) = editor.buffer() else {
                return;
            };
            let query = entry.text().to_string();
            let options = search_state.borrow().options;
            match note_search_matches(&note_buffer_text(&buffer), &query, options) {
                Ok(matches) => {
                    entry.style_context().remove_class("search-error");
                    entry.set_tooltip_text(None);
                    let mut state = search_state.borrow_mut();
                    state.matches = matches;
                    state.current = if state.matches.is_empty() {
                        None
                    } else if reset_current {
                        Some(0)
                    } else {
                        Some(state.current.unwrap_or(0).min(state.matches.len() - 1))
                    };
                    paint_note_search(&editor, &count, &match_tag, &current_tag, &state, scroll);
                }
                Err(error) => {
                    clear_note_search_tags(&buffer, &match_tag, &current_tag);
                    let mut state = search_state.borrow_mut();
                    state.matches.clear();
                    state.current = None;
                    count.set_text("ERR");
                    entry.style_context().add_class("search-error");
                    entry.set_tooltip_text(Some(&format!("Invalid regular expression: {error}")));
                }
            }
        }
    });

    entry.connect_changed({
        let refresh = refresh.clone();
        move |_| refresh(true, true)
    });
    buffer.connect_changed({
        let refresh = refresh.clone();
        let revealer = revealer.clone();
        move |_| {
            if revealer.reveals_child() {
                refresh(false, false);
            }
        }
    });

    case_sensitive.connect_toggled({
        let search_state = search_state.clone();
        let refresh = refresh.clone();
        move |checkbox| {
            search_state.borrow_mut().options.case_sensitive = checkbox.is_active();
            refresh(true, true);
        }
    });
    whole_word.connect_toggled({
        let search_state = search_state.clone();
        let refresh = refresh.clone();
        move |checkbox| {
            search_state.borrow_mut().options.whole_word = checkbox.is_active();
            refresh(true, true);
        }
    });
    regular_expression.connect_toggled({
        let search_state = search_state.clone();
        let refresh = refresh.clone();
        move |checkbox| {
            search_state.borrow_mut().options.regular_expression = checkbox.is_active();
            refresh(true, true);
        }
    });

    let navigate: Rc<dyn Fn(i32)> = Rc::new({
        let editor = editor.clone();
        let count = count.clone();
        let match_tag = match_tag.clone();
        let current_tag = current_tag.clone();
        let search_state = search_state.clone();
        move |direction| {
            let mut state = search_state.borrow_mut();
            let total = state.matches.len();
            if total == 0 {
                return;
            }
            let current = state.current.unwrap_or(0);
            state.current = Some(if direction < 0 {
                (current + total - 1) % total
            } else {
                (current + 1) % total
            });
            paint_note_search(&editor, &count, &match_tag, &current_tag, &state, true);
        }
    });
    previous.connect_clicked({
        let navigate = navigate.clone();
        move |_| navigate(-1)
    });
    next.connect_clicked({
        let navigate = navigate.clone();
        move |_| navigate(1)
    });

    let close: Rc<dyn Fn()> = Rc::new({
        let editor = editor.clone();
        let revealer = revealer.clone();
        let entry = entry.clone();
        let options_popover = options_popover.clone();
        let match_tag = match_tag.clone();
        let current_tag = current_tag.clone();
        let toggle = toggle.clone();
        move || {
            revealer.set_reveal_child(false);
            set_note_search_toggle_open(&toggle, false);
            options_popover.popdown();
            if let Some(buffer) = editor.buffer() {
                clear_note_search_tags(&buffer, &match_tag, &current_tag);
            }
            entry.style_context().remove_class("search-error");
            entry.set_tooltip_text(None);
            editor.grab_focus();
        }
    });

    let open: Rc<dyn Fn()> = Rc::new({
        let registry = registry.clone();
        let revealer = revealer.clone();
        let entry = entry.clone();
        let refresh = refresh.clone();
        let toggle = toggle.clone();
        move || {
            let other_searches: Vec<NoteSearchControls> = registry
                .borrow()
                .iter()
                .filter_map(|item| item.note_search.clone())
                .filter(|search| search.entry != entry && search.revealer.reveals_child())
                .collect();
            for search in other_searches {
                (search.close)();
            }
            revealer.set_reveal_child(true);
            set_note_search_toggle_open(&toggle, true);
            refresh(false, true);
            glib::idle_add_local_once({
                let entry = entry.clone();
                move || {
                    entry.grab_focus();
                    entry.select_region(0, -1);
                }
            });
        }
    });
    toggle.connect_clicked({
        let revealer = revealer.clone();
        let open = open.clone();
        let close = close.clone();
        move |_| {
            if revealer.reveals_child() {
                close();
            } else {
                open();
            }
        }
    });

    entry.connect_key_press_event({
        let entry = entry.clone();
        let close = close.clone();
        let navigate = navigate.clone();
        move |_, event| {
            let key = event.keyval();
            if key == gdk::keys::constants::Escape {
                close();
                return glib::Propagation::Stop;
            }
            let ctrl_f = event.state().contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::keys::constants::f | gdk::keys::constants::F);
            if ctrl_f {
                entry.select_region(0, -1);
                return glib::Propagation::Stop;
            }
            let activate = matches!(
                key,
                gdk::keys::constants::Return
                    | gdk::keys::constants::KP_Enter
                    | gdk::keys::constants::F3
            );
            if activate {
                navigate(if event.state().contains(gdk::ModifierType::SHIFT_MASK) {
                    -1
                } else {
                    1
                });
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    editor.connect_key_press_event({
        let revealer = revealer.clone();
        let open = open.clone();
        let navigate = navigate.clone();
        move |_, event| {
            let key = event.keyval();
            let ctrl_f = event.state().contains(gdk::ModifierType::CONTROL_MASK)
                && matches!(key, gdk::keys::constants::f | gdk::keys::constants::F);
            if ctrl_f {
                open();
                return glib::Propagation::Stop;
            }
            if key == gdk::keys::constants::F3 && revealer.reveals_child() {
                navigate(if event.state().contains(gdk::ModifierType::SHIFT_MASK) {
                    -1
                } else {
                    1
                });
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    NoteSearchControls {
        revealer,
        entry,
        close,
    }
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
    lookup: LookupActions,
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
        // Note rows are intentionally flush: the editor already owns its text
        // padding, and the find bar must not inherit CardBody's generic 4px
        // gap above and below it.
        body.set_spacing(0);
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
        // No delete button here on purpose: it sat next to the hide button, so
        // one stray click threw a note's text away for good. Deleting lives on
        // the history row's right-click menu instead, where it takes a
        // deliberate second click on a named item.
        let unpin = small_button("−");
        unpin.style_context().add_class("note-window-button");
        unpin.style_context().add_class("note-hide");
        unpin.set_tooltip_text(Some("Move to History"));
        let search_toggle = icon_button("edit-find-symbolic", "Find in note (Ctrl+F)");
        header.pack_start(&unpin, false, false, 0);
        header.pack_end(&search_toggle, false, false, 0);
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
        let note_search = build_note_search(&editor, &registry, &search_toggle);
        body.pack_start(&note_search.revealer, false, false, 0);
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
            item.note_search = Some(note_search);
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
                max_width: NOTE_MAX_WIDTH,
                max_height: NOTE_MAX_HEIGHT,
                aspect_ratio: None,
                preserve_current_aspect: false,
                height_for_width: None,
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
        card.show_all();
        header_drag.set_visible(interactive.get());
    }
}

fn track_widget_hover(
    registry: Rc<RefCell<Vec<RegisteredWidget>>>,
    scrollers: Rc<RefCell<Vec<gtk::ScrolledWindow>>>,
) {
    // GTK3 delivers enter/leave to the window under the pointer, so hovering
    // the editor (which owns its own window) never sets :hover on the note
    // card. Poll the pointer position cheaply — one timer serves both the
    // pinned notes and the history list — and toggle classes that fade the
    // scrollbar thumbs in instead of relying on CSS :hover propagation.
    glib::timeout_add_local(Duration::from_millis(100), move || {
        // Reading the pointer is a round trip to the X server. With no note and
        // no scrolling window on screen there is nothing that could take a
        // hover class, so the tick costs nothing at all.
        let hoverable = registry
            .borrow()
            .iter()
            .any(|item| item.key.starts_with("note:") && item.widget.is_visible())
            || scrollers
                .borrow()
                .iter()
                .any(|scroller| scroller.is_visible() && scroller.window().is_some());
        if !hoverable {
            return glib::ControlFlow::Continue;
        }
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
        for scroller in scrollers.borrow().iter() {
            let context = scroller.style_context();
            let hovered = scroller.window().is_some_and(|window| {
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
    // Width must remain unconstrained, but a fixed one-pixel height would also
    // become the natural height and clip every answer to a single line.
    scroller.set_size_request(1, -1);
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
    close: gtk::Button,
    back: gtk::Button,
    forward: gtk::Button,
    open_search: gtk::Button,
    close_search: gtk::Button,
    search_panel: gtk::Box,
    input: gtk::TextView,
    suggestions: gtk::Box,
    results: gtk::Box,
    scroller: gtk::ScrolledWindow,
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

fn dictionary_key(id: u64) -> String {
    format!("dict:{id}")
}

/// Write a window's browsing history back to disk. Cheap enough to do on every
/// move: the list is a handful of short words.
fn store_dictionary_history(
    state: &Rc<RefCell<AppState>>,
    id: u64,
    history: &[String],
    cursor: usize,
) {
    let mut data = state.borrow_mut();
    if let Some(entry) = data.dictionaries.iter_mut().find(|entry| entry.id == id) {
        entry.history = history.to_vec();
        entry.cursor = cursor;
    }
    let _ = data.save();
}

/// Take a dictionary window off the desk for good. Unlike hiding, this forgets
/// the window: its place, size and colour go with it, so the next one opens
/// fresh rather than inheriting a slot the user closed.
fn close_translate_window(ctx: &TranslateContext, id: u64) {
    let key = dictionary_key(id);
    let instance = ctx
        .instances
        .borrow()
        .iter()
        .find(|instance| instance.id == id)
        .cloned();
    let Some(instance) = instance else {
        return;
    };
    // Stop every callback which can still reach the card before destroying
    // it. In particular, a slow network reply must not render into children
    // that gtk_widget_destroy() has already torn down.
    (instance.cleanup)();
    instance.cache.borrow_mut().clear();
    let card = instance.window.card.clone();
    ctx.root.remove(&card);
    // The card's own handlers hold it, the same cycle the note rebuild breaks.
    // SAFETY: it has just been unparented and nothing reads it again.
    unsafe { card.destroy() };
    ctx.registry.borrow_mut().retain(|item| item.key != key);
    ctx.instances
        .borrow_mut()
        .retain(|instance| instance.id != id);
    ctx.scrollers
        .borrow_mut()
        .retain(|scroller| scroller.window().is_some());
    if ctx.recent.get() == Some(id) {
        ctx.recent.set(None);
    }
    {
        let mut data = ctx.state.borrow_mut();
        data.dictionaries.retain(|entry| entry.id != id);
        data.positions.remove(&key);
        data.sizes.remove(&key);
        data.widget_color_modes.remove(&key);
        if data.dictionaries.is_empty() {
            data.settings.translate_open = false;
        }
        let _ = data.save();
    }
    refresh_input_shape(&ctx.window, &ctx.registry, ctx.interactive.get());
}

/// Build one dictionary window and wire it to itself. Every window owns its
/// query panel, its own in-flight request counters and its own back/forward
/// history, so a slow lookup started in one can never land in another.
/// Put a dictionary window where the click that asked for it happened.
///
/// Opening a fresh one and bringing an existing one back both come through
/// here, so the panel's button lands them the same way either way. Left at its
/// saved position, a window put away in the corner of another monitor came
/// straight back to that corner, which is no answer to a click on the panel.
///
/// Deliberately no stepping clear of what is already open: that walks the card
/// down and away from the click, and two dictionaries overlapping is fine --
/// the new one is the one being read.
fn place_translate_near_click(ctx: &TranslateContext, id: u64, card: &gtk::EventBox) {
    let size = card_size(
        card,
        Size {
            width: TRANSLATE_WIDTH,
            height: TRANSLATE_EMPTY_HEIGHT,
        },
    );
    let point = reopen_point(
        reopen_anchor(),
        size,
        &ctx.screens,
        ctx.primary,
        widget_rect(&ctx.picker),
    );
    ctx.root.move_(card, point.x, point.y);
    ctx.state
        .borrow_mut()
        .positions
        .insert(dictionary_key(id), point);
}

fn spawn_translate_window(ctx: &TranslateContext, id: u64, near_pointer: bool) {
    let key = dictionary_key(id);
    let translate = Rc::new(build_translate_window(saved_color_mode(
        &ctx.state.borrow(),
        &key,
    )));
    // Closing the receiver wakes its task immediately. The flag also prevents
    // an event already queued before close from touching the destroyed GTK
    // children while the closed channel is being drained.
    let alive = Rc::new(Cell::new(true));
    let fit_height: Rc<dyn Fn()> = {
        let card = translate.card.clone();
        let chrome = translate.chrome.clone();
        let results = translate.results.clone();
        let alive = alive.clone();
        Rc::new(move || fit_translate_height(&card, &chrome, &results, &alive))
    };

    // The card joins the container first, whatever decides its place. Only
    // gtk_fixed_put() adds a child; gtk_fixed_move(), which reopen_widget uses
    // to cascade a window out from under the pointer, looks the widget up in
    // the child list and walks off the end of it -- taking the process with it
    // -- when handed a card that was never put there.
    let point = ctx
        .state
        .borrow()
        .positions
        .get(&key)
        .copied()
        .unwrap_or(Point {
            x: ctx.primary.x + 292,
            y: ctx.primary.y + 186,
        });
    place_card(&ctx.root, &translate.card, point);
    if near_pointer {
        place_translate_near_click(ctx, id, &translate.card);
    }
    apply_translate_elastic_size(&translate.card, &ctx.state);
    register(
        &ctx.registry,
        &key,
        &translate.card,
        translate.color_mode.clone(),
    );
    if let Some(item) = ctx
        .registry
        .borrow_mut()
        .iter_mut()
        .find(|item| item.key == key)
    {
        item.edit_only = Some(translate.chrome.clone());
    }
    attach_drag(
        &translate.header,
        &translate.card,
        &ctx.root,
        key.clone(),
        ctx.state.clone(),
        ctx.registry.clone(),
        ctx.interactive.clone(),
        ctx.window.clone(),
    );
    attach_resize(
        &translate.resize,
        &translate.card,
        &ctx.root,
        key.clone(),
        ctx.state.clone(),
        ctx.registry.clone(),
        ctx.interactive.clone(),
        ctx.window.clone(),
        ResizeBounds {
            min_width: 196,
            min_height: 120,
            max_width: 680,
            max_height: 860,
            aspect_ratio: None,
            preserve_current_aspect: false,
            height_for_width: None,
        },
    );

    // Lookups and completions run on worker threads and report back here. One
    // channel per window, so a reply is delivered to the window that asked for
    // it and nowhere else.
    let (tx, rx) = async_channel::unbounded::<translate::TranslateEvent>();
    // Two counters, not one: a lookup can be in flight for seconds while the
    // user types the next query, and a shared counter would let those
    // keystrokes retire the lookup — leaving "Looking it up…" on screen with
    // nothing left to replace it.
    // Shared with the worker threads, which read them to find out whether the
    // request they are part-way through is still the one the window wants.
    let lookup_generation = Arc::new(AtomicU64::new(0));
    let suggest_generation = Arc::new(AtomicU64::new(0));
    // The play buttons currently on screen, keyed by the clip they are waiting
    // for. Cleared on every rebuild, so a download that outlives its button
    // finds no one waiting and is neither re-enabled nor played.
    let audio_buttons: Rc<RefCell<HashMap<String, gtk::Button>>> =
        Rc::new(RefCell::new(HashMap::new()));
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    // Whether the query panel is dropped down. Tracked explicitly because
    // show_all() on the card would otherwise reveal it along with everything
    // else, exactly like the history window's search mode.
    let search_open = Rc::new(Cell::new(false));
    // The recents start folded away behind their arrow: opening the panel is a
    // move to type something, not to read the last ten things typed.
    let recents_open = Rc::new(Cell::new(false));

    let cache: TranslateCache = Rc::new(RefCell::new(HashMap::new()));
    // Rendering a cached answer needs the very closure that is being built, so
    // it goes through a slot filled in as soon as that closure exists.
    let lookup_self: WeakLookupSlot = Rc::new(RefCell::new(None));

    let saved = ctx
        .state
        .borrow()
        .dictionaries
        .iter()
        .find(|entry| entry.id == id)
        .cloned();
    let history_stack: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(
        saved
            .as_ref()
            .map(|entry| entry.history.clone())
            .unwrap_or_default(),
    ));
    let history_cursor = Rc::new(Cell::new(
        saved.as_ref().map(|entry| entry.cursor).unwrap_or(0),
    ));

    let update_nav: Rc<dyn Fn()> = {
        let back = translate.back.clone();
        let forward = translate.forward.clone();
        let stack = history_stack.clone();
        let cursor = history_cursor.clone();
        Rc::new(move || {
            let len = stack.borrow().len();
            let at = cursor.get();
            back.set_visible(at > 0);
            forward.set_visible(len > 0 && at + 1 < len);
        })
    };

    // Run a query. `record` is what separates following a new word from
    // stepping through words already visited: only the former rewrites history.
    let run_lookup: RunLookup = {
        let translate = translate.clone();
        let state = ctx.state.clone();
        let recent = ctx.recent.clone();
        let search_open = search_open.clone();
        let lookup_generation = lookup_generation.clone();
        let suggest_generation = suggest_generation.clone();
        let audio_buttons = audio_buttons.clone();
        let pending = pending.clone();
        let tx = tx.clone();
        let fit_height = fit_height.clone();
        let stack = history_stack.clone();
        let cursor = history_cursor.clone();
        let update_nav = update_nav.clone();
        let cache = cache.clone();
        let lookup_self = lookup_self.clone();
        Rc::new(move |query: &str, record: bool| {
            let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
            if query.is_empty() {
                return;
            }
            recent.set(Some(id));
            if record {
                {
                    let mut stack = stack.borrow_mut();
                    // Following a new word from part-way back drops whatever
                    // was ahead of it, exactly the way a browser does.
                    if !stack.is_empty() {
                        let keep = (cursor.get() + 1).min(stack.len());
                        stack.truncate(keep);
                    }
                    if stack.last().map(String::as_str) != Some(query.as_str()) {
                        stack.push(query.clone());
                        if stack.len() > TRANSLATE_HISTORY_LIMIT {
                            stack.remove(0);
                        }
                    }
                    cursor.set(stack.len().saturating_sub(1));
                }
                store_dictionary_history(&state, id, &stack.borrow(), cursor.get());
            }
            update_nav();
            // A completion that is still in flight would land on top of the
            // answer; cancel its timer and retire its generation.
            if let Some(source) = pending.borrow_mut().take() {
                source.remove();
            }
            suggest_generation.fetch_add(1, Ordering::Relaxed);
            // The answer is what the user wants to see now, so the panel gets
            // out of the way until they ask for it again.
            search_open.set(false);
            translate.set_search_visible(false);
            clear_children(&translate.suggestions);
            audio_buttons.borrow_mut().clear();
            lookup_generation.fetch_add(1, Ordering::Relaxed);
            // A fresh answer chooses its own natural height and starts at the
            // top even if the previous entry had been scrolled down.
            apply_translate_elastic_size(&translate.card, &state);
            translate.scroller.vadjustment().set_value(0.0);
            clear_children(&translate.results);
            let cached = cache.borrow().get(&query).cloned();
            if let Some(content) = cached {
                let result = translate::LookupResult {
                    query: query.clone(),
                    kind: translate::ResultKind::Content(Box::new(content)),
                };
                remember_search(&state, &result.query);
                // Cloned out of the borrow: rendering wires up rows that can
                // call straight back into this closure.
                let run = lookup_self.borrow().as_ref().and_then(Weak::upgrade);
                if let Some(run) = run {
                    render_translate_result(
                        &translate.results,
                        &fit_height,
                        &result,
                        &audio_buttons,
                        &tx,
                        &run,
                    );
                }
                return;
            }
            translate.results.pack_start(
                &translate_line("Looking it up\u{2026}", "translate-status"),
                false,
                false,
                0,
            );
            translate.results.show_all();
            fit_height();
            translate::spawn_lookup(
                query,
                translate::Request::new(
                    lookup_generation.clone(),
                    lookup_generation.load(Ordering::Relaxed),
                ),
                tx.clone(),
            );
        })
    };
    let lookup: Rc<dyn Fn(&str)> = {
        let run_lookup = run_lookup.clone();
        Rc::new(move |query: &str| run_lookup(query, true))
    };
    *lookup_self.borrow_mut() = Some(Rc::downgrade(&lookup));

    // Opening and closing the query panel, including the focus grab that lets
    // the user start typing the moment it appears.
    let set_search: Rc<dyn Fn(bool)> = {
        let translate = translate.clone();
        let search_open = search_open.clone();
        let state = ctx.state.clone();
        let window = ctx.window.clone();
        let lookup = lookup.clone();
        let suggest_generation = suggest_generation.clone();
        let pending = pending.clone();
        let recents_open = recents_open.clone();
        let fit_height = fit_height.clone();
        Rc::new(move |open: bool| {
            search_open.set(open);
            translate.set_search_visible(open);
            // Whichever way the panel moves, a completion request that has not
            // fired yet is no longer wanted.
            if let Some(source) = pending.borrow_mut().take() {
                source.remove();
            }
            suggest_generation.fetch_add(1, Ordering::Relaxed);
            if !open {
                clear_children(&translate.suggestions);
                fit_height();
                return;
            }
            // Start empty rather than pre-selecting the last query: selecting
            // text in a GtkTextView hands it the X11 primary selection, which
            // would clobber whatever the user had highlighted elsewhere — the
            // very thing the "LOOK UP" menu item reads.
            translate.set_query("");
            render_translate_recents(
                &translate.suggestions,
                &state,
                &lookup,
                &recents_open,
                &fit_height,
            );
            fit_height();
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

    translate.close.connect_clicked({
        let ctx = ctx.clone();
        move |_| close_translate_window(&ctx, id)
    });
    translate.open_search.connect_clicked({
        let set_search = set_search.clone();
        move |_| set_search(true)
    });
    translate.close_search.connect_clicked({
        let set_search = set_search.clone();
        move |_| set_search(false)
    });
    translate.back.connect_clicked({
        let stack = history_stack.clone();
        let cursor = history_cursor.clone();
        let run_lookup = run_lookup.clone();
        let state = ctx.state.clone();
        move |_| {
            if cursor.get() == 0 {
                return;
            }
            let at = cursor.get() - 1;
            let Some(query) = stack.borrow().get(at).cloned() else {
                return;
            };
            cursor.set(at);
            store_dictionary_history(&state, id, &stack.borrow(), at);
            run_lookup(&query, false);
        }
    });
    translate.forward.connect_clicked({
        let stack = history_stack.clone();
        let cursor = history_cursor.clone();
        let run_lookup = run_lookup.clone();
        let state = ctx.state.clone();
        move |_| {
            let at = cursor.get() + 1;
            let Some(query) = stack.borrow().get(at).cloned() else {
                return;
            };
            cursor.set(at);
            store_dictionary_history(&state, id, &stack.borrow(), at);
            run_lookup(&query, false);
        }
    });

    // Enter runs the query; Shift+Enter is left alone so a multi-line paste can
    // still be edited by hand.
    translate.input.connect_key_press_event({
        let translate = translate.clone();
        let lookup = lookup.clone();
        move |_, event| {
            let enter = matches!(
                event.keyval(),
                gdk::keys::constants::Return | gdk::keys::constants::KP_Enter
            );
            if enter && !event.state().contains(gdk::ModifierType::SHIFT_MASK) {
                lookup(&translate.query());
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });

    if let Some(buffer) = translate.input.buffer() {
        buffer.connect_changed({
            let translate = translate.clone();
            let state = ctx.state.clone();
            let generation = suggest_generation.clone();
            let pending = pending.clone();
            let tx = tx.clone();
            let lookup = lookup.clone();
            let recents_open = recents_open.clone();
            let fit_height = fit_height.clone();
            move |_| {
                if let Some(source) = pending.borrow_mut().take() {
                    source.remove();
                }
                // Retire an in-flight request immediately. Waiting until the
                // next debounce fires leaves a window where suggestions for
                // the previous prefix can render under the new text.
                let request_generation = generation.fetch_add(1, Ordering::Relaxed) + 1;
                let text = translate.query();
                // Prose has no completions worth offering; an empty box goes
                // back to showing what was searched before.
                if text.is_empty() {
                    render_translate_recents(
                        &translate.suggestions,
                        &state,
                        &lookup,
                        &recents_open,
                        &fit_height,
                    );
                    fit_height();
                    return;
                }
                clear_children(&translate.suggestions);
                fit_height();
                if translate::is_sentence(&text) {
                    return;
                }
                let pending_for_timer = pending.clone();
                let tx = tx.clone();
                let generation = generation.clone();
                let source = glib::timeout_add_local_once(TRANSLATE_SUGGEST_DELAY, move || {
                    pending_for_timer.borrow_mut().take();
                    translate::spawn_suggest(
                        text,
                        translate::Request::new(generation, request_generation),
                        tx,
                    );
                });
                *pending.borrow_mut() = Some(source);
            }
        });
    }

    let rx_cleanup = rx.clone();
    glib::MainContext::default().spawn_local({
        let suggestions = translate.suggestions.clone();
        let results = translate.results.clone();
        let scroller = translate.scroller.clone();
        let fit_height = fit_height.clone();
        let lookup_generation = lookup_generation.clone();
        let suggest_generation = suggest_generation.clone();
        let audio_buttons = audio_buttons.clone();
        let lookup = lookup.clone();
        let state = ctx.state.clone();
        let tx = tx.clone();
        let cache = cache.clone();
        let stack = history_stack.clone();
        let alive = alive.clone();
        async move {
            while let Ok(event) = rx.recv().await {
                if !alive.get() {
                    break;
                }
                match event {
                    translate::TranslateEvent::Suggestions {
                        generation: at,
                        items,
                    } if at == suggest_generation.load(Ordering::Relaxed) => {
                        render_translate_suggestions(&suggestions, &items, &lookup);
                        fit_height();
                    }
                    translate::TranslateEvent::Lookup {
                        generation: at,
                        result,
                    } if at == lookup_generation.load(Ordering::Relaxed) => {
                        // Remembered here rather than when the query was sent,
                        // so a typo that found nothing never enters the list.
                        if let translate::ResultKind::Content(content) = &result.kind {
                            remember_search(&state, &result.query);
                            // Only answers are worth keeping. Caching a network
                            // failure would pin this window to an error page
                            // every time the user stepped back onto the word.
                            let mut cache = cache.borrow_mut();
                            cache.insert(result.query.clone(), (**content).clone());
                            if cache.len() > TRANSLATE_HISTORY_LIMIT {
                                // Words no longer reachable by back or forward
                                // can never be shown from here again.
                                let stack = stack.borrow();
                                cache.retain(|query, _| stack.contains(query));
                            }
                        }
                        scroller.vadjustment().set_value(0.0);
                        render_translate_result(
                            &results,
                            &fit_height,
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

    // A lookup asked for from inside this window stays in this window; only the
    // separate "new window" item spawns another one.
    attach_color_mode_menu(
        &translate.card,
        key.clone(),
        ctx.state.clone(),
        ctx.registry.clone(),
        ctx.interactive.clone(),
        None,
        None,
        Some(LookupActions {
            here: Rc::new(RefCell::new(Some(lookup.clone()))),
            new_window: ctx.lookup_new_window.clone(),
            open_window: Some(NewWindowAction {
                spawn: ctx.spawn.clone(),
                header: translate.header.clone(),
            }),
        }),
        None,
    );

    ctx.scrollers.borrow_mut().push(translate.scroller.clone());
    let cleanup: Rc<dyn Fn()> = {
        let alive = alive.clone();
        let rx = rx_cleanup;
        let pending = pending.clone();
        let lookup_self = lookup_self.clone();
        let search_open = search_open.clone();
        let lookup_generation = lookup_generation.clone();
        let suggest_generation = suggest_generation.clone();
        Rc::new(move || {
            if !alive.replace(false) {
                return;
            }
            search_open.set(false);
            if let Some(source) = pending.borrow_mut().take() {
                source.remove();
            }
            // Workers still out on the network belong to a window that no
            // longer exists; retiring both counters stops them at their next
            // checkpoint instead of at their timeout.
            translate::Request::retire_all(&lookup_generation);
            translate::Request::retire_all(&suggest_generation);
            lookup_self.borrow_mut().take();
            rx.close();
        })
    };
    ctx.instances.borrow_mut().push(TranslateInstance {
        id,
        window: translate,
        lookup: lookup.clone(),
        set_search,
        search_open,
        refresh_nav: update_nav.clone(),
        cache: cache.clone(),
        cleanup,
    });
    update_nav();
    // Put back the word this window was showing when Sysi last closed.
    if let Some(query) = saved.as_ref().and_then(|entry| entry.query()) {
        run_lookup(query, false);
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
    // A dictionary is a window in its own right, so its title bar carries a
    // close cross rather than the notes' hide dash: pressing it takes the
    // window away for good, along with the answers it had cached.
    let close = small_button("\u{00d7}");
    close.style_context().add_class("note-window-button");
    close.style_context().add_class("note-close");
    close.set_tooltip_text(Some("Close this dictionary"));
    // Browser-style history for this window, sitting where a browser puts it:
    // immediately after the button that dismisses the window.
    let back = nav_button("go-previous-symbolic", "Back");
    let forward = nav_button("go-next-symbolic", "Forward");
    // The title fills the row so almost all of the header is drag surface.
    let title = gtk::Label::new(Some("DICTIONARY"));
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    title.style_context().add_class("history-title");
    let open_search = icon_button("edit-find-symbolic", "Search");
    // Not another cross. The window's own close button is one, sitting a few
    // pixels away in the same bar, and the two read as the same control — press
    // the wrong one and the window goes for good, cached answers and all. The
    // query panel drops down out of the header, so the chevron that folds it
    // back up says plainly which of the two this is.
    let close_search = icon_button("pan-up-symbolic", "Close search (Esc)");
    // The close cross and the two arrows are one cluster of window controls, so
    // they sit tight against each other and keep the header's wider spacing
    // only between the cluster and the title.
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 1);
    controls.pack_start(&close, false, false, 0);
    controls.pack_start(&back, false, false, 0);
    controls.pack_start(&forward, false, false, 0);
    bar.pack_start(&controls, false, false, 0);
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
    search_panel
        .style_context()
        .add_class("translate-search-panel");
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
    suggestions
        .style_context()
        .add_class("translate-suggestions");
    search_panel.pack_start(&suggestions, false, false, 0);
    chrome_box.pack_start(&search_panel, false, false, 0);

    let scroller = gtk::ScrolledWindow::new(None::<&gtk::Adjustment>, None::<&gtk::Adjustment>);
    // External (not Never) horizontally, or the widest definition line would
    // stop the window from ever being dragged narrower again.
    scroller.set_policy(gtk::PolicyType::External, gtk::PolicyType::Automatic);
    scroller.set_overlay_scrolling(true);
    scroller.set_shadow_type(gtk::ShadowType::None);
    scroller.set_propagate_natural_width(false);
    // Short answers set the card's natural height. Once the content reaches
    // this cap, the viewport stops growing and the vertical scrollbar takes
    // over.
    scroller.set_propagate_natural_height(true);
    scroller.set_min_content_height(1);
    scroller.set_max_content_height(TRANSLATE_RESULTS_MAX_HEIGHT);
    scroller.set_size_request(1, -1);
    scroller.set_hexpand(true);
    scroller.set_vexpand(false);
    scroller.style_context().add_class("history-scroller");
    let results = gtk::Box::new(gtk::Orientation::Vertical, 3);
    results.style_context().add_class("translate-results");
    scroller.add(&results);
    body.pack_start(&scroller, true, true, 0);

    TranslateWindow {
        card,
        chrome,
        header,
        close,
        back,
        forward,
        open_search,
        close_search,
        search_panel,
        input,
        suggestions,
        results,
        scroller,
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
    if let Some(label) = button
        .child()
        .and_then(|child| child.downcast::<gtk::Label>().ok())
    {
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
    fit_height: &Rc<dyn Fn()>,
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
    // Ellipsizing is what the completion rows do, and here it is load-bearing
    // for a second reason: with the row's CSS letter-spacing, an un-ellipsized
    // label reports a two-line height on its first measurement, so the fold
    // asked for 24px instead of the 14px its style says. Nothing put that
    // right until the pointer crossed the row and its :hover state forced a
    // restyle -- which is the gap that used to sit above RECENT for as long as
    // the mouse stayed away from the panel.
    caption.set_ellipsize(gtk::pango::EllipsizeMode::End);
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
        let fit_height = fit_height.clone();
        move |_| {
            expanded.set(!expanded.get());
            apply(expanded.get());
            fit_height();
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

const TRANSLATE_PAGE_SIZE: usize = 5;

fn translate_more_button() -> gtk::Button {
    let button = gtk::Button::new();
    button.set_can_focus(false);
    button.style_context().add_class("translate-more");
    button
}

fn append_translate_page<T>(
    list: &gtk::Box,
    more: &gtk::Button,
    items: &[T],
    shown: &Cell<usize>,
    render: &Rc<dyn Fn(&T) -> gtk::Box>,
) {
    let end = (shown.get() + TRANSLATE_PAGE_SIZE).min(items.len());
    for item in &items[shown.get()..end] {
        list.pack_start(&render(item), false, false, 0);
    }
    shown.set(end);
    list.show_all();
    let remaining = items.len().saturating_sub(end);
    if remaining == 0 {
        more.set_no_show_all(true);
        more.hide();
    } else {
        more.set_no_show_all(false);
        more.set_label(&format!("MORE · {remaining}"));
        more.show();
    }
}

fn translate_paged_list<T: Clone + 'static>(
    items: Vec<T>,
    render: Rc<dyn Fn(&T) -> gtk::Box>,
    fit_height: Rc<dyn Fn()>,
) -> gtk::Box {
    let panel = gtk::Box::new(gtk::Orientation::Vertical, 3);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 3);
    let more = translate_more_button();
    panel.pack_start(&list, false, false, 0);
    panel.pack_start(&more, false, false, 0);

    let items = Rc::new(items);
    let shown = Rc::new(Cell::new(0usize));
    append_translate_page(&list, &more, &items, &shown, &render);
    more.connect_clicked({
        let list = list.clone();
        let items = items.clone();
        let shown = shown.clone();
        let render = render.clone();
        move |more| {
            append_translate_page(&list, more, &items, &shown, &render);
            fit_height();
        }
    });
    panel
}

fn translate_meaning_block(meaning: &crate::translate::ViMeaning) -> gtk::Box {
    use crate::translate::escape_markup;
    let block = gtk::Box::new(gtk::Orientation::Vertical, 1);
    if !meaning.text.is_empty() {
        block.pack_start(
            &translate_line(
                &format!("• {}", escape_markup(&meaning.text)),
                "translate-meaning",
            ),
            false,
            false,
            0,
        );
    }
    for example in &meaning.examples {
        block.pack_start(
            &translate_line(
                &format!("<i>{}</i>", escape_markup(&example.en)),
                "translate-example",
            ),
            false,
            false,
            0,
        );
        if let Some(vi) = &example.vi {
            block.pack_start(
                &translate_line(&escape_markup(vi), "translate-example-vi"),
                false,
                false,
                0,
            );
        }
    }
    block
}

fn translate_phrase_block(phrase: &crate::translate::ViPhrase) -> gtk::Box {
    use crate::translate::escape_markup;
    let block = gtk::Box::new(gtk::Orientation::Vertical, 2);
    block.pack_start(
        &translate_line(
            &format!("▸ <b>{}</b>", escape_markup(&phrase.text)),
            "translate-phrase",
        ),
        false,
        false,
        0,
    );
    for meaning in &phrase.meanings {
        block.pack_start(&translate_meaning_block(meaning), false, false, 0);
    }
    block
}

fn build_cambridge_section(
    content: &gtk::Box,
    definitions: Vec<crate::translate::EnDefinition>,
    fit_height: Rc<dyn Fn()>,
) {
    use crate::translate::escape_markup;
    let render: Rc<dyn Fn(&crate::translate::EnDefinition) -> gtk::Box> = Rc::new(|definition| {
        let block = gtk::Box::new(gtk::Orientation::Vertical, 1);
        if !definition.pos.is_empty() {
            block.pack_start(
                &translate_line(
                    &escape_markup(&definition.pos.to_uppercase()),
                    "translate-pos",
                ),
                false,
                false,
                0,
            );
        }
        block.pack_start(
            &translate_line(&escape_markup(&definition.text), "translate-body"),
            false,
            false,
            0,
        );
        for example in &definition.examples {
            block.pack_start(
                &translate_line(
                    &format!("<i>{}</i>", escape_markup(example)),
                    "translate-example",
                ),
                false,
                false,
                0,
            );
        }
        block
    });
    content.pack_start(
        &translate_paged_list(definitions, render, fit_height),
        false,
        false,
        0,
    );
}

fn build_vi_section(
    content: &gtk::Box,
    entries: Vec<crate::translate::ViEntry>,
    fit_height: Rc<dyn Fn()>,
) {
    use crate::translate::escape_markup;
    for entry in entries {
        let pos = if entry.pos.is_empty() {
            "NGHĨA".to_owned()
        } else {
            entry.pos.to_uppercase()
        };
        content.pack_start(
            &translate_line(&escape_markup(&pos), "translate-pos"),
            false,
            false,
            0,
        );
        if !entry.meanings.is_empty() {
            let render: Rc<dyn Fn(&crate::translate::ViMeaning) -> gtk::Box> =
                Rc::new(translate_meaning_block);
            content.pack_start(
                &translate_paged_list(entry.meanings, render, fit_height.clone()),
                false,
                false,
                0,
            );
        }
        if !entry.phrases.is_empty() {
            content.pack_start(
                &translate_line("CỤM TỪ", "translate-subsection"),
                false,
                false,
                0,
            );
            let render: Rc<dyn Fn(&crate::translate::ViPhrase) -> gtk::Box> =
                Rc::new(translate_phrase_block);
            content.pack_start(
                &translate_paged_list(entry.phrases, render, fit_height.clone()),
                false,
                false,
                0,
            );
        }
    }
}

fn build_examples_section(
    content: &gtk::Box,
    examples: Vec<crate::translate::BilingualExample>,
    fit_height: Rc<dyn Fn()>,
) {
    use crate::translate::escape_markup;
    let render: Rc<dyn Fn(&crate::translate::BilingualExample) -> gtk::Box> = Rc::new(|example| {
        let block = gtk::Box::new(gtk::Orientation::Vertical, 1);
        block.pack_start(
            &translate_line(&example.en_markup, "translate-example-en"),
            false,
            false,
            0,
        );
        block.pack_start(
            &translate_line(&escape_markup(&example.vi), "translate-example-vi"),
            false,
            false,
            0,
        );
        block
    });
    content.pack_start(
        &translate_paged_list(examples, render, fit_height),
        false,
        false,
        0,
    );
}

type TranslateSectionBuilder = Rc<dyn Fn(&gtk::Box)>;
type TranslateSection = (
    glib::WeakRef<gtk::Button>,
    gtk::Box,
    String,
    TranslateSectionBuilder,
);

/// Put a caption on a section header. `gtk_button_set_label` rebuilds the
/// button's child, so everything the old label carried has to go back on each
/// time -- otherwise the headers, which start left-aligned, all jump to centre
/// the first time any section is opened and stay there for good.
fn set_translate_section_caption(header: &gtk::Button, caption: &str) {
    header.set_label(caption);
    if let Some(label) = header
        .child()
        .and_then(|child| child.downcast::<gtk::Label>().ok())
    {
        label.set_xalign(0.0);
        // Ellipsized for the same reason as the recents fold: with the
        // letter-spacing this row carries, an un-ellipsized label measures two
        // lines tall on its first pass and the column is sized around that.
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    }
}

fn add_translate_section(
    results: &gtk::Box,
    sections: &Rc<RefCell<Vec<TranslateSection>>>,
    open: &Rc<Cell<Option<usize>>>,
    fit_height: &Rc<dyn Fn()>,
    label: &str,
    count: usize,
    build: TranslateSectionBuilder,
) {
    let index = sections.borrow().len();
    let caption = format!("{label} · {count}");
    let button = gtk::Button::new();
    button.set_can_focus(false);
    button.style_context().add_class("translate-section-toggle");
    set_translate_section_caption(&button, &format!("▸  {caption}"));
    let content = gtk::Box::new(gtk::Orientation::Vertical, 3);
    content
        .style_context()
        .add_class("translate-section-content");
    content.set_no_show_all(true);
    content.hide();
    results.pack_start(&button, false, false, 0);
    results.pack_start(&content, false, false, 0);
    sections
        .borrow_mut()
        .push((button.downgrade(), content.clone(), caption, build));

    button.connect_clicked({
        let sections = sections.clone();
        let open = open.clone();
        let fit_height = fit_height.clone();
        move |_| {
            let closing = open.get() == Some(index);
            for (header, panel, caption, _) in sections.borrow().iter() {
                if let Some(header) = header.upgrade() {
                    set_translate_section_caption(&header, &format!("▸  {caption}"));
                }
                clear_children(panel);
                panel.hide();
            }
            open.set(None);
            if closing {
                fit_height();
                return;
            }
            let sections = sections.borrow();
            let (header, panel, caption, build) = &sections[index];
            let Some(header) = header.upgrade() else {
                return;
            };
            build(panel);
            panel.set_no_show_all(false);
            panel.show_all();
            panel.set_no_show_all(true);
            set_translate_section_caption(&header, &format!("▾  {caption}"));
            open.set(Some(index));
            fit_height();
        }
    });
}

/// Replace the answer column wholesale. Rebuilding rather than patching keeps
/// the render a pure function of the result, the way the note list works.
fn render_translate_result(
    results: &gtk::Box,
    fit_height: &Rc<dyn Fn()>,
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
        ResultKind::Content(word) => {
            results.pack_start(
                &translate_line(
                    &format!("<b>{}</b>", escape_markup(&word.headword)),
                    "translate-headword",
                ),
                false,
                false,
                0,
            );
            if let Some(translation) = &word.translation {
                let heading = if translation.detected.as_deref() == Some("vi") {
                    "GOOGLE · ENGLISH"
                } else {
                    "GOOGLE · VIETNAMESE"
                };
                results.pack_start(&translate_line(heading, "translate-pos"), false, false, 0);
                results.pack_start(
                    &translate_line(&escape_markup(&translation.translation), "translate-gloss"),
                    false,
                    false,
                    0,
                );
            }

            if !word.pronunciations.is_empty() {
                let row = translate_row(12);
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

            let sections: Rc<RefCell<Vec<TranslateSection>>> = Rc::new(RefCell::new(Vec::new()));
            let open = Rc::new(Cell::new(None));
            if !word.en_definitions.is_empty() {
                let definitions = word.en_definitions.clone();
                let count = definitions.len();
                add_translate_section(
                    results,
                    &sections,
                    &open,
                    fit_height,
                    "CAMBRIDGE",
                    count,
                    Rc::new({
                        let fit_height = fit_height.clone();
                        move |content| {
                            build_cambridge_section(
                                content,
                                definitions.clone(),
                                fit_height.clone(),
                            )
                        }
                    }),
                );
            }
            if !word.vi_entries.is_empty() {
                let entries = word.vi_entries.clone();
                let count = entries
                    .iter()
                    .map(|entry| entry.meanings.len() + entry.phrases.len())
                    .sum();
                add_translate_section(
                    results,
                    &sections,
                    &open,
                    fit_height,
                    "ANH–VIỆT",
                    count,
                    Rc::new({
                        let fit_height = fit_height.clone();
                        move |content| {
                            build_vi_section(content, entries.clone(), fit_height.clone())
                        }
                    }),
                );
            }
            if !word.examples.is_empty() {
                let examples = word.examples.clone();
                let count = examples.len();
                add_translate_section(
                    results,
                    &sections,
                    &open,
                    fit_height,
                    "EXAMPLES",
                    count,
                    Rc::new({
                        let fit_height = fit_height.clone();
                        move |content| {
                            build_examples_section(content, examples.clone(), fit_height.clone())
                        }
                    }),
                );
            }
            if !word.suggestions.is_empty() {
                results.pack_start(
                    &translate_line("DID YOU MEAN", "translate-pos"),
                    false,
                    false,
                    0,
                );
                for suggestion in &word.suggestions {
                    results.pack_start(&translate_word_button(suggestion, lookup), false, false, 0);
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
    fit_height();
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

/// Every panel action written since the last signal, oldest first.
///
/// Renamed before it is read, so an action the panel appends while this runs
/// lands in a fresh file and is picked up by its own signal rather than being
/// deleted unread.
/// A click on the GNOME panel: what to do, and where the button that asked for
/// it sits. The anchor is absent for anything that did not come from the panel.
struct PanelAction {
    name: String,
    anchor: Option<Point>,
}

fn take_panel_actions() -> Vec<PanelAction> {
    let dir = crate::state::cache_dir();
    let taken = dir.join("panel-action.taken");
    if fs::rename(dir.join("panel-action"), &taken).is_err() {
        return Vec::new();
    }
    let raw = fs::read_to_string(&taken).unwrap_or_default();
    let _ = fs::remove_file(&taken);
    raw.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            let (name, anchor) = match line.split_once('\t') {
                Some((name, anchor)) => (name, parse_panel_anchor(anchor)),
                None => (line, None),
            };
            PanelAction {
                name: name.trim().to_owned(),
                anchor,
            }
        })
        .collect()
}

fn parse_panel_anchor(raw: &str) -> Option<Point> {
    let (x, y) = raw.trim().split_once(',')?;
    Some(Point {
        x: x.trim().parse().ok()?,
        y: y.trim().parse().ok()?,
    })
}

/// One short line for the GNOME panel extension to read: whether the overlay is
/// accepting edits, and which colour mode it is in.
///
/// The extension used to flip its own two labels on click, which was wrong the
/// moment either was changed from anywhere else — locking with Escape or the
/// hotkey, or cycling the colour from the widget picker. Sysi owns both facts,
/// so it publishes them and the panel just reads. A tiny file of its own rather
/// than state.json: the shell would otherwise re-parse every note on the
/// overlay's every save.
fn publish_panel_state(interactive: bool, mode: ColorMode) {
    let dir = crate::state::cache_dir();
    let result = fs::create_dir_all(&dir).and_then(|_| {
        fs::write(
            dir.join("panel-state"),
            format!(
                "{} {}\n",
                if interactive { "editing" } else { "locked" },
                mode.key()
            ),
        )
    });
    if let Err(error) = result {
        eprintln!("Could not publish the Sysi panel state: {error}");
    }
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
        note_search: None,
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
    lookup: Option<LookupActions>,
    // Pops a menu for whatever the click landed on inside the widget, and says
    // whether it did. Only the history window has one (its note rows).
    row_menu: Option<Rc<dyn Fn() -> bool>>,
) {
    let menu = context_menu();

    // Looking up the selection sits at the top, above the colour modes: it is
    // the one item that acts on what the user just highlighted rather than on
    // the widget itself. The label is rewritten and the row shown or hidden
    // each time the menu pops up, from whatever is in the X11 primary
    // selection — which is set by selecting text in any application, so this
    // works on a note, on a result, or on text selected in a browser.
    let selected: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
    let lookup_item = lookup.map(|lookup| {
        let run_slot = |slot: LookupSlot, selected: Rc<RefCell<String>>| {
            move |_: &gtk::MenuItem| {
                let query = selected.borrow().clone();
                if query.is_empty() {
                    return;
                }
                // Cloned out of the borrow: the callback re-enters the UI, and
                // holding the slot borrowed across that would be a trap for
                // whoever next writes to it.
                let run = slot.borrow().clone();
                if let Some(run) = run {
                    run(&query);
                }
            }
        };
        let item = gtk::MenuItem::with_label("LOOK UP");
        item.connect_activate(run_slot(lookup.here.clone(), selected.clone()));
        // A second way out for the same selection: keep the answer already on
        // screen and put this one beside it.
        let new_item = gtk::MenuItem::with_label("LOOK UP IN NEW WINDOW");
        new_item.connect_activate(run_slot(lookup.new_window.clone(), selected.clone()));
        // Opening an empty window needs no selection; what gates it instead is
        // where the click landed, decided when the menu pops up.
        let open_item = lookup.open_window.clone().map(|action| {
            let open_item = gtk::MenuItem::with_label("NEW WINDOW");
            open_item.connect_activate({
                let spawn = action.spawn.clone();
                move |_| {
                    let run = spawn.borrow().clone();
                    if let Some(run) = run {
                        run(None);
                    }
                }
            });
            (open_item, action.header.clone())
        });
        let separator = gtk::SeparatorMenuItem::new();
        menu.append(&item);
        menu.append(&new_item);
        if let Some((open_item, _)) = &open_item {
            menu.append(open_item);
        }
        menu.append(&separator);
        (item, new_item, open_item, separator)
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
        let submenu = context_menu();
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
        menu.set_reserve_toggle_size(true);
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

        let gpus = gtk::CheckMenuItem::with_label("GPUS");
        gpus.set_active(system_details.details.get().gpus);
        gpus.connect_toggled({
            let preview = system_details.clone();
            let state = state.clone();
            move |item| {
                let mut details = preview.details.get();
                details.gpus = item.is_active();
                apply_system_details(&preview, &state, details);
            }
        });
        menu.append(&gpus);

        let root_disk = gtk::CheckMenuItem::with_label("DISK /");
        root_disk.set_active(system_details.details.get().root_disk);
        root_disk.connect_toggled({
            let preview = system_details.clone();
            let state = state.clone();
            move |item| {
                let mut details = preview.details.get();
                details.root_disk = item.is_active();
                apply_system_details(&preview, &state, details);
            }
        });
        menu.append(&root_disk);

        let home_disk = gtk::CheckMenuItem::with_label("DISK /HOME");
        home_disk.set_active(system_details.details.get().home_disk);
        home_disk.connect_toggled({
            let preview = system_details.clone();
            let state = state.clone();
            move |item| {
                let mut details = preview.details.get();
                details.home_disk = item.is_active();
                apply_system_details(&preview, &state, details);
            }
        });
        menu.append(&home_disk);
    }
    menu.show_all();

    let gesture = gtk::GestureMultiPress::new(widget);
    gesture.set_button(3);
    gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
    let menu_card = widget.clone();
    gesture.connect_pressed(move |gesture, _, x, y| {
        if !interactive.get() {
            gesture.set_state(gtk::EventSequenceState::Denied);
            return;
        }
        // The gesture is in the capture phase, so it sees the press before any
        // child does; ask the row menu first or the rows could never own a
        // right-click of their own.
        if let Some(row_menu) = &row_menu {
            if row_menu() {
                gesture.set_state(gtk::EventSequenceState::Claimed);
                return;
            }
        }
        if let Some((item, new_item, open_item, separator)) = &lookup_item {
            let query = primary_selection();
            let on_header = open_item
                .as_ref()
                .map(|(open_item, header)| {
                    let inside = menu_card
                        .translate_coordinates(header, x as i32, y as i32)
                        .is_some_and(|(hx, hy)| {
                            let area = header.allocation();
                            hx >= 0 && hy >= 0 && hx < area.width() && hy < area.height()
                        });
                    open_item.set_visible(inside);
                    inside
                })
                .unwrap_or(false);
            // The title bar is not text, so a lookup offered there would be
            // acting on a selection made somewhere the click never touched.
            let offer_lookup = !query.is_empty() && !on_header;
            // The separator belongs to the items above it; leaving it behind
            // would open every menu with a stray rule above the colour modes.
            item.set_visible(offer_lookup);
            new_item.set_visible(offer_lookup);
            separator.set_visible(offer_lookup || on_header);
            if !query.is_empty() {
                if let Some(label) = item.child().and_then(|c| c.downcast::<gtk::Label>().ok()) {
                    label.set_label(&format!(
                        "LOOK UP  \u{201c}{}\u{201d}",
                        ellipsize(&query, 22)
                    ));
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
    let size = system_card_size(details, &preview.values.borrow(), &state.borrow());
    preview.auto_size.set(Some(size));
    preview.card.set_size_request(size.width, size.height);
    preview.card.queue_resize();
    preview.canvas.queue_draw();
    // A section switched on has nothing sampled behind it yet. Ask for a
    // reading now so the card settles in one step instead of two seconds later.
    let resample = preview.resample.borrow().clone();
    if let Some(resample) = resample {
        resample();
    }
    let mut data = state.borrow_mut();
    data.settings.system_details = details;
    // A width the user dragged to survives; the height belongs to whatever the
    // card now has to show. Turning CPU CORES on computes that height from the
    // cores read so far — none, because the reader was not collecting them —
    // so the periodic update corrects it as soon as the first sample lands.
    if let Some(stored) = data.sizes.get_mut("system") {
        stored.height = size.height;
    }
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
    // Two rectangles per monitor. The geometry is the physical panel of glass
    // and is what the coordinate maths below is calibrated against; the work
    // area is that minus the shell's own furniture — the top bar, the dock — and
    // is the only part a widget may be placed in. Using the geometry here let a
    // window land under the top bar, where the bar paints over its header and
    // there is nothing left to drag it back out by.
    let mut raw_screens = Vec::new();
    let mut raw_areas = Vec::new();
    for index in 0..display.n_monitors() {
        if let Some(monitor) = display.monitor(index) {
            raw_screens.push(screen_rect(monitor.geometry()));
            raw_areas.push(screen_rect(monitor.workarea()));
        }
    }
    if raw_screens.is_empty() {
        return vec![fallback];
    }
    let divisor = monitor_coordinate_divisor(&raw_screens, scale, fallback);
    let root_bounds = monitor_root_bounds(&raw_screens, divisor, fallback);
    let screens: Vec<_> = raw_areas
        .into_iter()
        .filter_map(|screen| normalize_monitor_rect(screen, divisor, root_bounds))
        .collect();
    if screens.is_empty() {
        vec![fallback]
    } else {
        screens
    }
}

fn screen_rect(rectangle: gdk::Rectangle) -> ScreenRect {
    ScreenRect {
        x: rectangle.x(),
        y: rectangle.y(),
        width: rectangle.width(),
        height: rectangle.height(),
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
    // The work area, for the same reason as above: a default position derived
    // from the full geometry starts the widget under the top bar.
    let primary = screen_rect(monitor.workarea());
    let raw_screens: Vec<_> = (0..display.n_monitors())
        .filter_map(|index| display.monitor(index))
        .map(|monitor| screen_rect(monitor.geometry()))
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

fn screen_overlap(point: Point, width: i32, height: i32, screen: &ScreenRect) -> i64 {
    let left = point.x.max(screen.x);
    let top = point.y.max(screen.y);
    let right = (point.x + width).min(screen.x + screen.width);
    let bottom = (point.y + height).min(screen.y + screen.height);
    i64::from((right - left).max(0)) * i64::from((bottom - top).max(0))
}

/// The monitor a widget sits on: the one it covers most of.
///
/// It has to be chosen by where the widget actually is, not by which work area
/// could hold the most of it — a portrait monitor elsewhere on the desk would
/// otherwise be judged the better fit for a tall widget and leave it oversized
/// on the screen it is really on.
fn host_screen(
    point: Point,
    width: i32,
    height: i32,
    screens: &[ScreenRect],
) -> Option<ScreenRect> {
    let covered = screens
        .iter()
        .copied()
        .max_by_key(|screen| screen_overlap(point, width, height, screen))
        .filter(|screen| screen_overlap(point, width, height, screen) > 0);
    if covered.is_some() {
        return covered;
    }
    // Touching nothing — left on a monitor that has since been unplugged.
    // Fall back to whichever one it would travel least far to reach.
    screens.iter().copied().min_by_key(|screen| {
        let max_x = (screen.x + screen.width - width).max(screen.x);
        let max_y = (screen.y + screen.height - height).max(screen.y);
        let dx = i64::from(point.x.clamp(screen.x, max_x) - point.x);
        let dy = i64::from(point.y.clamp(screen.y, max_y) - point.y);
        dx * dx + dy * dy
    })
}

/// Trim a widget to its monitor's work area.
///
/// A widget taller than the area it lives in cannot be positioned at all:
/// `clamp_to_screens` collapses that axis to a single point and pins it there,
/// so it can no longer be dragged. That is how a window sized against the full
/// screen height ends up frozen once the shell's top bar is excluded from the
/// space it may occupy — it still slides sideways, but not up or down.
fn fit_to_work_area(point: Point, width: i32, height: i32, screens: &[ScreenRect]) -> (i32, i32) {
    match host_screen(point, width, height, screens) {
        Some(screen) => (width.min(screen.width), height.min(screen.height)),
        None => (width, height),
    }
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
) {
    let size = card_size(card, fallback);
    let point = reopen_point(
        reopen_anchor(),
        size,
        screens,
        primary,
        avoid.and_then(widget_rect),
    );
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
        // Shrink before placing: a widget larger than the work area has no room
        // to be moved in, and clamping its position alone would wedge it in the
        // corner with nothing to drag it back by.
        let origin = Point {
            x: allocation.x(),
            y: allocation.y(),
        };
        let (width, height) =
            fit_to_work_area(origin, allocation.width(), allocation.height(), screens);
        if width != allocation.width() || height != allocation.height() {
            item.widget.set_size_request(width, height);
            data.sizes.insert(item.key.clone(), Size { width, height });
        }
        let point = clamp_to_screens(origin, width, height, screens);
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

fn apply_translate_elastic_size(card: &gtk::EventBox, state: &Rc<RefCell<AppState>>) {
    let width = state
        .borrow()
        .sizes
        .get("translate")
        .map(|size| size.width)
        .filter(|width| *width > 0)
        .unwrap_or(TRANSLATE_WIDTH);
    // The empty/loading state is deliberately compact. Once content exists,
    // `fit_translate_height` replaces this with a measured height.
    card.set_size_request(width, TRANSLATE_EMPTY_HEIGHT);
    card.queue_resize();
}

fn fit_translate_height(
    card: &gtk::EventBox,
    chrome: &gtk::EventBox,
    results: &gtk::Box,
    alive: &Rc<Cell<bool>>,
) {
    // GtkFixed and GtkScrolledWindow do not reliably bubble an async child's
    // new natural height up to the card. Measure the two visible columns after
    // GTK has built their layouts, then make that measured height explicit.
    // The result column is capped; content beyond it belongs to the scrollbar.
    let card = card.clone();
    let chrome = chrome.clone();
    let results = results.clone();
    let alive = alive.clone();
    glib::idle_add_local_once(move || {
        if !alive.get() {
            return;
        }
        let width = if card.width_request() > 0 {
            card.width_request()
        } else {
            card.allocation().width().max(TRANSLATE_WIDTH)
        };
        let content_width = (width - 4).max(1);
        let chrome_height = if chrome.is_visible() {
            chrome.preferred_height_for_width(content_width).1
        } else {
            0
        };
        let results_height = if results.is_visible() {
            results
                .preferred_height_for_width(content_width)
                .1
                .min(TRANSLATE_RESULTS_MAX_HEIGHT)
        } else {
            0
        };
        let spacing = i32::from(chrome_height > 0 && results_height > 0) * 5;
        let height = (chrome_height + spacing + results_height).max(TRANSLATE_EMPTY_HEIGHT);
        card.set_size_request(width, height);
        card.queue_resize();
    });
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
    // Enumerating the monitors asks the server for each one's geometry and work
    // area. That is far too much to repeat per motion event, and the answer
    // cannot change in the middle of a drag, so it is taken once on the press.
    let gesture_screens: Rc<RefCell<Vec<ScreenRect>>> = Rc::new(RefCell::new(Vec::new()));
    handle.hitbox.connect_button_press_event({
        let start = start.clone();
        let latest = latest.clone();
        let card = card.clone();
        let root = root.clone();
        let gesture_screens = gesture_screens.clone();
        let interactive = interactive.clone();
        move |_, event| {
            if !interactive.get() || event.button() != 1 {
                return glib::Propagation::Proceed;
            }
            let allocation = card.allocation();
            let (pointer_x, pointer_y) = event.root();
            let root_allocation = root.allocation();
            *gesture_screens.borrow_mut() = logical_screen_rects(
                card.scale_factor(),
                root_allocation.width(),
                root_allocation.height(),
            );
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
        let gesture_screens = gesture_screens.clone();
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
            let screens = gesture_screens.borrow();
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
            let mut next = Size {
                width: next.width.clamp(bounds.min_width, max_width),
                height: next.height.clamp(bounds.min_height, max_height),
            };
            if let Some(height_for_width) = &bounds.height_for_width {
                // Not clamped to max_height: the content is what it is, and a
                // card cut short would simply hide the bottom of it.
                next.height = height_for_width(next.width).max(bounds.min_height);
            }
            drop(screens);
            let previous = latest.replace(next);
            card.set_size_request(next.width, next.height);
            card.queue_resize();
            // Only the card's own rectangle changed, and its origin did not
            // move. Redrawing the whole overlay here meant invalidating every
            // monitor on every motion event.
            root.queue_draw_area(
                allocation.x() - 3,
                allocation.y() - 3,
                next.width.max(previous.width) + 6,
                next.height.max(previous.height) + 6,
            );
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
    // Read once when the drag starts rather than on every motion event: asking
    // the server for each monitor's geometry and work area is not something to
    // repeat sixty times a second, and it cannot change mid-drag.
    let gesture_screens: Rc<RefCell<Vec<ScreenRect>>> = Rc::new(RefCell::new(Vec::new()));
    gesture.connect_drag_begin({
        let start = start.clone();
        let card = card.clone();
        let root = root.clone();
        let gesture_screens = gesture_screens.clone();
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
                let root_allocation = root.allocation();
                *gesture_screens.borrow_mut() = logical_screen_rects(
                    card.scale_factor(),
                    root_allocation.width(),
                    root_allocation.height(),
                );
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
        let gesture_screens = gesture_screens.clone();
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
                let screens = gesture_screens.borrow();
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
        let root = root.clone();
        let registry = registry.clone();
        let window = window.clone();
        let interactive = interactive.clone();
        move |_, _, _| {
            if start.get().is_none() {
                return;
            }
            start.set(None);
            let allocation = card.allocation();
            let origin = Point {
                x: allocation.x(),
                y: allocation.y(),
            };
            // The widget may have just landed on a shorter monitor than the one
            // it was sized on. Left oversized it would be stuck there: the
            // position clamp collapses that axis to a single point, so it could
            // slide sideways but never up or down again.
            let root_allocation = root.allocation();
            let screens = logical_screen_rects(
                card.scale_factor(),
                root_allocation.width(),
                root_allocation.height(),
            );
            let (width, height) =
                fit_to_work_area(origin, allocation.width(), allocation.height(), &screens);
            let resized = width != allocation.width() || height != allocation.height();
            if resized {
                card.set_size_request(width, height);
            }
            let point = clamp_to_screens(origin, width, height, &screens);
            if point.x != origin.x || point.y != origin.y {
                root.move_(&card, point.x, point.y);
            }
            let mut data = state.borrow_mut();
            if resized {
                data.sizes.insert(key.clone(), Size { width, height });
            }
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

thread_local! {
    /// Set while a panel action is being carried out; see `reopen_anchor`.
    static PANEL_ANCHOR: Cell<Option<Point>> = const { Cell::new(None) };
}

/// Where a widget being opened should be centred.
///
/// A click on the GNOME panel has to say where it happened, because nothing
/// here can find out. The panel is the compositor's own surface, so the X
/// server never sees the pointer over it and answers with wherever the mouse
/// last crossed an X window — which is how a widget asked for from the panel
/// kept opening on the far side of the screen. Everything else, including the
/// overlay's own picker, is a real X click and goes by the pointer.
fn reopen_anchor() -> Option<(f64, f64)> {
    if let Some(point) = PANEL_ANCHOR.with(|cell| cell.get()) {
        return Some((f64::from(point.x), f64::from(point.y)));
    }
    pointer_position()
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
        let lock_note = receives_input_when_locked(&item.key);
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

fn receives_input_when_locked(key: &str) -> bool {
    key.starts_with("note:") || key.starts_with("dict:") || key == "history"
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

// Every right-click menu in Sysi wears the same chrome. Left to the desktop
// theme they arrived in whatever shape and accent colour the session happened
// to use, which read as a foreign window dropped on top of the overlay; the
// look lives in style.css instead, under `.sysi-menu`.
fn context_menu() -> gtk::Menu {
    let menu = gtk::Menu::new();
    menu.style_context().add_class("sysi-menu");
    // Nothing to tick in most menus, and the reserved gutter left them looking
    // like a list with a missing column. The system menu turns it back on.
    menu.set_reserve_toggle_size(false);
    menu
}

fn small_button(label: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.style_context().add_class("tiny-button");
    button.set_can_focus(false);
    button
}

/// The dictionary header's back and forward arrows. An icon button, trimmed
/// down so a pair of them fits beside the title, and hidden rather than greyed
/// out when there is nowhere to go -- an arrow that cannot be used is noise.
fn nav_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = icon_button(icon_name, tooltip);
    button.style_context().add_class("translate-nav");
    if let Some(image) = button.child().and_then(|c| c.downcast::<gtk::Image>().ok()) {
        image.set_pixel_size(9);
    }
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

/// A meter caption sits in the gap at the bottom of its ring, which is only so
/// wide. Anything longer than that ("DISK /HOME", a spelt-out GPU model) is
/// stepped down in size until it fits rather than being drawn over the stroke.
#[allow(clippy::too_many_arguments)]
fn center_text_fitted(
    ctx: &Context,
    x: f64,
    y: f64,
    text: &str,
    size: f64,
    max_width: f64,
    weight: FontWeight,
    color: (f64, f64, f64),
) {
    center_text(
        ctx,
        x,
        y,
        text,
        fitted_font_size(ctx, text, size, max_width),
        weight,
        color,
    );
}

fn fitted_font_size(ctx: &Context, text: &str, size: f64, max_width: f64) -> f64 {
    let mut chosen = size;
    while chosen > 6.0 {
        ctx.set_font_size(chosen);
        let width = ctx
            .text_extents(text)
            .map(|extents| extents.x_advance())
            .unwrap_or(0.0);
        if width <= max_width {
            break;
        }
        chosen -= 0.25;
    }
    chosen
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
        ellipsize, fit_to_work_area, fit_within_bounds, history_row_budget, parse_panel_anchor,
        image_room, image_room_after_y, monitor_coordinate_divisor, monitor_root_bounds,
        normalize_monitor_rect, note_headline, note_image_cap, note_search_matches,
        note_size_for_image, parse_timer_input, push_recent_search, receives_input_when_locked,
        record_note_undo, reopen_point, resize_width_limit, resized_image_size,
        round_pixbuf_corners, system_content_size, system_meter_columns, system_meter_gap,
        system_meter_ink_width, system_meter_row_width, system_meter_rows, timer_style_size,
        NoteSearchMatch, NoteSearchOptions, NoteSnapshot, NoteUndo, NoteUndoState, ScreenRect,
        HISTORY_HEIGHT, HISTORY_WIDTH, NOTE_HEIGHT, NOTE_IMAGE_BORDER_RADIUS,
        NOTE_IMAGE_DEFAULT_MAX, NOTE_IMAGE_MAX, NOTE_IMAGE_MIN, NOTE_MAX_HEIGHT, NOTE_WIDTH,
        SYSTEM_HEIGHT, SYSTEM_METER_CELL, SYSTEM_METER_GAP, SYSTEM_METER_GAP_MIN,
        SYSTEM_METER_RING, SYSTEM_METER_RING_RADIUS, SYSTEM_METER_RING_STROKE,
    };
    use crate::state::{NoteImage, Point, Size, SystemDetails, TimerStyle, IMAGE_PLACEHOLDER};
    use crate::system::SystemSnapshot;
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
    fn dictionary_content_keeps_receiving_input_while_locked() {
        assert!(receives_input_when_locked("dict:1"));
        assert!(receives_input_when_locked("dict:42"));
        assert!(receives_input_when_locked("note:7"));
        assert!(receives_input_when_locked("history"));
        assert!(!receives_input_when_locked("system"));
        assert!(!receives_input_when_locked("translate"));
    }

    #[test]
    fn a_panel_click_carries_the_place_it_happened() {
        assert_eq!(parse_panel_anchor("960,42"), Some(Point { x: 960, y: 42 }));
        assert_eq!(parse_panel_anchor(" 960 , 42 "), Some(Point { x: 960, y: 42 }));
        // A monitor left of the primary puts the panel at a negative origin.
        assert_eq!(parse_panel_anchor("-40,42"), Some(Point { x: -40, y: 42 }));
        // Anything unreadable falls back to the pointer rather than to (0, 0),
        // which would fling the widget into the top-left corner.
        assert_eq!(parse_panel_anchor(""), None);
        assert_eq!(parse_panel_anchor("960"), None);
        assert_eq!(parse_panel_anchor("960,"), None);
        assert_eq!(parse_panel_anchor("left,top"), None);
    }

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
    fn note_search_is_case_insensitive_and_uses_character_offsets() {
        let matches = note_search_matches(
            "Đây là ghi chú. GHI CHÚ nữa.",
            "ghi chú",
            Default::default(),
        )
        .expect("literal search must compile");
        assert_eq!(
            matches,
            [
                NoteSearchMatch { start: 7, end: 14 },
                NoteSearchMatch { start: 16, end: 23 },
            ]
        );
    }

    #[test]
    fn note_search_options_handle_case_words_and_regular_expressions() {
        let case_sensitive = NoteSearchOptions {
            case_sensitive: true,
            ..Default::default()
        };
        assert_eq!(
            note_search_matches("Note note notebook", "Note", case_sensitive).unwrap(),
            [NoteSearchMatch { start: 0, end: 4 }]
        );

        let whole_word = NoteSearchOptions {
            whole_word: true,
            ..Default::default()
        };
        assert_eq!(
            note_search_matches("Note note notebook", "note", whole_word).unwrap(),
            [
                NoteSearchMatch { start: 0, end: 4 },
                NoteSearchMatch { start: 5, end: 9 },
            ]
        );

        let regex = NoteSearchOptions {
            regular_expression: true,
            ..Default::default()
        };
        assert_eq!(
            note_search_matches("item 7, item 42", r"item \d+", regex).unwrap(),
            [
                NoteSearchMatch { start: 0, end: 6 },
                NoteSearchMatch { start: 8, end: 15 },
            ]
        );
        assert!(note_search_matches("text", "(", regex).is_err());
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
                ..SystemDetails::default()
            },
            &SystemSnapshot::default(),
            None,
        );
        // The ring plus its stroke, and not a pixel of margin around it.
        assert_eq!(
            size,
            Size {
                width: 63,
                height: 63
            }
        );
    }

    #[test]
    fn the_card_hugs_its_rings_rather_than_the_cells_around_them() {
        // What a ring actually paints: the arc plus the half of its stroke that
        // falls outside. The card has to measure exactly this, on every side.
        // Measured to the cell instead, the widget kept a blank margin left,
        // right and below, and could not be pushed flush against a screen edge
        // the way it already could against the top.
        let ring = 2.0 * (SYSTEM_METER_RING_RADIUS + SYSTEM_METER_RING_STROKE / 2.0);
        assert_eq!(system_meter_ink_width(1), ring);
        // Each further ring adds one whole cell and no margin of its own.
        for columns in 1..8 {
            assert_eq!(
                system_meter_ink_width(columns + 1) - system_meter_ink_width(columns),
                SYSTEM_METER_CELL
            );
        }
        // A one-ring card is that same square in both directions, and a row of
        // rings is no taller than one of them.
        let one_meter = SystemDetails {
            cpu: true,
            ram: false,
            processes: false,
            cores: false,
            ..SystemDetails::default()
        };
        let size = system_content_size(one_meter, &SystemSnapshot::default(), None);
        assert_eq!(size.width, ring.ceil() as i32);
        assert_eq!(size.height, ring.ceil() as i32);
        // The row still occupies its full cell height internally; only the
        // slack above and below the outermost rows is given back.
        assert!(f64::from(size.height) < f64::from(SYSTEM_HEIGHT));
    }

    #[test]
    fn meters_split_evenly_across_their_rows_rather_than_leaving_a_stray() {
        let default_rows = |count: usize| system_meter_rows(count, count.min(3));
        assert!(default_rows(0).is_empty());
        assert_eq!(default_rows(1), [1]);
        assert_eq!(default_rows(2), [2]);
        assert_eq!(default_rows(3), [3]);
        // Four rings read as two over two, not three over one.
        assert_eq!(default_rows(4), [2, 2]);
        assert_eq!(default_rows(5), [3, 2]);
        assert_eq!(default_rows(6), [3, 3]);
        assert_eq!(default_rows(7), [3, 2, 2]);
        assert_eq!(default_rows(8), [3, 3, 2]);
        // A row never carries more than fits across, and the split stays even.
        assert_eq!(system_meter_rows(6, 6), [6]);
        assert_eq!(system_meter_rows(6, 4), [3, 3]);
        assert_eq!(system_meter_rows(6, 1), [1, 1, 1, 1, 1, 1]);
        assert_eq!(system_meter_rows(7, 4), [4, 3]);
    }

    #[test]
    fn the_rings_reflow_into_whatever_width_the_card_was_dragged_to() {
        // Six meters on the default card: three across, two rows.
        assert_eq!(system_meter_columns(6, 290.0), 3);
        // Dragged wide enough for all six, they take one row.
        assert_eq!(system_meter_columns(6, 564.0), 6);
        // A row is kept as long as its rings still fit at their tightest
        // spacing, so the sixth ring comes up well before the card is wide
        // enough to give it a whole cell of its own.
        let six_across = system_meter_row_width(6, SYSTEM_METER_GAP_MIN);
        assert_eq!(system_meter_columns(6, six_across), 6);
        assert_eq!(system_meter_columns(6, six_across - 1.0), 3);
        // Never more columns than there are rings, however wide it gets.
        assert_eq!(system_meter_columns(2, 900.0), 2);
        // Narrower than one ring still draws that ring, scaled down.
        assert_eq!(system_meter_columns(4, 76.0), 1);
        assert_eq!(system_meter_columns(0, 900.0), 0);
    }

    #[test]
    fn a_row_of_meters_stays_centred_on_the_card_it_is_drawn_in() {
        // Ring centres on an untouched card, measured from its left edge. At
        // the default spacing they land one cell apart, starting half a ring
        // in — the geometry the card has always drawn.
        let centres = |count: usize, columns: usize, gap: f64| -> Vec<f64> {
            let rows = system_meter_rows(count, columns);
            let widest = rows.iter().copied().max().unwrap_or(0);
            let content = system_meter_row_width(widest, gap);
            let mut result = Vec::new();
            for row in rows {
                let left = (content - system_meter_row_width(row, gap)) / 2.0;
                for column in 0..row {
                    result.push(
                        left + column as f64 * (SYSTEM_METER_RING + gap) + SYSTEM_METER_RING / 2.0,
                    );
                }
            }
            result
        };
        let half = SYSTEM_METER_RING / 2.0;
        let step = SYSTEM_METER_CELL;
        assert_eq!(centres(1, 1, SYSTEM_METER_GAP), [half]);
        assert_eq!(centres(2, 2, SYSTEM_METER_GAP), [half, half + step]);
        assert_eq!(
            centres(3, 3, SYSTEM_METER_GAP),
            [half, half + step, half + 2.0 * step]
        );
        // Five rings: three across, then two centred underneath them.
        assert_eq!(
            centres(5, 3, SYSTEM_METER_GAP),
            [
                half,
                half + step,
                half + 2.0 * step,
                half + step / 2.0,
                half + 1.5 * step,
            ]
        );
        // Spread the same three rings over a wider card and only the spacing
        // changes: the first ring stays flush against the left edge and the
        // last against the right.
        let wide = 400.0;
        let gap = system_meter_gap(3, wide);
        let spread = centres(3, 3, gap);
        assert_eq!(spread[0], half);
        assert_eq!(spread[2], wide - half);
    }

    #[test]
    fn dragging_the_card_spreads_the_rings_before_it_reflows_them() {
        // Widening the card moves the rings apart and leaves them filling it
        // edge to edge. It never resizes them, and never leaves a margin at
        // either end for the card to hang off a screen edge by.
        let natural = system_meter_ink_width(6);
        for width in [natural, natural + 40.0, natural + 160.0] {
            let gap = system_meter_gap(6, width);
            assert_eq!(system_meter_row_width(6, gap), width);
            assert!(gap >= SYSTEM_METER_GAP_MIN);
        }
        // The spacing tracks the width continuously rather than in jumps.
        assert!(system_meter_gap(6, 560.0) > system_meter_gap(6, 460.0));
        // Left alone, that spacing is the one the card has always used.
        assert_eq!(system_meter_gap(6, natural), SYSTEM_METER_GAP);

        // Squeezed past the point where the rings would sit closer than they
        // are allowed to, the row gives one up instead of overlapping them.
        let tightest = system_meter_row_width(6, SYSTEM_METER_GAP_MIN);
        assert_eq!(system_meter_columns(6, tightest), 6);
        assert_eq!(system_meter_columns(6, tightest - 1.0), 3);
        // Whatever row takes over fills the card the same way, so the widget
        // stays flush through the change.
        let gap = system_meter_gap(3, tightest - 1.0);
        assert_eq!(system_meter_row_width(3, gap), tightest - 1.0);
    }

    #[test]
    fn the_card_grows_a_row_at_a_time_as_meters_are_switched_on() {
        let details = SystemDetails {
            cpu: true,
            ram: true,
            gpus: true,
            root_disk: true,
            home_disk: true,
            processes: false,
            cores: false,
        };
        let values = SystemSnapshot {
            gpus: vec![
                crate::system::GpuSnapshot {
                    label: "RTX 4060".into(),
                    percent: 12.0,
                },
                crate::system::GpuSnapshot {
                    label: "AMD GPU".into(),
                    percent: 4.0,
                },
            ],
            root_disk_percent: Some(66.0),
            home_disk_percent: Some(63.0),
            ..SystemSnapshot::default()
        };
        // Six meters, untouched card: two rows of three, wide enough for three.
        assert_eq!(
            system_content_size(details, &values, None),
            Size {
                width: 251,
                height: 139
            }
        );
        // A machine where nothing answered for the GPUs must not be left
        // holding an empty row.
        assert_eq!(
            system_content_size(
                details,
                &SystemSnapshot {
                    root_disk_percent: Some(66.0),
                    home_disk_percent: Some(63.0),
                    ..SystemSnapshot::default()
                },
                None
            ),
            Size {
                width: 157,
                height: 139
            }
        );
        // Dragged out to six across, the second row has to be handed back
        // rather than left behind as empty space.
        assert_eq!(
            system_content_size(details, &values, Some(572)),
            Size {
                width: 572,
                height: 63
            }
        );
        // And pulled in to one across, the card has to find five more rows.
        assert_eq!(
            system_content_size(details, &values, Some(76)),
            Size {
                width: 76,
                height: 443
            }
        );
        // The process table keeps its own 108 on top of whatever the rings need.
        assert_eq!(
            system_content_size(
                SystemDetails {
                    processes: true,
                    ..details
                },
                &values,
                Some(572)
            ),
            Size {
                width: 572,
                height: 181
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
            assert!(push_recent_search(
                &mut recents,
                &format!("word{index}"),
                10
            ));
        }
        assert_eq!(recents.len(), 10);
        // Newest first, oldest dropped off the end.
        assert_eq!(recents[0], "word11");
        assert_eq!(recents[9], "word2");
    }

    #[test]
    fn a_widget_taller_than_the_work_area_is_trimmed_to_it() {
        let area = [ScreenRect {
            x: 0,
            y: 32,
            width: 1280,
            height: 688,
        }];
        let origin = Point { x: 100, y: 40 };
        // The exact case that froze the dictionary: sized against the full
        // screen height, then judged against the height minus the top bar.
        assert_eq!(fit_to_work_area(origin, 359, 700, &area), (359, 688));
        assert_eq!(fit_to_work_area(origin, 2000, 100, &area), (1280, 100));
        // Anything that already fits is left alone.
        assert_eq!(fit_to_work_area(origin, 300, 400, &area), (300, 400));
        assert_eq!(fit_to_work_area(origin, 300, 400, &[]), (300, 400));
    }

    #[test]
    fn a_widget_is_trimmed_to_the_monitor_it_sits_on_not_the_roomiest_one() {
        // A landscape monitor beside a portrait one — the layout that hid this:
        // the portrait screen is tall enough for the widget, so judging by
        // "which could hold most of it" left the widget oversized and frozen on
        // the landscape screen it was actually on.
        let screens = [
            ScreenRect {
                x: 0,
                y: 0,
                width: 540,
                height: 809,
            },
            ScreenRect {
                x: 540,
                y: 151,
                width: 1280,
                height: 691,
            },
        ];
        let on_landscape = Point { x: 756, y: 151 };
        assert_eq!(
            fit_to_work_area(on_landscape, 359, 700, &screens),
            (359, 691)
        );
        let on_portrait = Point { x: 40, y: 200 };
        assert_eq!(
            fit_to_work_area(on_portrait, 359, 700, &screens),
            (359, 700)
        );
    }

    #[test]
    fn a_widget_on_no_monitor_at_all_falls_back_to_the_nearest() {
        let screens = [ScreenRect {
            x: 0,
            y: 0,
            width: 800,
            height: 600,
        }];
        // Left behind on a monitor that has since been unplugged.
        let stranded = Point { x: 5000, y: 5000 };
        assert_eq!(fit_to_work_area(stranded, 900, 700, &screens), (800, 600));
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
