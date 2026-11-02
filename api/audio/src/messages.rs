// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{AudioError, AudioStatus};

/// Queue PCM for playback: interleaved 16-bit stereo frames at [`crate::SAMPLE_RATE`],
/// `frames` of them at the start of `buf`. Answers with the frames taken, fewer than sent
/// when the queue is full.
#[derive(Debug, server::Message)]
#[response(Result<usize, AudioError>)]
pub struct Play {
    pub buf: xous::MemoryRange,
    pub frames: usize,
}

impl From<server::SimpleMemoryMessage> for Play {
    fn from(msg: server::SimpleMemoryMessage) -> Self { Self { buf: msg.buf, frames: msg.arg1 } }
}

impl From<Play> for server::SimpleMemoryMessage {
    fn from(msg: Play) -> Self { Self { buf: msg.buf, arg1: msg.frames, arg2: 0 } }
}

/// Set the output volume in percent, 0 to 100.
#[derive(Debug, server::Message)]
pub struct SetVolume(pub u8);

/// Drop everything queued and not yet played.
#[derive(Debug, server::Message)]
pub struct Stop;

/// Whether an output device is connected, and at which rate it plays.
#[derive(Debug, server::Message)]
#[response(AudioStatus)]
pub struct GetStatus;
