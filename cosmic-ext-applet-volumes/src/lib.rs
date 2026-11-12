// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod localize;
pub mod mtp;
pub mod udisks;
pub mod watch;

/// Entry point, called by this crate's own binary or by a multicall binary such as
/// `cosmic-applets` that dispatches on `argv[0]`.
///
/// # Errors
/// Returns the error of the iced event loop (for example when no Wayland session is available).
pub fn run() -> cosmic::iced::Result {
    localize::localize();

    cosmic::applet::run::<app::VolumesApplet>(())
}
