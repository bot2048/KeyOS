// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Audio output. `os/audio` plays PCM on a USB audio device (a USB-C headphone adapter,
//! USB headphones, or a USB Bluetooth audio transmitter) connected to the USB-C port.
//!
//! Apps hand over 16-bit stereo PCM at 48 kHz; the service converts it to whatever the
//! device takes and mixes nothing: the last app to play is what is heard.

pub mod messages;

use server::{AsScalar, CheckedConn, CheckedPermissions, FromScalar, MessageAllowed};
use xous::{MemoryFlags, PAGE_SIZE};

use crate::messages::*;

/// Rate of the PCM apps send.
pub const SAMPLE_RATE: u32 = 48_000;

#[macro_export]
macro_rules! use_api {
    () => {
        mod audio_permissions {
            use audio::messages::*;
            #[derive(Clone, Default, server::Permissions)]
            #[server_name = "os/audio"]
            pub struct AudioPermissions;
        }
        type AudioApi = audio::AudioApi<audio_permissions::AudioPermissions>;
    };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AudioError {
    #[error("OS error: {:?}", xous::Error::from_usize(*.0))]
    XousError(usize),
    #[error("No audio output device is connected")]
    NotConnected,
    #[error("The buffer does not hold the frames it claims")]
    InvalidBuffer,
    #[error("Other error")]
    Other,
}

impl From<xous::Error> for AudioError {
    fn from(value: xous::Error) -> Self { Self::XousError(value.to_usize()) }
}

impl AsScalar<2> for AudioError {
    fn as_scalar(&self) -> [u32; 2] {
        match self {
            AudioError::XousError(e) => [1, *e as u32],
            AudioError::NotConnected => [2, 0],
            AudioError::InvalidBuffer => [3, 0],
            AudioError::Other => [4, 0],
        }
    }
}

impl FromScalar<2> for AudioError {
    fn from_scalar(value: [u32; 2]) -> Self {
        match value[0] {
            1 => AudioError::XousError(value[1] as usize),
            2 => AudioError::NotConnected,
            3 => AudioError::InvalidBuffer,
            _ => AudioError::Other,
        }
    }
}

impl From<usize> for AudioError {
    fn from(value: usize) -> Self { FromScalar::from_scalar([value as u32 >> 24, value as u32 & 0xFF_FFFF]) }
}

impl From<AudioError> for usize {
    fn from(value: AudioError) -> Self {
        let [h, l] = AsScalar::as_scalar(&value);
        ((h << 24) | l) as usize
    }
}

/// What `os/audio` is playing on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioStatus {
    /// A device is connected and streaming.
    pub connected: bool,
    /// Rate the device plays at; the service resamples to it.
    pub device_rate: u32,
    /// The device is on the full-speed bus.
    pub full_speed: bool,
}

impl AsScalar<3> for AudioStatus {
    fn as_scalar(&self) -> [u32; 3] { [self.connected as u32, self.device_rate, self.full_speed as u32] }
}

impl FromScalar<3> for AudioStatus {
    fn from_scalar(value: [u32; 3]) -> Self {
        Self { connected: value[0] != 0, device_rate: value[1], full_speed: value[2] != 0 }
    }
}

/// Client of `os/audio`.
pub struct AudioApi<P: CheckedPermissions> {
    conn: CheckedConn<P>,
    buffer: xous::MemoryRange,
}

impl<P: CheckedPermissions> Default for AudioApi<P> {
    fn default() -> Self {
        let buffer = xous::map_memory(None, None, Self::BUFFER_LEN, MemoryFlags::W).expect("audio buffer");
        Self { conn: Default::default(), buffer }
    }
}

impl<P: CheckedPermissions> Drop for AudioApi<P> {
    fn drop(&mut self) { xous::unmap_memory(self.buffer).ok(); }
}

impl<P: CheckedPermissions> AudioApi<P> {
    const BUFFER_LEN: usize = 4 * PAGE_SIZE;
    /// How long to wait for room in the queue before offering the rest again.
    const RETRY: core::time::Duration = core::time::Duration::from_millis(10);

    /// Queue frames for playback, waiting for room as the device plays.
    pub fn play(&mut self, frames: &[[i16; 2]]) -> Result<(), AudioError>
    where
        P: MessageAllowed<Play>,
    {
        let per_lend = Self::BUFFER_LEN / 4;
        let mut rest = frames;
        while !rest.is_empty() {
            let chunk = &rest[..rest.len().min(per_lend)];
            let samples = self.buffer.as_slice_mut::<[i16; 2]>();
            samples[..chunk.len()].copy_from_slice(chunk);
            let taken = self.conn.lend_mut(Play { buf: self.buffer, frames: chunk.len() })?;
            rest = &rest[taken.min(chunk.len())..];
            if taken < chunk.len() {
                std::thread::sleep(Self::RETRY);
            }
        }
        Ok(())
    }

    /// Set the output volume in percent.
    pub fn set_volume(&self, percent: u8)
    where
        P: MessageAllowed<SetVolume>,
    {
        self.conn.send_scalar_nowait(SetVolume(percent.min(100))).ok();
    }

    /// Drop whatever is queued.
    pub fn stop(&self)
    where
        P: MessageAllowed<Stop>,
    {
        self.conn.send_scalar_nowait(Stop).ok();
    }

    /// The output device and its rate.
    pub fn status(&self) -> AudioStatus
    where
        P: MessageAllowed<GetStatus>,
    {
        self.conn.try_send_blocking_scalar(GetStatus).unwrap_or_default()
    }
}
