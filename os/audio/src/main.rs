// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Plays PCM on a USB audio device connected to the USB-C port.

#[cfg(keyos)]
mod device;
#[cfg(not(keyos))]
mod hosted;

fn main() {
    log_server::init_wait(env!("CARGO_CRATE_NAME")).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    xous::set_thread_priority(xous::ThreadPriority::System4).unwrap();

    #[cfg(keyos)]
    server::listen(device::AudioServer::default());
    #[cfg(not(keyos))]
    server::listen(hosted::AudioServer);
}
