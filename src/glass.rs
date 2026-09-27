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

/// How a card's glass is cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    /// A rounded plate: notes, usage, dictionaries, the system card.
    Plate,
    /// The disc behind the ring, ticks and arc timers.
    Round,
    /// The capsule behind the digital timer.
    Pill,
}

impl Shape {
    pub fn of(widget: &impl IsA<gtk::Widget>) -> Self {
        let context = widget.style_context();
        if !context.has_class("timer-card") {
            Self::Plate
        } else if context.has_class("timer-style-digital") {
            Self::Pill
        } else {
            Self::Round
        }
    }
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
    pub fn new(shape: Shape, x: f64, y: f64, width: f64, height: f64) -> Self {
        match shape {
            Shape::Plate => Self {
                x,
                y,
                width,
                height,
                radius: RADIUS.min(width / 2.0).min(height / 2.0),
            },
            // The disc inscribed in the card: a timer is square, but a card
            // mid-resize need not be.
            Shape::Round => {
                let side = width.min(height);
                Self {
                    x: x + (width - side) / 2.0,
                    y: y + (height - side) / 2.0,
                    width: side,
                    height: side,
                    radius: side / 2.0,
                }
            }
            Shape::Pill => Self {
                x,
                y,
                width,
                height,
                radius: width.min(height) / 2.0,
            },
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
    pub shape: Shape,
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
                card.shape,
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
    connection: RefCell<Option<gio::DBusConnection>>,
    last: RefCell<Option<Message>>,
    /// Bumped on every send; only the newest reply may change `glass-live`.
    serial: Cell<u64>,
    retry: Cell<bool>,
}

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
/// after every painted frame.
pub fn start(window: &gtk::Window, collect: impl Fn() -> Vec<CardSample> + 'static) {
    let link = Rc::new(Link {
        window: window.clone(),
        connection: RefCell::new(None),
        last: RefCell::new(None),
        serial: Cell::new(0),
        retry: Cell::new(false),
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
                link.window.queue_draw();
            })
        },
        |_, _| {
            with_link(|link| {
                link.connection.replace(None);
                link.last.replace(None);
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
            wire_cards(&collect(), scale),
        );
        if link.last.borrow().as_ref() == Some(&message) {
            return;
        }
        send(&link, message);
    });
}

fn send(link: &Rc<Link>, message: Message) {
    let Some(connection) = link.connection.borrow().clone() else {
        return;
    };
    let serial = link.serial.get() + 1;
    link.serial.set(serial);
    let parameters = message.to_variant();
    link.last.replace(Some(message));
    let weak = Rc::downgrade(link);
    connection.call(
        Some(BUS_NAME),
        OBJECT_PATH,
        INTERFACE,
        "SetCards",
        Some(&parameters),
        Some(glib::VariantTy::new("(b)").expect("valid reply type")),
        gio::DBusCallFlags::NONE,
        1000,
        None::<&gio::Cancellable>,
        move |reply| {
            let Some(link) = weak.upgrade() else {
                return;
            };
            if link.serial.get() != serial {
                return;
            }
            let attached = reply
                .ok()
                .and_then(|reply| reply.get::<(bool,)>())
                .is_some_and(|(attached,)| attached);
            set_live(&link.window, attached);
            if !attached {
                retry_once(&link);
            }
        },
    );
}

/// The extension may not have found the window yet (it maps a moment after
/// Sysi first paints). Ask again once, a little later.
fn retry_once(link: &Rc<Link>) {
    if link.retry.replace(true) {
        return;
    }
    let weak = Rc::downgrade(link);
    glib::timeout_add_local_once(Duration::from_millis(250), move || {
        if let Some(link) = weak.upgrade() {
            link.retry.set(false);
            link.last.replace(None);
            link.window.queue_draw();
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

/// Keep a card from painting where a glass card above it sits. Glass cards
/// are clear, and the glass is drawn under the whole window, so without this
/// the lower card's text would show straight through the upper card.
pub fn clip_under_glass(widget: &gtk::Widget, cr: &Context) {
    let Some(parent) = widget
        .parent()
        .and_then(|parent| parent.downcast::<gtk::Container>().ok())
    else {
        return;
    };
    let own = widget.allocation();
    let siblings = parent.children();
    let Some(index) = siblings.iter().position(|child| child == widget) else {
        return;
    };
    for above in &siblings[index + 1..] {
        if !above.is_visible() || !above.is_mapped() || above.widget_name() == "dictate" {
            continue;
        }
        if !above.style_context().has_class("mode-glass") {
            continue;
        }
        let rect = above.allocation();
        if rect.width() <= 1 || rect.height() <= 1 || rect.intersect(&own).is_none() {
            continue;
        }
        let outline = Outline::new(
            Shape::of(above),
            f64::from(rect.x() - own.x()),
            f64::from(rect.y() - own.y()),
            f64::from(rect.width()),
            f64::from(rect.height()),
        );
        cr.rectangle(0.0, 0.0, f64::from(own.width()), f64::from(own.height()));
        outline.trace(cr);
        cr.set_fill_rule(gtk::cairo::FillRule::EvenOdd);
        cr.clip();
    }
    cr.set_fill_rule(gtk::cairo::FillRule::Winding);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(key: &str, x: i32, y: i32, width: i32, height: i32, shape: Shape) -> CardSample {
        CardSample {
            key: key.into(),
            x,
            y,
            width,
            height,
            shape,
            pressed: false,
        }
    }

    #[test]
    fn cards_go_out_in_paint_order_in_x11_pixels() {
        let cards = wire_cards(
            &[
                card("note:1", 10, 20, 218, 124, Shape::Plate),
                card("system", 0, 0, 196, 76, Shape::Plate),
            ],
            2.0,
        );
        assert_eq!(
            cards,
            vec![
                ("note:1".into(), 20.0, 40.0, 436.0, 248.0, 28.0, false),
                ("system".into(), 0.0, 0.0, 392.0, 152.0, 28.0, false),
            ]
        );
    }

    #[test]
    fn hidden_cards_are_left_out() {
        let cards = wire_cards(&[card("usage", 5, 5, 1, 1, Shape::Plate)], 1.0);
        assert!(cards.is_empty());
    }

    #[test]
    fn timers_get_a_disc_or_a_capsule() {
        let disc = Outline::new(Shape::Round, 0.0, 0.0, 120.0, 116.0);
        assert_eq!(
            (disc.x, disc.y, disc.width, disc.radius),
            (2.0, 0.0, 116.0, 58.0)
        );
        let pill = Outline::new(Shape::Pill, 0.0, 0.0, 84.0, 36.0);
        assert_eq!((pill.width, pill.height, pill.radius), (84.0, 36.0, 18.0));
    }

    #[test]
    fn a_small_plate_never_rounds_past_its_own_middle() {
        let plate = Outline::new(Shape::Plate, 0.0, 0.0, 40.0, 20.0);
        assert_eq!(plate.radius, 10.0);
    }

    #[test]
    fn the_message_has_the_signature_the_extension_expects() {
        let message: Message = (
            7,
            1280.0,
            768.0,
            wire_cards(&[card("a", 1, 2, 30, 40, Shape::Plate)], 1.0),
        );
        assert_eq!(message.to_variant().type_().as_str(), "(tdda(sdddddb))");
    }
}
