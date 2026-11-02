// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! The simulator has no USB host, so there is never an output device.

use audio::{messages::*, AudioError, AudioStatus};
use server::{BlockingScalarHandler, LendMutHandler, ScalarHandler};

#[derive(server::Server)]
#[name = "os/audio"]
pub struct AudioServer;

impl server::Server for AudioServer {}

impl LendMutHandler<Play> for AudioServer {
    fn handle(
        &mut self,
        _msg: Play,
        _sender: xous::PID,
        _context: &mut server::ServerContext<Self>,
    ) -> Result<usize, AudioError> {
        Err(AudioError::NotConnected)
    }
}

impl ScalarHandler<SetVolume> for AudioServer {
    fn handle(&mut self, _msg: SetVolume, _sender: xous::PID, _context: &mut server::ServerContext<Self>) {}
}

impl ScalarHandler<Stop> for AudioServer {
    fn handle(&mut self, _msg: Stop, _sender: xous::PID, _context: &mut server::ServerContext<Self>) {}
}

impl BlockingScalarHandler<GetStatus> for AudioServer {
    fn handle(
        &mut self,
        _msg: GetStatus,
        _sender: xous::PID,
        _context: &mut server::ServerContext<Self>,
    ) -> AudioStatus {
        AudioStatus::default()
    }
}
