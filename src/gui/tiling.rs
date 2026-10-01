//! Responsive auto layout shared by project workspaces and the Agents wall.
//! Only topology changes reparent widgets; allocation changes resize existing
//! dividers in place. Neither operation recreates a terminal or its process.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};

use super::split::{self, Axis, Node};

/// Reflow only when the preferred row counts change. The callback replaces
/// the owner's divider index, keeping keyboard navigation on current widgets.
pub(super) fn automatic(
    panels: Vec<gtk::Widget>,
    window: &adw::ApplicationWindow,
    on_dividers: impl Fn(Vec<gtk::Paned>) + 'static,
) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.set_hexpand(true);
    root.set_vexpand(true);
    let panels: Vec<_> = panels.into_iter().map(Rc::new).collect();
    // Attach synchronously: callers focus a newly opened program immediately
    // after adding this root. Its first allocation refines the initial plan.
    let mut initial_dividers = Vec::new();
    if let Some(tree) = split::auto_node(&panels, window.width(), window.height()) {
        root.append(&build(&tree, window, &mut initial_dividers));
    }
    on_dividers(initial_dividers.clone());
    let dividers = RefCell::new(initial_dividers);
    let shape = RefCell::new(split::auto_rows(
        panels.len(),
        window.width(),
        window.height(),
    ));
    let window = window.downgrade();
    let last_size = Cell::new((0, 0));
    root.add_tick_callback(move |root, _| {
        if !root.is_mapped() {
            return glib::ControlFlow::Continue;
        }
        let width = root.width();
        let height = root.height();
        if width <= 0 || height <= 0 {
            return glib::ControlFlow::Continue;
        }
        if last_size.replace((width, height)) == (width, height) {
            return glib::ControlFlow::Continue;
        }
        let next = split::auto_rows(panels.len(), width, height);
        if next != *shape.borrow() {
            let Some(window) = window.upgrade() else {
                return glib::ControlFlow::Break;
            };
            // Focus belongs to the program, not to the containers we replace.
            let focus = window
                .focus_widget()
                .filter(|focus| focus.is_ancestor(root));
            let divider_index = focus.as_ref().and_then(|focus| {
                dividers
                    .borrow()
                    .iter()
                    .position(|divider| focus == divider.upcast_ref::<gtk::Widget>())
            });
            if focus.is_some() {
                gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
            }
            detach_dividers(&dividers.borrow());
            for panel in &panels {
                panel.unparent();
            }
            while let Some(child) = root.first_child() {
                root.remove(&child);
            }
            let mut next_dividers = Vec::new();
            if let Some(tree) = split::auto_node(&panels, width, height) {
                root.append(&build(&tree, &window, &mut next_dividers));
            }
            on_dividers(next_dividers.clone());
            if let Some(focus) = focus {
                if focus.is_ancestor(root) {
                    focus.grab_focus();
                } else if let Some(index) = divider_index {
                    if let Some(divider) =
                        next_dividers.get(index.min(next_dividers.len().saturating_sub(1)))
                    {
                        divider.grab_focus();
                    }
                }
            }
            *dividers.borrow_mut() = next_dividers;
            *shape.borrow_mut() = next;
        }
        glib::ControlFlow::Continue
    });
    root.upcast()
}

fn build(
    node: &Node<gtk::Widget>,
    window: &adw::ApplicationWindow,
    dividers: &mut Vec<gtk::Paned>,
) -> gtk::Widget {
    match node {
        Node::Leaf(widget) => widget.as_ref().clone(),
        Node::Split {
            axis,
            ratio,
            first,
            second,
            ..
        } => {
            let orientation = match axis {
                Axis::Horizontal => gtk::Orientation::Horizontal,
                Axis::Vertical => gtk::Orientation::Vertical,
            };
            let paned = gtk::Paned::new(orientation);
            paned.set_wide_handle(true);
            paned.set_hexpand(true);
            paned.set_vexpand(true);
            paned.set_resize_start_child(true);
            paned.set_resize_end_child(true);
            // Auto mode must fit even when there are more panels than can have
            // comfortable minimum sizes. Zoom remains the way to work closely.
            paned.set_shrink_start_child(true);
            paned.set_shrink_end_child(true);
            paned.set_start_child(Some(&build(first, window, dividers)));
            paned.set_end_child(Some(&build(second, window, dividers)));
            fit_divider(&paned, *ratio, None);
            keyboard_resize(&paned, window);
            dividers.push(paned.clone());
            paned.upcast()
        }
    }
}

/// Initialize against this divider's actual allocation, then retain the
/// user's local share on resize. Saved legacy positions are consumed once.
pub(super) fn fit_divider(paned: &gtk::Paned, initial: f64, saved: Option<i32>) {
    let share = Rc::new(Cell::new(initial));
    let extent = Rc::new(Cell::new(0));
    let applying = Rc::new(Cell::new(false));
    let share_for_position = share.clone();
    let extent_for_position = extent.clone();
    let applying_for_position = applying.clone();
    paned.connect_position_notify(move |paned| {
        let size = available(paned);
        if !applying_for_position.get() && size > 0 && size == extent_for_position.get() {
            share_for_position.set((paned.position() as f64 / size as f64).clamp(0.0, 1.0));
        }
    });
    let saved = Cell::new(saved);
    paned.add_tick_callback(move |paned, _| {
        let size = available(paned);
        if size > 0 && size != extent.get() {
            let position = saved
                .take()
                .unwrap_or((size as f64 * share.get()).round() as i32)
                .clamp(0, size);
            applying.set(true);
            paned.set_position(position);
            applying.set(false);
            extent.set(size);
            share.set(position as f64 / size as f64);
        }
        glib::ControlFlow::Continue
    });
}

fn available(paned: &gtk::Paned) -> i32 {
    let (size, handle) = match paned.orientation() {
        gtk::Orientation::Horizontal => (paned.width(), paned.is_wide_handle()),
        _ => (paned.height(), paned.is_wide_handle()),
    };
    // GTK's max-position excludes the handle, but also includes child minima.
    // Measuring the actual separator gives the usable local split extent.
    let mut separator = paned.first_child();
    while let Some(widget) = separator.as_ref() {
        if widget.css_name() == "separator" {
            break;
        }
        separator = widget.next_sibling();
    }
    let handle_size = separator
        .map(|widget| match paned.orientation() {
            gtk::Orientation::Horizontal => widget.width(),
            _ => widget.height(),
        })
        .unwrap_or(if handle { 5 } else { 1 });
    (size - handle_size.max(0)).max(0)
}

pub(super) fn keyboard_resize(paned: &gtk::Paned, window: &adw::ApplicationWindow) {
    paned.set_focusable(true);
    paned.set_tooltip_text(Some("Focusable divider — use arrow keys to resize"));
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak_paned = paned.downgrade();
    let window = window.downgrade();
    keys.connect_key_pressed(move |_, key, _, _| {
        let (Some(paned), Some(window)) = (weak_paned.upgrade(), window.upgrade()) else {
            return glib::Propagation::Proceed;
        };
        if window.focus_widget().as_ref() != Some(paned.upcast_ref()) {
            return glib::Propagation::Proceed;
        }
        let delta = match (paned.orientation(), key) {
            (gtk::Orientation::Horizontal, gdk::Key::Left)
            | (gtk::Orientation::Vertical, gdk::Key::Up) => -16,
            (gtk::Orientation::Horizontal, gdk::Key::Right)
            | (gtk::Orientation::Vertical, gdk::Key::Down) => 16,
            _ => return glib::Propagation::Proceed,
        };
        paned.set_position(paned.position().saturating_add(delta));
        glib::Propagation::Stop
    });
    paned.add_controller(keys);
}

/// Clear container-owned child references before moving widgets. Merely
/// unparenting leaves GtkPaned's cached child references alive; its later
/// disposal can otherwise unparent a panel from the new arrangement.
pub(super) fn detach_dividers(dividers: &[gtk::Paned]) {
    for divider in dividers {
        divider.set_start_child(None::<&gtk::Widget>);
        divider.set_end_child(None::<&gtk::Widget>);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn settle() {
        let until = Instant::now() + Duration::from_millis(180);
        while Instant::now() < until {
            while glib::MainContext::default().iteration(false) {}
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn wait_for_panels(root: &gtk::Widget, panels: &[gtk::Widget]) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while !panels.iter().all(|panel| {
            panel
                .compute_bounds(root)
                .is_some_and(|rect| rect.width() > 0.0 && rect.height() > 0.0)
        }) {
            assert!(
                Instant::now() < deadline,
                "panels never allocated; connect a Broadway browser client"
            );
            while glib::MainContext::default().iteration(false) {}
            std::thread::sleep(Duration::from_millis(5));
        }
        settle();
    }

    #[test]
    #[ignore = "requires a private D-Bus session and GTK display"]
    fn responsive_panels_fit_and_reflow_without_replacing_widgets() {
        adw::init().unwrap();
        let app = adw::Application::builder()
            .application_id("dev.omarchy.Radar.TilingTest")
            .build();
        app.register(None::<&gtk::gio::Cancellable>).unwrap();
        let window = adw::ApplicationWindow::builder()
            .application(&app)
            .default_width(1200)
            .default_height(800)
            .build();
        let panels: Vec<gtk::Widget> = (0..9)
            .map(|i| {
                let entry = gtk::Entry::new();
                entry.set_text(&format!("panel-{i}"));
                entry.set_hexpand(true);
                entry.set_vexpand(true);
                entry.upcast()
            })
            .collect();
        window.present();
        for count in [1, 2, 3, 4, 5, 9, 4, 1] {
            let previous_focus = window.focus_widget();
            gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
            window.set_content(None::<&gtk::Widget>);
            for panel in &panels {
                if panel.parent().is_some() {
                    panel.unparent();
                }
            }
            let root = automatic(panels[..count].to_vec(), &window, |_| {});
            assert!(
                panels[0].parent().is_some(),
                "initial panel must attach synchronously"
            );
            window.set_content(Some(&root));
            if let Some(focus) = previous_focus {
                if focus.is_ancestor(&root) {
                    focus.grab_focus();
                }
            }
            wait_for_panels(&root, &panels[..count]);
            panels[0].grab_focus();
            assert!(window
                .focus_widget()
                .is_some_and(|focus| focus.is_ancestor(&panels[0]) || focus == panels[0]));
            let width = root.width() as f32;
            let height = root.height() as f32;
            let rects: Vec<_> = panels[..count]
                .iter()
                .map(|panel| {
                    panel.compute_bounds(&root).unwrap_or_else(|| {
                        panic!(
                            "panel missing for {count}; root {}x{}, mapped {}",
                            root.width(),
                            root.height(),
                            root.is_mapped()
                        )
                    })
                })
                .collect();
            for rect in &rects {
                assert!(
                    rect.width() > 0.0 && rect.height() > 0.0,
                    "zero panel for {count}"
                );
                assert!(rect.x() >= -1.0 && rect.y() >= -1.0);
                assert!(
                    rect.x() + rect.width() <= width + 1.0,
                    "horizontal overflow for {count}"
                );
                assert!(
                    rect.y() + rect.height() <= height + 1.0,
                    "vertical overflow for {count}"
                );
            }
            let area: f32 = rects.iter().map(|r| r.width() * r.height()).sum();
            assert!(
                area >= width * height * 0.8,
                "unused space for {count}: {area} / {}",
                width * height
            );
            assert_eq!(
                panels[0].clone().downcast::<gtk::Entry>().unwrap().text(),
                "panel-0"
            );
        }
        // A wide viewport chooses a row; a tall viewport chooses a column.
        gtk::prelude::GtkWindowExt::set_focus(&window, None::<&gtk::Widget>);
        window.set_content(None::<&gtk::Widget>);
        for panel in &panels {
            if panel.parent().is_some() {
                panel.unparent();
            }
        }
        let root = automatic(panels[..4].to_vec(), &window, |_| {});
        window.set_content(Some(&root));
        window.set_default_size(2200, 400);
        wait_for_panels(&root, &panels[..4]);
        let wide = panels[0].compute_bounds(&root).unwrap();
        assert!(wide.width() < root.width() as f32 / 2.0);
        panels[0].grab_focus();
        window.set_default_size(500, 1500);
        settle();
        let tall = panels[0].compute_bounds(&root).unwrap();
        assert!(tall.height() < root.height() as f32 / 2.0);
        assert!(
            tall.width() > root.width() as f32 * 0.9,
            "tall viewport {}x{}, panel {}x{}",
            root.width(),
            root.height(),
            tall.width(),
            tall.height()
        );
        assert!(
            window
                .focus_widget()
                .is_some_and(|focus| focus.is_ancestor(&panels[0]) || focus == panels[0]),
            "reflow lost input focus"
        );
        root.first_child().unwrap().grab_focus();
        window.set_default_size(2200, 400);
        settle();
        assert!(
            window
                .focus_widget()
                .is_some_and(|focus| focus.is::<gtk::Paned>() && focus.is_ancestor(&root)),
            "reflow lost divider focus"
        );
        // Manual dividers retain their local proportions across a resize,
        // including a legacy saved pixel position on the first allocation.
        let manual = gtk::Paned::new(gtk::Orientation::Horizontal);
        manual.set_start_child(Some(&gtk::Label::new(Some("left"))));
        manual.set_end_child(Some(&gtk::Label::new(Some("right"))));
        manual.set_resize_start_child(true);
        manual.set_resize_end_child(true);
        fit_divider(&manual, 0.5, Some(180));
        window.set_content(Some(&manual));
        window.set_default_size(1000, 600);
        settle();
        assert!((manual.position() - 180).abs() <= 2);
        manual.set_position(240);
        settle();
        let share = manual.position() as f64 / available(&manual) as f64;
        window.set_default_size(1800, 700);
        settle();
        let resized = manual.position() as f64 / available(&manual) as f64;
        assert!(
            (resized - share).abs() < 0.03,
            "manual share changed: {share} -> {resized}"
        );
        window.close();
    }
}
