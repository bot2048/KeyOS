// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

#![no_std]
#![warn(missing_docs)]
//! Host side helpers for USB Audio Class 1.0 and 2.0 playback devices.
//!
//! The crate does no I/O. It reads the descriptor bytes a host stack fetched during
//! enumeration (the device descriptor followed by a configuration descriptor, as in
//! `ehci::descriptors::DescriptorSet::bytes`), picks an output streaming alternate setting
//! and sample rate, says how many frames go into each isochronous packet, builds the class
//! control requests that set the device up, and converts PCM into the device's format.

extern crate alloc;

pub mod descriptors;
pub mod pcm;
pub mod requests;
pub mod schedule;
pub mod select;

pub use descriptors::{parse_audio_function, AudioFunction, ParseError, SampleRates, StreamingAlt, UacVersion};
pub use schedule::PacketSchedule;
pub use select::{choose_output, OutputChoice, Speed};
