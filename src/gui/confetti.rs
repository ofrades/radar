//! A short confetti burst for a finished to-do.
//!
//! Completing a card closes it for good: it leaves the lanes and the project
//! view. The burst is the one flourish — a non-interactive layer painted over
//! the whole window for a moment, then gone. It holds no state once it ends.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

const DURATION: f64 = 2.2;
const PARTICLES: usize = 150;
const GRAVITY: f64 = 900.0;

struct Particle {
    x: f64,
    y: f64,
    vx: f64,
    vy: f64,
    size: f64,
    rot: f64,
    vrot: f64,
    r: f64,
    g: f64,
    b: f64,
}

#[derive(Default)]
struct State {
    particles: Vec<Particle>,
    elapsed: f64,
    last: f64,
    running: bool,
}

pub struct Confetti {
    area: gtk::DrawingArea,
    state: Rc<RefCell<State>>,
    seed: Rc<RefCell<u64>>,
}

impl Confetti {
    pub fn new() -> Self {
        let area = gtk::DrawingArea::new();
        area.set_can_target(false);
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_visible(false);
        let state = Rc::new(RefCell::new(State::default()));
        {
            let state = state.clone();
            area.set_draw_func(move |_, cr, _, _| {
                let state = state.borrow();
                for particle in &state.particles {
                    let _ = cr.save();
                    cr.translate(particle.x, particle.y);
                    cr.rotate(particle.rot);
                    cr.set_source_rgb(particle.r, particle.g, particle.b);
                    cr.rectangle(
                        -particle.size / 2.0,
                        -particle.size / 2.0,
                        particle.size,
                        particle.size * 0.62,
                    );
                    let _ = cr.fill();
                    let _ = cr.restore();
                }
            });
        }
        Self {
            area,
            state,
            seed: Rc::new(RefCell::new(seed())),
        }
    }

    /// The layer to stack over the window content. It never takes input.
    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Throw a burst across the window. Safe to call again mid-flight.
    pub fn celebrate(&self) {
        let width = match self.area.width() {
            width if width > 0 => width as f64,
            _ => 1200.0,
        };
        {
            let mut state = self.state.borrow_mut();
            state.elapsed = 0.0;
            state.last = 0.0;
            state.particles.clear();
            for _ in 0..PARTICLES {
                let (r, g, b) = palette(self.random());
                state.particles.push(Particle {
                    x: self.random() * width,
                    y: -20.0 - self.random() * 260.0,
                    vx: (self.random() - 0.5) * 140.0,
                    vy: 40.0 + self.random() * 220.0,
                    size: 5.0 + self.random() * 8.0,
                    rot: self.random() * std::f64::consts::TAU,
                    vrot: (self.random() - 0.5) * 8.0,
                    r,
                    g,
                    b,
                });
            }
            if state.running {
                drop(state);
                self.area.set_visible(true);
                self.area.queue_draw();
                return;
            }
            state.running = true;
        }
        self.area.set_visible(true);

        let state = self.state.clone();
        self.area.add_tick_callback(move |area, clock| {
            let now = clock.frame_time() as f64 / 1_000_000.0;
            let mut state = state.borrow_mut();
            if state.last == 0.0 {
                state.last = now;
            }
            let dt = (now - state.last).clamp(0.0, 0.05);
            state.last = now;
            state.elapsed += dt;
            for particle in &mut state.particles {
                particle.vy += GRAVITY * dt;
                particle.x += particle.vx * dt;
                particle.y += particle.vy * dt;
                particle.rot += particle.vrot * dt;
            }
            let alive = state
                .particles
                .iter()
                .filter(|particle| particle.y < area.height() as f64 + 40.0)
                .count();
            if state.elapsed >= DURATION || alive == 0 {
                state.running = false;
                state.particles.clear();
                drop(state);
                area.set_visible(false);
                area.queue_draw();
                return gtk::glib::ControlFlow::Break;
            }
            drop(state);
            area.queue_draw();
            gtk::glib::ControlFlow::Continue
        });
    }

    /// A tiny xorshift: enough variety for confetti, no dependency.
    fn random(&self) -> f64 {
        let mut seed = self.seed.borrow_mut();
        let mut x = *seed;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *seed = x;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn palette(t: f64) -> (f64, f64, f64) {
    const COLORS: [(f64, f64, f64); 6] = [
        (0.72, 0.85, 0.47), // radar accent green
        (0.91, 0.90, 0.45), // yellow
        (0.55, 0.76, 0.62), // teal
        (0.93, 0.72, 0.42), // amber
        (0.85, 0.55, 0.55), // coral
        (0.60, 0.72, 0.85), // blue
    ];
    COLORS[(t * COLORS.len() as f64) as usize % COLORS.len()]
}

fn seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15)
        | 1
}
