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
            let outline = Outline::new(
                f64::from(card.x),
                f64::from(card.y),
                f64::from(card.width),
                f64::from(card.height),
            );
            (
                card.key.clone(),
                outline.x * scale,
                outline.y * scale,
                outline.width * scale,
                outline.height * scale,
                outline.radius * scale,
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
    });
    LINK.with(|slot| *slot.borrow_mut() = Some(link.clone()));
    gio::bus_watch_name(
        gio::BusType::Session,
        BUS_NAME,
        gio::BusNameWatcherFlags::NONE,
        |connection, _, _| {
            with_link(|link| {
                link.connection.replace(Some(connection));
                link.last.replace(None);
                link.retries.set(0);
                link.window.queue_draw();
            })
        },
        |_, _| {
            with_link(|link| {
                link.connection.replace(None);
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
        if link.connection.borrow().is_none() {
            return;
        }
        let Some(xid) = link
            .window
            .window()
            .and_then(|window| window.downcast::<gdkx11::X11Window>().ok())
            .map(|window| window.xid())
        else {
            return;
        };
        let scale = f64::from(link.window.scale_factor().max(1));
        let message = (
            xid,
            f64::from(link.window.allocated_width()) * scale,
            f64::from(link.window.allocated_height()) * scale,
            wire_cards(&[collect(), popover_samples(&link.root)].concat(), scale),
        );
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
/// The inset a popover's glass keeps around its content.
const POPOVER_PAD: i32 = 6;
/// Matches `.sysi-menu`'s corner radius.
const MENU_RADIUS: f64 = 8.0;

thread_local! {
    static POPOVERS: RefCell<Vec<(String, glib::WeakRef<gtk::Popover>)>> =
        const { RefCell::new(Vec::new()) };
    static NEXT_POPOVER: Cell<u64> = const { Cell::new(0) };
}

fn glass_is_live() -> bool {
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
                    pressed: false,
                })
            })
            .collect()
    })
}

/// Give a context menu glass while it was opened on a glass card; a
/// submenu follows the menu it hangs off. A menu is a window of its own, so
/// its glass goes to the extension under the menu's own X window.
pub fn glass_menu(menu: &gtk::Menu) {
    menu.connect_map(|menu| {
        let glass = glass_is_live() && menu_opened_on_glass(menu);
        set_class(menu, GLASS_POPUP, glass);
        if glass {
            let menu = menu.clone();
            glib::idle_add_local_once(move || send_menu(&menu, true, 6));
        }
    });
}

/// Put an open menu on glass or take it off it when the card it was opened
/// on changes mode under it: the colour item keeps the menu open, and it
/// would otherwise stay the way it opened until it closed.
pub fn restyle_menu(menu: &gtk::Menu, card: &impl IsA<gtk::Widget>) {
    let glass = glass_is_live() && has_glass(card);
    if !menu.is_mapped() || glass == menu.style_context().has_class(GLASS_POPUP) {
        return;
    }
    set_class(menu, GLASS_POPUP, glass);
    send_menu(menu, glass, 6);
}

fn menu_opened_on_glass(menu: &gtk::Menu) -> bool {
    if let Some(parent) = menu
        .attach_widget()
        .and_then(|item| item.parent())
        .and_then(|parent| parent.downcast::<gtk::Menu>().ok())
    {
        return parent.style_context().has_class(GLASS_POPUP);
    }
    let Some(root) = root() else {
        return false;
    };
    // Menus open under the pointer, on the card it is over.
    let Some(surface) = root.window() else {
        return false;
    };
    let Some(pointer) = root
        .display()
        .default_seat()
        .and_then(|seat| seat.pointer())
    else {
        return false;
    };
    let (_, x, y, _) = surface.device_position(&pointer);
    paint_order(&root)
        .into_iter()
        .rev()
        .find(|card| {
            let rect = card.allocation();
            card.is_mapped()
                && x >= rect.x()
                && y >= rect.y()
                && x < rect.x() + rect.width()
                && y < rect.y() + rect.height()
        })
        .is_some_and(|card| has_glass(&card))
}

/// Ask for glass under a menu's window, or (`on` false) for it to go. The
/// extension only finds the window once the compositor has mapped it, a
/// moment after GTK has, so a miss is asked again a few times before the menu
/// falls back to its own plate.
fn send_menu(menu: &gtk::Menu, on: bool, tries: u32) {
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
