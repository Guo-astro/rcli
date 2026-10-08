//! A micro square spinner for work with no measurable progress (model load,
//! on-device scoring). One animated line on stderr, cleared on stop.
//!
//! The spinner only runs where a person watches: stderr must be a terminal.
//! Everywhere else (pipes, CI, `--json`) it is a silent no-op, so scripted
//! output never changes.

use std::io::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Square quadrants, cycling. npm-style: small, monochrome, one cell wide.
const FRAMES: &[&str] = &["◰", "◳", "◲", "◱"];

/// How long each frame stays up.
const FRAME_TIME: Duration = Duration::from_millis(80);

struct State {
    label: String,
    frame: usize,
}

pub struct Spinner {
    state: Arc<Mutex<State>>,
    done: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    active: bool,
}

impl Spinner {
    /// Start spinning with `label`. Returns an inert spinner when stderr is
    /// not a terminal; `stop` is still safe to call.
    pub fn start(label: &str) -> Self {
        let active = crate::util::term::stderr_is_tty();
        if !active {
            return Spinner {
                state: Arc::new(Mutex::new(State {
                    label: String::new(),
                    frame: 0,
                })),
                done: Arc::new(AtomicBool::new(true)),
                handle: None,
                active: false,
            };
        }
        let state = Arc::new(Mutex::new(State {
            label: label.to_string(),
            frame: 0,
        }));
        let done = Arc::new(AtomicBool::new(false));
        let worker_state = state.clone();
        let worker_done = done.clone();
        let handle = std::thread::spawn(move || {
            while !worker_done.load(Ordering::Relaxed) {
                {
                    let mut guard = worker_state.lock().unwrap_or_else(|e| e.into_inner());
                    let frame = FRAMES[guard.frame % FRAMES.len()];
                    guard.frame += 1;
                    let mut err = std::io::stderr().lock();
                    let _ = write!(err, "\r{} {frame}", guard.label);
                    let _ = err.flush();
                }
                std::thread::sleep(FRAME_TIME);
            }
        });
        Spinner {
            state,
            done,
            handle: Some(handle),
            active: true,
        }
    }

    /// Swap the label mid-spin (load → score). No-op when inert.
    pub fn set_label(&self, label: &str) {
        if let Ok(mut guard) = self.state.lock() {
            guard.label = label.to_string();
        }
    }

    /// Stop the thread and wipe the line. Safe to call on an inert spinner.
    pub fn stop(mut self) {
        if !self.active {
            return;
        }
        self.done.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        let mut err = std::io::stderr().lock();
        let _ = write!(err, "\r\x1b[K");
        let _ = err.flush();
    }
}
