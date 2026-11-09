//! modron-desktop binary entry: arg parsing, the tokio runtime thread,
//! and the GPUI app launch. The real logic lives in the library crate.
use modron_desktop::ui;

fn main() {
    ui::run(ui::LaunchOptions::from_args());
}
