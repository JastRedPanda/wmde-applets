// SPDX-License-Identifier: GPL-3.0-only

mod app;
mod localize;

use localize::localize;

pub fn run() -> cosmic::iced::Result {
    localize();
    app::run()
}
