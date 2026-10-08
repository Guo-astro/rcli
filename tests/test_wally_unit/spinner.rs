//! Spinner lifecycle: inert without a terminal, safe to relabel and stop.

use wally::progress::spinner::Spinner;

#[test]
fn stop_without_terminal_never_panics() {
    let spinner = Spinner::start("loading");
    spinner.set_label("scoring");
    spinner.stop();
}
