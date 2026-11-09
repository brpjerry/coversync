//! Keeping the page's frames at the display's rate while it scrolls or
//! answers a click or a key.
//!
//! WebKitGTK 6.0 (2.54) paces a page with nothing animating on it well below
//! the display: 30 frames a second when idle, and only about 60 to 90 on a
//! 120 Hz display while a touchpad scrolls it, since it applies each scroll
//! delta as it arrives rather than once a frame. With an animation running,
//! it renders every refresh. An animation that starts from idle, such as an
//! album opening on a click, gets its first frame late and a few more slowly
//! before the page is paced at the display's rate. So while scroll input,
//! presses and keys come in, and for a while after them for the momentum or
//! the animation they started, a user style sheet keeps an invisible
//! animation running; idle, there's none, and the page costs nothing.
//!
//! Measured on a 120 Hz laptop (Hyprland), with two-finger drags: on a plain
//! page, about 70 frames a second (16.5 ms between most frames) without it,
//! 120 with it (99% of frames within 10 ms), as in Firefox; on coversync's
//! album grid, 92-105 without it, 104-113 with it. An album opened with a
//! click from idle drew its first frame 60 ms after the click, and one frame
//! of 20-28 ms early on; one opened while the page was already animating,
//! at once and at 120. Left on all the time it would cost a frame every
//! refresh, idle: about a fifth of a core for the app and as much for the
//! compositor.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;
use gtk::prelude::*;

/// A transparent pixel whose transform animates: enough for WebKit to render
/// every refresh, with nothing to see. (One with `opacity: 0` doesn't count.)
const SHEET: &str = "\
:root::after {
    content: \"\";
    position: fixed;
    top: 0;
    left: 0;
    width: 1px;
    height: 1px;
    pointer-events: none;
    animation: coversync-keep-frames 1s linear infinite;
}
@keyframes coversync-keep-frames {
    to { transform: translateX(1px); }
}";

/// How long after the last input the animation keeps running: a flick's
/// momentum goes on after the fingers lift, and an album takes about a
/// second to open or close.
const LINGER: Duration = Duration::from_millis(2000);
/// How often, while it runs, whether it's time to stop is checked.
const CHECK: Duration = Duration::from_millis(250);

struct Pacing {
    content: webkit::UserContentManager,
    sheet: webkit::UserStyleSheet,
    running: Cell<bool>,
    last_input: Cell<Instant>,
}

/// Watches scroll input, presses and keys to `widget` (the view, or a widget
/// around it), and runs the animation on `content`'s pages while they last.
/// The input still goes on to the view.
pub fn install(widget: &impl IsA<gtk::Widget>, content: &webkit::UserContentManager) {
    let pacing = Rc::new(Pacing {
        content: content.clone(),
        sheet: webkit::UserStyleSheet::new(
            SHEET,
            webkit::UserContentInjectedFrames::TopFrame,
            webkit::UserStyleLevel::User,
            &[],
            &[],
        ),
        running: Cell::new(false),
        last_input: Cell::new(Instant::now()),
    });
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::BOTH_AXES);
    // Before the view's own controller, which takes the events.
    scroll.set_propagation_phase(gtk::PropagationPhase::Capture);
    scroll.connect_scroll_begin({
        let pacing = pacing.clone();
        move |_| input(&pacing)
    });
    scroll.connect_scroll({
        let pacing = pacing.clone();
        move |_, _, _| {
            input(&pacing);
            glib::Propagation::Proceed
        }
    });
    widget.add_controller(scroll);
    // A press rather than the click: the page acts on the release, by which
    // time the animation runs.
    let presses = gtk::EventControllerLegacy::new();
    presses.set_propagation_phase(gtk::PropagationPhase::Capture);
    presses.connect_event(move |_, event| {
        if matches!(
            event.event_type(),
            gtk::gdk::EventType::ButtonPress
                | gtk::gdk::EventType::TouchBegin
                | gtk::gdk::EventType::KeyPress
        ) {
            input(&pacing);
        }
        glib::Propagation::Proceed
    });
    widget.add_controller(presses);
}

fn input(pacing: &Rc<Pacing>) {
    pacing.last_input.set(Instant::now());
    if pacing.running.replace(true) {
        return;
    }
    pacing.content.add_style_sheet(&pacing.sheet);
    let weak = Rc::downgrade(pacing);
    glib::timeout_add_local(CHECK, move || {
        let Some(pacing) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if pacing.last_input.get().elapsed() < LINGER {
            return glib::ControlFlow::Continue;
        }
        pacing.content.remove_style_sheet(&pacing.sheet);
        pacing.running.set(false);
        glib::ControlFlow::Break
    });
}
