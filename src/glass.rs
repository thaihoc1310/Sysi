//! Sysi's half of GLASS mode. The glass itself is drawn by the GNOME Shell
//! extension, the only thing on Wayland that can see what lies behind a
//! window. Sysi tells it where each glass card is, once per painted frame
//! and only when something changed, and learns from the reply whether glass
//! is really being drawn. Until it is, the cards keep a frosted plate of
//! their own (the `glass-live` class decides which).

use gtk::cairo::Context;
use gtk::gio;
use gtk::glib::{self, ToVariant};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

/// Corner radius of a glass plate, in logical pixels.
pub const RADIUS: f64 = 14.0;

const BUS_NAME: &str = "io.sysi.Glass";
const OBJECT_PATH: &str = "/io/sysi/Glass";
const INTERFACE: &str = "io.sysi.Glass1";
const LIVE_CLASS: &str = "glass-live";

/// Whether a card sits on glass.
pub fn has_glass(widget: &impl IsA<gtk::Widget>) -> bool {
    widget.style_context().has_class("mode-glass")
}

/// The rounded rectangle one card's glass occupies, in the card's own
/// coordinate space (logical pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Outline {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub radius: f64,
}

impl Outline {
    pub fn new(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
            radius: RADIUS.min(width / 2.0).min(height / 2.0),
        }
    }

    pub fn pill(x: f64, y: f64, width: f64, height: f64) -> Self {
        Self {
            x,
            y,
            width,
            height,
            radius: (width / 2.0).min(height / 2.0),
        }
    }

    pub fn trace(&self, cr: &Context) {
        let Self {
            x,
            y,
            width,
            height,
            radius,
        } = *self;
        let quarter = std::f64::consts::FRAC_PI_2;
        cr.new_sub_path();
        cr.arc(x + width - radius, y + radius, radius, -quarter, 0.0);
        cr.arc(
            x + width - radius,
            y + height - radius,
            radius,
            0.0,
            quarter,
        );
        cr.arc(
            x + radius,
            y + height - radius,
            radius,
            quarter,
            2.0 * quarter,
        );
        cr.arc(x + radius, y + radius, radius, 2.0 * quarter, 3.0 * quarter);
        cr.close_path();
    }
}

/// One card as the publisher finds it, in paint order.
#[derive(Clone, Debug, PartialEq)]
pub struct CardSample {
    pub key: String,
    /// Allocation in window coordinates, logical pixels.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub pressed: bool,
    /// False for a card with no glass (LIGHT, DARK, ...): sent all the same,
    /// with a negative radius, so the extension knows it is shown and can
    /// swap it with its lifted window in one frame.
    pub glass: bool,
}

/// What the extension is sent for one card: key, rect and radius in X11
/// pixels, and whether the pointer is holding it.
pub type WireCard = (String, f64, f64, f64, f64, f64, bool);

pub fn wire_cards(samples: &[CardSample], scale: f64) -> Vec<WireCard> {
    samples
        .iter()
        // A hidden card keeps a 1x1 allocation.
        .filter(|card| card.width > 1 && card.height > 1)
        .map(|card| {
            let outline = if card.key == "voice" {
                Outline::pill(
                    f64::from(card.x),
                    f64::from(card.y),
                    f64::from(card.width),
                    f64::from(card.height),
                )
            } else {
                Outline::new(
                    f64::from(card.x),
                    f64::from(card.y),
                    f64::from(card.width),
                    f64::from(card.height),
                )
            };
            (
                card.key.clone(),
                outline.x * scale,
                outline.y * scale,
                outline.width * scale,
                outline.height * scale,
                if card.glass {
                    outline.radius * scale
                } else {
                    -1.0
                },
                card.pressed,
            )
        })
        .collect()
}

type Message = (u64, f64, f64, Vec<WireCard>);

struct Link {
    window: gtk::Window,
    /// The container the cards sit in.
    root: gtk::Container,
    connection: RefCell<Option<gio::DBusConnection>>,
    /// The newest cards, sent or waiting to be.
    last: RefCell<Option<Message>>,
    /// Whether a call is on the bus. One at a time: while a card is dragged
    /// Sysi paints faster than a busy shell answers, and every frame's call
    /// would queue up behind the last.
    in_flight: Cell<bool>,
    /// Cards painted while a call was on the bus; only the newest is kept.
    waiting: RefCell<Option<Message>>,
    /// Retries since the extension last took the cards.
    retries: Cell<u32>,
    retry_pending: Cell<bool>,
    /// The overlay's cards, in paint order.
    collect: Box<dyn Fn() -> Vec<CardSample>>,
}

/// How often a refusal is asked again before Sysi keeps its own plates.
const MAX_RETRIES: u32 = 8;

thread_local! {
    // The bus watcher's callbacks must be Send, so they reach the link
    // through here rather than by capturing it. They always run on the main
    // thread, where it was set.
    static LINK: RefCell<Option<Rc<Link>>> = const { RefCell::new(None) };
}

fn with_link(action: impl FnOnce(&Rc<Link>)) {
    let link = LINK.with(|link| link.borrow().clone());
    if let Some(link) = link {
        action(&link);
    }
}

/// Start watching for the extension. `collect` is asked for the glass cards
/// on `root` after every painted frame.
pub fn start(
    window: &gtk::Window,
    root: &gtk::Container,
    collect: impl Fn() -> Vec<CardSample> + 'static,
) {
    let link = Rc::new(Link {
        window: window.clone(),
        root: root.clone(),
        connection: RefCell::new(None),
        last: RefCell::new(None),
        in_flight: Cell::new(false),
        waiting: RefCell::new(None),
        retries: Cell::new(0),
        retry_pending: Cell::new(false),
        collect: Box::new(collect),
    });
    LINK.with(|slot| *slot.borrow_mut() = Some(link.clone()));
    gio::bus_watch_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameWatcherFlags::NONE,
        |connection, _, _| {
            with_link(|link| {
                link.connection.replace(Some(connection.clone()));
                link.last.replace(None);
                link.retries.set(0);
                link.window.queue_draw();
            });
            ask_caps(&connection);
        },
        |_, _| {
            with_link(|link| {
                link.connection.replace(None);
                CAPS.with(|caps| caps.set(0));
                link.last.replace(None);
                link.waiting.replace(None);
                set_live(&link.window, false);
            })
        },
    );
    let Some(clock) = window.frame_clock() else {
        return;
    };
    clock.connect_after_paint(move |_| {
        // A swap is under way: its update goes out on its own, and one sent
        // from here as well would start the extension's wait for the paint
        // early (see swap_cards).
        if SWAPPING.with(Cell::get) > 0 {
            return;
        }
        let Some(message) = current_message(&link) else {
            return;
        };
        if link.last.borrow().as_ref() == Some(&message) {
            return;
        }
        link.last.replace(Some(message.clone()));
        if link.in_flight.get() {
            link.waiting.replace(Some(message));
            return;
        }
        send(&link, message);
    });
}

/// The cards as they are now, ready to send; None with no extension or
/// window to send them for.
fn current_message(link: &Link) -> Option<Message> {
    link.connection.borrow().as_ref()?;
    let xid = link
        .window
        .window()
        .and_then(|window| window.downcast::<gdkx11::X11Window>().ok())
        .map(|window| window.xid())?;
    let scale = f64::from(link.window.scale_factor().max(1));
    Some((
        xid,
        f64::from(link.window.allocated_width()) * scale,
        f64::from(link.window.allocated_height()) * scale,
        wire_cards(
            &[(link.collect)(), popover_samples(&link.root)]
                .concat()
                .into_iter()
                // An extension without lifts would draw them square.
                .filter(|card| card.glass || lifts_supported())
                .collect::<Vec<_>>(),
            scale,
        ),
    ))
}

/// Send the cards now, before the overlay paints the change they describe,
/// and run `then` (which paints it) once the extension has them, or after
/// SWAP_WAIT if it is slow to say so: a card leaving for its lifted window or
/// coming back from it. The extension applies such an update in the first
/// frame that paints the overlay after it arrives, which is then this one;
/// sent after the paint, as every frame's update is, it could miss that
/// frame, and meanwhile the card would show twice or not at all. Not held
/// behind a call already on the bus, for the same reason.
fn send_cards_now(then: impl FnOnce() + 'static) {
    let then: Rc<RefCell<Option<Box<dyn FnOnce()>>>> = Rc::new(RefCell::new(Some(Box::new(then))));
    let run = {
        let then = then.clone();
        move || {
            let then = then.borrow_mut().take();
            if let Some(then) = then {
                then();
            }
        }
    };
    let sent = LINK.with(|link| link.borrow().clone()).and_then(|link| {
        let message = current_message(&link)?;
        let connection = link.connection.borrow().clone()?;
        link.last.replace(Some(message.clone()));
        link.waiting.replace(None);
        Some((connection, message))
    });
    let Some((connection, message)) = sent else {
        run();
        return;
    };
    glib::timeout_add_local_once(SWAP_WAIT, run.clone());
    connection.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        INTERFACE,
        "SetCards",
        Some(&message.to_variant()),
        Some(glib::VariantTy::new("(b)").expect("valid reply type")),
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
        move |_| run(),
    );
}

/// How long a swap waits for the extension to take its cards before it is
/// painted all the same.
const SWAP_WAIT: Duration = Duration::from_millis(60);

/// The longest wait for the overlay's next frame, should its clock stall.
const FRAME_WAIT: Duration = Duration::from_millis(100);

/// Run `then` once a frame of `window` has run after this call: GTK does not
/// start a frame until the shell has drawn the last one it painted, so by
/// then every paint sent before is on screen, not still on its way.
pub fn after_next_frame(window: &gtk::Window, then: impl FnOnce() + 'static) {
    let then: Rc<RefCell<Option<Box<dyn FnOnce()>>>> = Rc::new(RefCell::new(Some(Box::new(then))));
    let run = {
        let then = then.clone();
        move || {
            let then = then.borrow_mut().take();
            if let Some(then) = then {
                then();
            }
        }
    };
    let Some(clock) = window.frame_clock() else {
        run();
        return;
    };
    glib::timeout_add_local_once(FRAME_WAIT, run.clone());
    // Called from inside a frame, that frame's own end does not count.
    let asked_at = clock.frame_counter();
    let handler = Rc::new(Cell::new(None::<glib::SignalHandlerId>));
    let id = clock.connect_after_paint({
        let handler = handler.clone();
        move |clock| {
            if clock.frame_counter() <= asked_at {
                clock.request_phase(gdk::FrameClockPhase::AFTER_PAINT);
                return;
            }
            if let Some(id) = handler.take() {
                clock.disconnect(id);
            }
            run();
        }
    });
    handler.set(Some(id));
    clock.request_phase(gdk::FrameClockPhase::AFTER_PAINT);
}

/// Move a card between the overlay and its lifted window: `change` makes
/// the change (the card's opacity, its place), and the extension shows or
/// hides the lifted window in the frame that paints it. The extension waits
/// for the first paint of the overlay after it has the update, so no other
/// paint may come between: the overlay is frozen while the update goes out,
/// until a frame after the freeze has run (every paint before it is then on
/// screen), and thawed once the extension has it, with the change, into one
/// paint.
pub fn swap_cards(window: &gtk::Window, change: impl FnOnce() + 'static) {
    let connected = LINK.with(|link| {
        link.borrow()
            .as_ref()
            .is_some_and(|link| link.connection.borrow().is_some())
    });
    let surface = window.window();
    let (true, Some(surface)) = (connected, surface) else {
        change();
        return;
    };
    surface.freeze_updates();
    SWAPPING.with(|swapping| swapping.set(swapping.get() + 1));
    after_next_frame(window, move || {
        send_cards_now(move || {
            change();
            surface.thaw_updates();
            SWAPPING.with(|swapping| swapping.set(swapping.get().saturating_sub(1)));
        });
    });
}

thread_local! {
    /// Swaps under way. The extension gives a swap's paint 50 ms from the
    /// first update that asks for it; an update from the frame clock in the
    /// meantime would start that wait before the swap's own.
    static SWAPPING: Cell<u32> = const { Cell::new(0) };
}

thread_local! {
    /// What the extension answered to Caps: 1 for cards without glass and
    /// lifted cards; 0 for one too old to know the call.
    static CAPS: Cell<u32> = const { Cell::new(0) };
}

fn ask_caps(connection: &gio::DBusConnection) {
    connection.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        INTERFACE,
        "Caps",
        None,
        Some(glib::VariantTy::new("(u)").expect("valid reply type")),
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
        |reply| {
            let caps = reply
                .ok()
                .and_then(|reply| reply.get::<(u32,)>())
                .map_or(0, |(caps,)| caps);
            CAPS.with(|known| known.set(caps));
            // Send the cards again, now with those it can take.
            with_link(|link| {
                link.last.replace(None);
                link.window.queue_draw();
            });
        },
    );
}

/// Whether the extension swaps a dragged card's window with the card itself,
/// whatever the card's look: it then shows and hides that window in the
/// frames that change the overlay.
pub fn lifts_supported() -> bool {
    CAPS.with(Cell::get) >= 1 && glass_is_live()
}

fn send(link: &Rc<Link>, message: Message) {
    let Some(connection) = link.connection.borrow().clone() else {
        return;
    };
    link.in_flight.set(true);
    let weak = Rc::downgrade(link);
    connection.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        INTERFACE,
        "SetCards",
        Some(&message.to_variant()),
        Some(glib::VariantTy::new("(b)").expect("valid reply type")),
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
        move |reply| {
            let Some(link) = weak.upgrade() else {
                return;
            };
            link.in_flight.set(false);
            // Newer cards were painted meanwhile; only their answer counts.
            if let Some(waiting) = link.waiting.take() {
                send(&link, waiting);
                return;
            }
            match reply.map(|reply| reply.get::<(bool,)>()) {
                Ok(Some((true,))) => {
                    link.retries.set(0);
                    set_live(&link.window, true);
                }
                // A refusal: the extension has not found the window (yet).
                Ok(_) => {
                    set_live(&link.window, false);
                    retry(&link);
                }
                // A slow or busy shell is no reason to drop the glass it is
                // drawing; ask again. If the extension really went away, the
                // bus watcher says so.
                Err(_) => retry(&link),
            }
        },
    );
}

/// The extension may not have found the window yet (it maps a moment after
/// Sysi first paints). Ask again a little later, less and less often, and
/// give up after a while rather than asking for ever.
fn retry(link: &Rc<Link>) {
    let tries = link.retries.get();
    if tries >= MAX_RETRIES || link.retry_pending.replace(true) {
        return;
    }
    link.retries.set(tries + 1);
    let delay = Duration::from_millis(250 << tries.min(4));
    let weak = Rc::downgrade(link);
    glib::timeout_add_local_once(delay, move || {
        let Some(link) = weak.upgrade() else {
            return;
        };
        link.retry_pending.set(false);
        // Only the cards are asked again; repainting the whole window for it
        // made the shell copy every card's glass afresh too.
        if link.in_flight.get() {
            return;
        }
        let last = link.last.borrow().clone();
        if let Some(message) = last {
            send(&link, message);
        }
    });
}

fn set_live(window: &gtk::Window, live: bool) {
    let context = window.style_context();
    if context.has_class(LIVE_CLASS) == live {
        return;
    }
    if live {
        context.add_class(LIVE_CLASS);
    } else {
        context.remove_class(LIVE_CLASS);
    }
    window.queue_draw();
}

/// Popups (a card's context menu, its search options) take on the glass of
/// the card they belong to.
const GLASS_POPUP: &str = "glass-popup";
/// A menu opened on a LIGHT card, white like it.
const LIGHT_MENU: &str = "menu-light";
/// The inset a popover's glass keeps around its content.
const POPOVER_PAD: i32 = 6;
/// Matches `.sysi-menu`'s corner radius.
const MENU_RADIUS: f64 = 8.0;

thread_local! {
    static POPOVERS: RefCell<Vec<(String, glib::WeakRef<gtk::Popover>)>> =
        const { RefCell::new(Vec::new()) };
    static NEXT_POPOVER: Cell<u64> = const { Cell::new(0) };
}

pub fn glass_is_live() -> bool {
    LINK.with(|link| {
        link.borrow()
            .as_ref()
            .is_some_and(|link| link.window.style_context().has_class(LIVE_CLASS))
    })
}

fn root() -> Option<gtk::Container> {
    LINK.with(|link| link.borrow().as_ref().map(|link| link.root.clone()))
}

/// The card `widget` sits in: its ancestor placed straight on the root.
fn card_of(widget: &gtk::Widget, root: &gtk::Container) -> Option<gtk::Widget> {
    let root: &gtk::Widget = root.upcast_ref();
    let mut card = widget.clone();
    loop {
        let parent = card.parent()?;
        if &parent == root {
            return Some(card);
        }
        card = parent;
    }
}

fn set_class(widget: &impl IsA<gtk::Widget>, class: &str, on: bool) {
    let context = widget.style_context();
    if on {
        context.add_class(class);
    } else {
        context.remove_class(class);
    }
}

/// Glass under a window of Sysi's own standing in for a card, the one being
/// dragged: drawn by the extension inside that window, it moves with the
/// window in the same frame. The card sits `inset` logical pixels inside the
/// window on every side. `done` hears whether the window can stand in for the
/// card: yes at once when there is no live glass (the card wears its own
/// frosted plate then, and the window shows it), and otherwise whether the
/// glass went on.
pub fn glass_window(
    window: &gtk::Window,
    key: &str,
    inset: i32,
    glass: bool,
    pressed: bool,
    done: Rc<dyn Fn(bool)>,
) {
    if !lifts_supported() && !(glass && glass_is_live()) {
        done(true);
        return;
    }
    // Named as the lifted card, so the extension shows this window only
    // while the overlay does not show the card, switching the two in one
    // frame.
    send_window(
        window.clone(),
        format!("lift:{key}"),
        inset,
        glass,
        pressed,
        done,
        6,
    );
}

fn send_window(
    window: gtk::Window,
    key: String,
    inset: i32,
    glass: bool,
    pressed: bool,
    done: Rc<dyn Fn(bool)>,
    tries: u32,
) {
    let connection = LINK.with(|link| {
        link.borrow()
            .as_ref()
            .and_then(|link| link.connection.borrow().clone())
    });
    let xid = window
        .window()
        .filter(|_| window.is_mapped())
        .and_then(|window| window.downcast::<gdkx11::X11Window>().ok())
        .map(|window| window.xid());
    let (Some(connection), Some(xid)) = (connection, xid) else {
        done(false);
        return;
    };
    let scale = f64::from(window.scale_factor().max(1));
    let (width, height) = (
        f64::from(window.allocated_width()) * scale,
        f64::from(window.allocated_height()) * scale,
    );
    let inset_px = f64::from(inset) * scale;
    let message: Message = (
        xid,
        width,
        height,
        vec![(
            key.clone(),
            inset_px,
            inset_px,
            width - 2.0 * inset_px,
            height - 2.0 * inset_px,
            if glass { RADIUS * scale } else { -1.0 },
            pressed,
        )],
    );
    connection.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        INTERFACE,
        "SetCards",
        Some(&message.to_variant()),
        Some(glib::VariantTy::new("(b)").expect("valid reply type")),
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
        move |reply| {
            let attached = reply
                .ok()
                .and_then(|reply| reply.get::<(bool,)>())
                .is_some_and(|(attached,)| attached);
            // The shell makes the window's actor a moment after it maps.
            if attached || tries == 0 || !window.is_mapped() {
                done(attached);
                return;
            }
            glib::timeout_add_local_once(Duration::from_millis(15), move || {
                send_window(window, key, inset, glass, pressed, done, tries - 1)
            });
        },
    );
}

/// Give a popover glass of its own while the card it belongs to is glass.
/// A popover lives inside the overlay window, so its glass travels with the
/// cards' as one more card, painted last.
pub fn glass_popover(popover: &gtk::Popover) {
    POPOVERS.with(|list| {
        let mut list = list.borrow_mut();
        let serial = NEXT_POPOVER.with(|next| next.replace(next.get() + 1));
        list.push((format!("popover:{serial}"), popover.downgrade()));
    });
    popover.connect_map(|popover| {
        let glass = glass_is_live()
            && root()
                .zip(popover.relative_to())
                .and_then(|(root, anchor)| card_of(&anchor, &root))
                .is_some_and(|card| has_glass(&card));
        set_class(popover, GLASS_POPUP, glass);
    });
    popover.connect_draw(|popover, cr| {
        if let Some(outline) = popover_outline(popover) {
            clear(cr, &outline);
        }
        glib::Propagation::Proceed
    });
}

/// The rounded rectangle a glass popover's glass fills, in its own
/// coordinates: its content plus a margin, not the room left for the arrow.
fn popover_outline(popover: &gtk::Popover) -> Option<Outline> {
    if !popover.style_context().has_class(GLASS_POPUP) {
        return None;
    }
    let content = popover.child()?.allocation();
    Some(Outline::new(
        f64::from(content.x() - POPOVER_PAD),
        f64::from(content.y() - POPOVER_PAD),
        f64::from(content.width() + 2 * POPOVER_PAD),
        f64::from(content.height() + 2 * POPOVER_PAD),
    ))
}

fn popover_samples(root: &gtk::Container) -> Vec<CardSample> {
    POPOVERS.with(|list| {
        let mut list = list.borrow_mut();
        list.retain(|(_, popover)| popover.upgrade().is_some());
        list.iter()
            .filter_map(|(key, popover)| {
                let popover = popover.upgrade()?;
                if !popover.is_visible() || !popover.is_mapped() {
                    return None;
                }
                let outline = popover_outline(&popover)?;
                let (x, y) = popover.translate_coordinates(root, 0, 0)?;
                Some(CardSample {
                    key: key.clone(),
                    x: x + outline.x as i32,
                    y: y + outline.y as i32,
                    width: outline.width as i32,
                    height: outline.height as i32,
                    glass: true,
                    pressed: false,
                })
            })
            .collect()
    })
}

/// Dress a context menu like the card it was opened on: on glass over a
/// glass card, white over a LIGHT one, its dark plate otherwise. A submenu
/// follows the menu it hangs off. A menu is a window of its own, so its
/// glass goes to the extension under the menu's own X window.
pub fn glass_menu(menu: &gtk::Menu) {
    menu.connect_map(|menu| {
        let (glass, light) = match parent_menu(menu) {
            Some(parent) => {
                let parent = parent.style_context();
                (parent.has_class(GLASS_POPUP), parent.has_class(LIGHT_MENU))
            }
            None => card_under_pointer().map_or((false, false), |card| menu_look(&card)),
        };
        let glass = glass && glass_is_live();
        set_class(menu, GLASS_POPUP, glass);
        set_class(menu, LIGHT_MENU, light);
        // A menu keeps its window between openings, and the extension puts
        // the glass it last had back each time that window maps: one that
        // opens without glass says so, or a menu once opened on a glass card
        // would wear it again on a LIGHT one.
        let menu = menu.clone();
        glib::idle_add_local_once(move || send_menu(&menu, glass, if glass { 6 } else { 0 }));
    });
}

/// Dress an open menu again when the card it was opened on changes mode
/// under it: the colour item keeps the menu open, and it would otherwise
/// stay the way it opened until it closed.
pub fn restyle_menu(menu: &gtk::Menu, card: &impl IsA<gtk::Widget>) {
    if !menu.is_mapped() {
        return;
    }
    let (glass, light) = menu_look(card);
    let glass = glass && glass_is_live();
    set_class(menu, LIGHT_MENU, light);
    if glass != menu.style_context().has_class(GLASS_POPUP) {
        set_class(menu, GLASS_POPUP, glass);
        send_menu(menu, glass, 6);
    }
}

/// Whether a menu opened on this card goes on glass, and whether it is white.
fn menu_look(card: &impl IsA<gtk::Widget>) -> (bool, bool) {
    let context = card.style_context();
    (
        has_glass(card),
        context.has_class("mode-light") && context.has_class("mode-solid"),
    )
}

/// The menu a submenu hangs off.
fn parent_menu(menu: &gtk::Menu) -> Option<gtk::Menu> {
    menu.attach_widget()
        .and_then(|item| item.parent())
        .and_then(|parent| parent.downcast::<gtk::Menu>().ok())
}

/// The card under the pointer: menus open where they were asked for.
fn card_under_pointer() -> Option<gtk::Widget> {
    let root = root()?;
    let surface = root.window()?;
    let pointer = root.display().default_seat()?.pointer()?;
    let (_, x, y, _) = surface.device_position(&pointer);
    paint_order(&root).into_iter().rev().find(|card| {
        let rect = card.allocation();
        card.is_mapped()
            && x >= rect.x()
            && y >= rect.y()
            && x < rect.x() + rect.width()
            && y < rect.y() + rect.height()
    })
}

/// Ask for glass under a menu's window, or (`on` false) for it to go. The
/// extension only finds the window once the compositor has mapped it, a
/// moment after GTK has, so a miss is asked again a few times before the menu
/// falls back to its own plate.
fn send_menu(menu: &gtk::Menu, on: bool, tries: u32) {
    // Taken off glass since this was asked for (its card changed mode).
    if on && !menu.style_context().has_class(GLASS_POPUP) {
        return;
    }
    let Some(connection) = LINK.with(|link| {
        link.borrow()
            .as_ref()
            .and_then(|link| link.connection.borrow().clone())
    }) else {
        set_class(menu, GLASS_POPUP, false);
        return;
    };
    let Some(top) = menu.toplevel().filter(|top| top.is_mapped()) else {
        return;
    };
    let Some(xid) = top
        .window()
        .and_then(|window| window.downcast::<gdkx11::X11Window>().ok())
        .map(|window| window.xid())
    else {
        return;
    };
    let scale = f64::from(top.scale_factor().max(1));
    // The popup window leaves the theme room for a shadow around the menu;
    // the glass goes under the menu itself.
    let Some((x, y)) = menu.translate_coordinates(&top, 0, 0) else {
        return;
    };
    let body = menu.allocation();
    let message: Message = (
        xid,
        f64::from(top.allocated_width()) * scale,
        f64::from(top.allocated_height()) * scale,
        on.then(|| {
            (
                "menu".into(),
                f64::from(x) * scale,
                f64::from(y) * scale,
                f64::from(body.width()) * scale,
                f64::from(body.height()) * scale,
                MENU_RADIUS * scale,
                false,
            )
        })
        .into_iter()
        .collect(),
    );
    let menu = menu.clone();
    connection.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        INTERFACE,
        "SetCards",
        Some(&message.to_variant()),
        Some(glib::VariantTy::new("(b)").expect("valid reply type")),
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
        move |reply| {
            let attached = reply
                .ok()
                .and_then(|reply| reply.get::<(bool,)>())
                .is_some_and(|(attached,)| attached);
            // Taken off glass, or put on it since: nothing to ask again.
            if attached || !on || !menu.is_mapped() || !menu.style_context().has_class(GLASS_POPUP)
            {
                return;
            }
            if tries == 0 {
                set_class(&menu, GLASS_POPUP, false);
                return;
            }
            glib::timeout_add_local_once(Duration::from_millis(30), move || {
                send_menu(&menu, true, tries - 1)
            });
        },
    );
}

/// The children of the overlay in the order GTK really paints them. A card
/// with GdkWindows of its own (a note, or the canvas inside SYSTEM) is painted
/// with those windows, in their stacking order, after everything drawn
/// straight onto the overlay's window; the container's child order only
/// ranks the rest. Raising a card restacks its windows without always moving
/// it in the child list, so the two part ways.
pub fn paint_order(parent: &gtk::Container) -> Vec<gtk::Widget> {
    let Some(surface) = parent.window() else {
        return parent.children();
    };
    // Topmost first.
    let stack = surface.children();
    let mut ranked: Vec<(Option<usize>, gtk::Widget)> = parent
        .children()
        .into_iter()
        .map(|child| {
            let mut windows = Vec::new();
            own_windows(&child, &mut windows);
            let topmost = windows
                .iter()
                .filter_map(|window| stack.iter().position(|candidate| candidate == window))
                .min();
            (topmost.map(|index| stack.len() - index), child)
        })
        .collect();
    // Stable: windowless cards keep their child order, beneath the rest.
    ranked.sort_by_key(|(height, _)| height.unwrap_or(0));
    ranked.into_iter().map(|(_, child)| child).collect()
}

/// The outermost GdkWindows a widget paints into: its own, or else those of
/// its nearest descendants that have one.
fn own_windows(widget: &gtk::Widget, out: &mut Vec<gdk::Window>) {
    if widget.has_window() {
        out.extend(widget.window());
        return;
    }
    if let Some(container) = widget.downcast_ref::<gtk::Container>() {
        for child in container.children() {
            own_windows(&child, out);
        }
    }
}

/// Wipe what the cards beneath painted where this glass card sits. Glass
/// cards are clear and the glass is drawn under the whole window, so without
/// this a lower card's text shows straight through the upper one.
///
/// Every card, windows and all, paints into the overlay's one surface in
/// paint order, so clearing here, before this card paints anything of its
/// own, removes exactly the lower cards' pixels. Clipping the lower cards
/// instead cannot work: GTK paints their child windows with a clip of their
/// own, and skips the draw signal of widgets nothing listens to.
pub fn clear_below(card: &gtk::Widget, cr: &Context) {
    if !has_glass(card) {
        return;
    }
    let rect = card.allocation();
    clear(
        cr,
        &Outline::new(0.0, 0.0, f64::from(rect.width()), f64::from(rect.height())),
    );
}

fn clear(cr: &Context, outline: &Outline) {
    cr.new_path();
    outline.trace(cr);
    cr.save().ok();
    cr.set_operator(gtk::cairo::Operator::Clear);
    let _ = cr.fill();
    cr.restore().ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(key: &str, x: i32, y: i32, width: i32, height: i32) -> CardSample {
        CardSample {
            key: key.into(),
            x,
            y,
            width,
            height,
            pressed: false,
            glass: true,
        }
    }

    #[test]
    fn cards_go_out_in_paint_order_in_x11_pixels() {
        let cards = wire_cards(
            &[
                card("note:1", 10, 20, 218, 124),
                card("usage", 0, 0, 196, 76),
            ],
            2.0,
        );
        assert_eq!(
            cards,
            vec![
                ("note:1".into(), 20.0, 40.0, 436.0, 248.0, 28.0, false),
                ("usage".into(), 0.0, 0.0, 392.0, 152.0, 28.0, false),
            ]
        );
    }

    #[test]
    fn a_card_without_glass_goes_with_a_negative_radius() {
        let bare = CardSample {
            glass: false,
            ..card("usage", 0, 0, 100, 50)
        };
        assert_eq!(wire_cards(&[bare], 1.0)[0].5, -1.0);
    }

    #[test]
    fn hidden_cards_are_left_out() {
        let cards = wire_cards(&[card("usage", 5, 5, 1, 1)], 1.0);
        assert!(cards.is_empty());
    }

    #[test]
    fn a_small_plate_never_rounds_past_its_own_middle() {
        let plate = Outline::new(0.0, 0.0, 40.0, 20.0);
        assert_eq!(plate.radius, 10.0);
    }

    #[test]
    fn the_message_has_the_signature_the_extension_expects() {
        let message: Message = (
            7,
            1280.0,
            768.0,
            wire_cards(&[card("a", 1, 2, 30, 40)], 1.0),
        );
        assert_eq!(message.to_variant().type_().as_str(), "(tdda(sdddddb))");
    }
}
