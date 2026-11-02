// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Parsing of the audio function out of a device's descriptors.
//!
//! References: USB Device Class Definition for Audio Devices 1.0 (UAC1) sections 4.3 to
//! 4.6, and Audio Devices Rev. 2.0 (UAC2) sections 4.7 to 4.10.

use alloc::vec::Vec;

const DESCRIPTOR_CONFIGURATION: u8 = 0x02;
const DESCRIPTOR_INTERFACE: u8 = 0x04;
const DESCRIPTOR_ENDPOINT: u8 = 0x05;
const DESCRIPTOR_CS_INTERFACE: u8 = 0x24;
const DESCRIPTOR_CS_ENDPOINT: u8 = 0x25;

const CLASS_AUDIO: u8 = 0x01;
const SUBCLASS_AUDIOCONTROL: u8 = 0x01;
const SUBCLASS_AUDIOSTREAMING: u8 = 0x02;
const PROTOCOL_UAC2: u8 = 0x20;

const AC_INPUT_TERMINAL: u8 = 0x02;
const AC_OUTPUT_TERMINAL: u8 = 0x03;
const AC_MIXER_UNIT: u8 = 0x04;
const AC_SELECTOR_UNIT: u8 = 0x05;
const AC_FEATURE_UNIT: u8 = 0x06;
const UAC1_PROCESSING_UNIT: u8 = 0x07;
const UAC1_EXTENSION_UNIT: u8 = 0x08;
const UAC2_EFFECT_UNIT: u8 = 0x07;
const UAC2_PROCESSING_UNIT: u8 = 0x08;
const UAC2_EXTENSION_UNIT: u8 = 0x09;
const UAC2_CLOCK_SOURCE: u8 = 0x0a;
const UAC2_CLOCK_SELECTOR: u8 = 0x0b;
const UAC2_CLOCK_MULTIPLIER: u8 = 0x0c;
const UAC2_SAMPLE_RATE_CONVERTER: u8 = 0x0d;

const AS_GENERAL: u8 = 0x01;
const AS_FORMAT_TYPE: u8 = 0x02;
const EP_GENERAL: u8 = 0x01;

const FORMAT_TYPE_I: u8 = 0x01;
const UAC1_FORMAT_TAG_PCM: u16 = 0x0001;
const UAC2_FORMATS_PCM: u32 = 1 << 0;

/// Terminal type of the host side of a stream (Terminal Types 1.0/2.0, section 2.1).
const TERMINAL_USB_STREAMING: u16 = 0x0101;

/// Longest chain of units followed between a terminal and its clock or its output.
const MAX_PATH_DEPTH: usize = 16;

/// Audio Device Class release a function implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UacVersion {
    /// USB Audio 1.0: rates in the format descriptor, set on the endpoint.
    Uac1,
    /// USB Audio 2.0: rates owned by a clock source entity.
    Uac2,
}

/// Rates an alternate setting accepts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SampleRates {
    /// UAC1 list of discrete rates in Hz.
    Discrete(Vec<u32>),
    /// UAC1 continuous range in Hz, both ends included.
    Continuous {
        /// Lowest rate.
        min: u32,
        /// Highest rate.
        max: u32,
    },
    /// UAC2: the clock source decides. Ask it with
    /// [`crate::requests::uac2_get_sample_rate_range`].
    Clock,
}

/// Endpoint synchronization type, bmAttributes bits 3..2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncType {
    /// No synchronization.
    None,
    /// The device runs its own clock and reports its rate on a feedback endpoint.
    Asynchronous,
    /// The device locks onto the rate the data arrives at.
    Adaptive,
    /// The device locks onto start-of-frame.
    Synchronous,
}

/// Feature unit controls the host can use on the output path of a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeatureUnit {
    /// bUnitID.
    pub unit_id: u8,
    /// Mute on the master channel (channel 0).
    pub master_mute: bool,
    /// Volume on the master channel (channel 0).
    pub master_volume: bool,
    /// Logical channels (1 based) with their own volume control.
    pub channel_volume: Vec<u8>,
}

/// One playback alternate setting of an AudioStreaming interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingAlt {
    /// bInterfaceNumber.
    pub interface: u8,
    /// bAlternateSetting to select with SET_INTERFACE.
    pub alt_setting: u8,
    /// Input terminal the stream feeds.
    pub terminal_link: u8,
    /// Channels in each audio frame.
    pub channels: u8,
    /// Bytes per sample in the stream (bSubframeSize, bSubslotSize in UAC2).
    pub subframe_size: u8,
    /// Significant bits per sample.
    pub bit_resolution: u8,
    /// Rates the setting accepts.
    pub rates: SampleRates,
    /// Isochronous OUT endpoint address (bit 7 clear).
    pub endpoint_address: u8,
    /// wMaxPacketSize as found in the descriptor, multiplier bits included.
    pub max_packet_size: u16,
    /// bInterval of the data endpoint.
    pub interval: u8,
    /// Synchronization type of the data endpoint.
    pub sync: SyncType,
    /// Feedback endpoint address of an asynchronous endpoint, if it has one.
    pub feedback_endpoint: Option<u8>,
    /// UAC1: the endpoint takes a SET_CUR of its sampling frequency.
    pub sampling_frequency_control: bool,
    /// UAC2: clock source entity behind the terminal, the target of rate requests.
    pub clock_source: Option<u8>,
    /// Feature unit between the terminal and the output, for mute and volume.
    pub feature_unit: Option<FeatureUnit>,
}

impl StreamingAlt {
    /// Bytes in one audio frame (one sample for every channel).
    pub fn frame_bytes(&self) -> usize { self.channels as usize * self.subframe_size as usize }

    /// Largest payload one service interval carries, high-bandwidth transactions included.
    pub fn max_payload(&self) -> usize {
        let size = (self.max_packet_size & 0x7ff) as usize;
        let transactions = ((self.max_packet_size >> 11) & 0x3) as usize + 1;
        size * transactions
    }

    /// Whether the setting accepts `rate`. A UAC2 setting accepts whatever its clock does,
    /// which the descriptors do not say, so it always answers yes.
    pub fn supports_rate(&self, rate: u32) -> bool {
        match &self.rates {
            SampleRates::Discrete(rates) => rates.contains(&rate),
            SampleRates::Continuous { min, max } => (*min..=*max).contains(&rate),
            SampleRates::Clock => true,
        }
    }
}

/// The playback side of an audio function.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioFunction {
    /// Class release.
    pub version: UacVersion,
    /// bInterfaceNumber of the AudioControl interface, the target of unit requests.
    pub control_interface: u8,
    /// Every PCM playback alternate setting, in descriptor order.
    pub outputs: Vec<StreamingAlt>,
}

/// Why no audio function came out of a descriptor set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParseError {
    /// The bytes hold no configuration descriptor.
    NoConfiguration,
    /// The first configuration has no AudioControl interface.
    NotAudio,
    /// The function has no PCM playback alternate setting.
    NoOutput,
}

#[derive(Debug, Clone)]
enum EntityKind {
    InputTerminal { clock: Option<u8> },
    OutputTerminal { terminal_type: u16 },
    Feature(FeatureUnit),
    Unit,
    ClockSource,
    ClockSelector,
    ClockMultiplier,
}

#[derive(Debug, Clone)]
struct Entity {
    id: u8,
    sources: Vec<u8>,
    kind: EntityKind,
}

#[derive(Default)]
struct PartialAlt {
    interface: u8,
    alt_setting: u8,
    terminal_link: Option<u8>,
    pcm: bool,
    channels: u8,
    format: Option<(u8, u8)>,
    rates: Option<SampleRates>,
    endpoint: Option<(u8, u16, u8, SyncType)>,
    feedback_endpoint: Option<u8>,
    sampling_frequency_control: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CurrentInterface {
    Control,
    Streaming,
    Other,
}

/// Iterate over the descriptors in `bytes`, stopping at the first malformed one.
fn descriptors(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = bytes;
    core::iter::from_fn(move || {
        let length = *rest.first()? as usize;
        if length < 2 || length > rest.len() {
            return None;
        }
        let (descriptor, tail) = rest.split_at(length);
        rest = tail;
        Some(descriptor)
    })
}

fn u16_at(d: &[u8], offset: usize) -> u16 {
    d.get(offset..offset + 2).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
}

fn u24_at(d: &[u8], offset: usize) -> u32 {
    d.get(offset..offset + 3).map(|b| u32::from_le_bytes([b[0], b[1], b[2], 0])).unwrap_or(0)
}

fn u32_at(d: &[u8], offset: usize) -> u32 {
    d.get(offset..offset + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]])).unwrap_or(0)
}

fn byte(d: &[u8], offset: usize) -> u8 { d.get(offset).copied().unwrap_or(0) }

/// Source pins of a unit whose bNrInPins is at `count_offset`, followed by the IDs.
fn pins(d: &[u8], count_offset: usize) -> Vec<u8> {
    let count = byte(d, count_offset) as usize;
    d.get(count_offset + 1..).map(|p| p.iter().take(count).copied().collect()).unwrap_or_default()
}

fn parse_entity(version: UacVersion, d: &[u8]) -> Option<Entity> {
    let subtype = byte(d, 2);
    let id = byte(d, 3);
    let (sources, kind) = match (version, subtype) {
        (UacVersion::Uac1, AC_INPUT_TERMINAL) => {
            (Vec::new(), EntityKind::InputTerminal { clock: None })
        }
        (UacVersion::Uac2, AC_INPUT_TERMINAL) => (
            Vec::new(),
            EntityKind::InputTerminal { clock: Some(byte(d, 7)) },
        ),
        (_, AC_OUTPUT_TERMINAL) => {
            (alloc::vec![byte(d, 7)], EntityKind::OutputTerminal { terminal_type: u16_at(d, 4) })
        }
        (_, AC_MIXER_UNIT) | (_, AC_SELECTOR_UNIT) => (pins(d, 4), EntityKind::Unit),
        (UacVersion::Uac1, AC_FEATURE_UNIT) => {
            let control_size = byte(d, 5) as usize;
            let controls = |channel: usize| -> u32 {
                let start = 6 + channel * control_size;
                d.get(start..start + control_size)
                    .map(|b| b.iter().rev().fold(0, |acc, &x| (acc << 8) | x as u32))
                    .unwrap_or(0)
            };
            let channel_count = if control_size == 0 { 0 } else { (d.len().saturating_sub(7)) / control_size };
            let unit = FeatureUnit {
                unit_id: id,
                master_mute: channel_count > 0 && controls(0) & 0x1 != 0,
                master_volume: channel_count > 0 && controls(0) & 0x2 != 0,
                channel_volume: (1..channel_count).filter(|&c| controls(c) & 0x2 != 0).map(|c| c as u8).collect(),
            };
            (alloc::vec![byte(d, 4)], EntityKind::Feature(unit))
        }
        (UacVersion::Uac2, AC_FEATURE_UNIT) => {
            // Two bits per control: 0b11 is host programmable, 0b01 read only.
            let controls = |channel: usize| u32_at(d, 5 + channel * 4);
            let channel_count = d.len().saturating_sub(6) / 4;
            let writable = |bits: u32, shift: u32| (bits >> shift) & 0x3 == 0x3;
            let unit = FeatureUnit {
                unit_id: id,
                master_mute: channel_count > 0 && writable(controls(0), 0),
                master_volume: channel_count > 0 && writable(controls(0), 2),
                channel_volume: (1..channel_count)
                    .filter(|&c| writable(controls(c), 2))
                    .map(|c| c as u8)
                    .collect(),
            };
            (alloc::vec![byte(d, 4)], EntityKind::Feature(unit))
        }
        (UacVersion::Uac1, UAC1_PROCESSING_UNIT) | (UacVersion::Uac2, UAC2_PROCESSING_UNIT) => {
            (pins(d, 6), EntityKind::Unit)
        }
        (UacVersion::Uac1, UAC1_EXTENSION_UNIT) | (UacVersion::Uac2, UAC2_EXTENSION_UNIT) => {
            (pins(d, 6), EntityKind::Unit)
        }
        (UacVersion::Uac2, UAC2_EFFECT_UNIT) => (alloc::vec![byte(d, 6)], EntityKind::Unit),
        (UacVersion::Uac2, UAC2_SAMPLE_RATE_CONVERTER) => (alloc::vec![byte(d, 4)], EntityKind::Unit),
        (UacVersion::Uac2, UAC2_CLOCK_SOURCE) => (Vec::new(), EntityKind::ClockSource),
        (UacVersion::Uac2, UAC2_CLOCK_SELECTOR) => (pins(d, 4), EntityKind::ClockSelector),
        (UacVersion::Uac2, UAC2_CLOCK_MULTIPLIER) => (alloc::vec![byte(d, 4)], EntityKind::ClockMultiplier),
        _ => return None,
    };
    Some(Entity { id, sources, kind })
}

fn sync_type(attributes: u8) -> SyncType {
    match (attributes >> 2) & 0x3 {
        1 => SyncType::Asynchronous,
        2 => SyncType::Adaptive,
        3 => SyncType::Synchronous,
        _ => SyncType::None,
    }
}

/// Parse the playback side of the audio function in the first configuration of `bytes`.
///
/// `bytes` is a device descriptor followed by one or more configuration descriptors with
/// everything they contain, as fetched with GET_DESCRIPTOR.
pub fn parse_audio_function(bytes: &[u8]) -> Result<AudioFunction, ParseError> {
    let mut seen_configuration = false;
    let mut version = None;
    let mut control_interface = None;
    let mut entities: Vec<Entity> = Vec::new();
    let mut alts: Vec<PartialAlt> = Vec::new();
    let mut current = CurrentInterface::Other;

    for d in descriptors(bytes) {
        match d[1] {
            DESCRIPTOR_CONFIGURATION => {
                if seen_configuration {
                    break;
                }
                seen_configuration = true;
            }
            _ if !seen_configuration => {}
            DESCRIPTOR_INTERFACE if d.len() >= 9 => {
                let (number, alt, class, subclass, protocol) = (d[2], d[3], d[5], d[6], d[7]);
                current = match (class, subclass) {
                    (CLASS_AUDIO, SUBCLASS_AUDIOCONTROL) if control_interface.is_none() => {
                        control_interface = Some(number);
                        version =
                            Some(if protocol == PROTOCOL_UAC2 { UacVersion::Uac2 } else { UacVersion::Uac1 });
                        CurrentInterface::Control
                    }
                    (CLASS_AUDIO, SUBCLASS_AUDIOSTREAMING) => {
                        alts.push(PartialAlt { interface: number, alt_setting: alt, ..Default::default() });
                        CurrentInterface::Streaming
                    }
                    _ => CurrentInterface::Other,
                };
            }
            DESCRIPTOR_CS_INTERFACE if d.len() >= 3 => match (current, version) {
                (CurrentInterface::Control, Some(version)) => {
                    if let Some(entity) = parse_entity(version, d) {
                        entities.push(entity);
                    }
                }
                (CurrentInterface::Streaming, Some(version)) => {
                    let alt = alts.last_mut().unwrap();
                    match (version, d[2]) {
                        (UacVersion::Uac1, AS_GENERAL) => {
                            alt.terminal_link = Some(byte(d, 3));
                            alt.pcm = u16_at(d, 5) == UAC1_FORMAT_TAG_PCM;
                        }
                        (UacVersion::Uac2, AS_GENERAL) => {
                            alt.terminal_link = Some(byte(d, 3));
                            alt.pcm = byte(d, 5) == FORMAT_TYPE_I && u32_at(d, 6) & UAC2_FORMATS_PCM != 0;
                            alt.channels = byte(d, 10);
                        }
                        (UacVersion::Uac1, AS_FORMAT_TYPE) if byte(d, 3) == FORMAT_TYPE_I => {
                            alt.channels = byte(d, 4);
                            alt.format = Some((byte(d, 5), byte(d, 6)));
                            let count = byte(d, 7) as usize;
                            alt.rates = Some(if count == 0 {
                                SampleRates::Continuous { min: u24_at(d, 8), max: u24_at(d, 11) }
                            } else {
                                SampleRates::Discrete(
                                    (0..count).map(|i| u24_at(d, 8 + 3 * i)).filter(|&r| r != 0).collect(),
                                )
                            });
                        }
                        (UacVersion::Uac2, AS_FORMAT_TYPE) if byte(d, 3) == FORMAT_TYPE_I => {
                            alt.format = Some((byte(d, 4), byte(d, 5)));
                            alt.rates = Some(SampleRates::Clock);
                        }
                        _ => {}
                    }
                }
                _ => {}
            },
            DESCRIPTOR_ENDPOINT if d.len() >= 7 && current == CurrentInterface::Streaming => {
                let alt = alts.last_mut().unwrap();
                let (address, attributes) = (d[2], d[3]);
                let isochronous = attributes & 0x3 == 0x1;
                if !isochronous {
                    continue;
                }
                if address & 0x80 == 0 {
                    if alt.endpoint.is_none() {
                        alt.endpoint = Some((address, u16_at(d, 4), d[6], sync_type(attributes)));
                    }
                } else {
                    alt.feedback_endpoint = Some(address);
                }
            }
            DESCRIPTOR_CS_ENDPOINT if d.len() >= 4 && current == CurrentInterface::Streaming => {
                if d[2] == EP_GENERAL && version == Some(UacVersion::Uac1) {
                    alts.last_mut().unwrap().sampling_frequency_control = d[3] & 0x1 != 0;
                }
            }
            _ => {}
        }
    }

    if !seen_configuration {
        return Err(ParseError::NoConfiguration);
    }
    let (Some(version), Some(control_interface)) = (version, control_interface) else {
        return Err(ParseError::NotAudio);
    };

    let outputs: Vec<StreamingAlt> = alts
        .into_iter()
        .filter_map(|alt| {
            let (endpoint_address, max_packet_size, interval, sync) = alt.endpoint?;
            let (subframe_size, bit_resolution) = alt.format?;
            let terminal_link = alt.terminal_link?;
            if !alt.pcm || alt.channels == 0 || subframe_size == 0 {
                return None;
            }
            // The stream has to feed a USB streaming input terminal; a link to an output
            // terminal is a capture stream with an OUT endpoint, which does not exist in
            // practice but is not ours to play into either.
            let terminal = entities.iter().find(|e| e.id == terminal_link);
            if let Some(Entity { kind: EntityKind::OutputTerminal { .. }, .. }) = terminal {
                return None;
            }
            Some(StreamingAlt {
                interface: alt.interface,
                alt_setting: alt.alt_setting,
                terminal_link,
                channels: alt.channels,
                subframe_size,
                bit_resolution,
                rates: alt.rates?,
                endpoint_address,
                max_packet_size,
                interval,
                sync,
                feedback_endpoint: if sync == SyncType::Asynchronous { alt.feedback_endpoint } else { None },
                sampling_frequency_control: alt.sampling_frequency_control,
                clock_source: clock_source_of(&entities, terminal_link),
                feature_unit: feature_unit_after(&entities, terminal_link),
            })
        })
        .collect();

    if outputs.is_empty() {
        return Err(ParseError::NoOutput);
    }
    Ok(AudioFunction { version, control_interface, outputs })
}

fn entity(entities: &[Entity], id: u8) -> Option<&Entity> { entities.iter().find(|e| e.id == id) }

/// Follow a terminal's clock to the clock source it ends at. A selector is followed down
/// its first pin, which is the one it selects after reset.
fn clock_source_of(entities: &[Entity], terminal: u8) -> Option<u8> {
    let EntityKind::InputTerminal { clock: Some(mut id), .. } = entity(entities, terminal)?.kind else {
        return None;
    };
    for _ in 0..MAX_PATH_DEPTH {
        let e = entity(entities, id)?;
        match e.kind {
            EntityKind::ClockSource => return Some(id),
            EntityKind::ClockSelector | EntityKind::ClockMultiplier => id = *e.sources.first()?,
            _ => return None,
        }
    }
    None
}

/// First feature unit on a path from `start` to an output terminal that is not a USB
/// streaming terminal.
fn feature_unit_after(entities: &[Entity], start: u8) -> Option<FeatureUnit> {
    fn walk(entities: &[Entity], id: u8, depth: usize, found: Option<&FeatureUnit>) -> Option<FeatureUnit> {
        if depth > MAX_PATH_DEPTH {
            return None;
        }
        for next in entities.iter().filter(|e| e.sources.contains(&id)) {
            match &next.kind {
                EntityKind::OutputTerminal { terminal_type } if *terminal_type != TERMINAL_USB_STREAMING => {
                    if let Some(unit) = found {
                        return Some(unit.clone());
                    }
                }
                EntityKind::OutputTerminal { .. } => {}
                EntityKind::Feature(unit) => {
                    if let Some(unit) = walk(entities, next.id, depth + 1, found.or(Some(unit))) {
                        return Some(unit);
                    }
                }
                _ => {
                    if let Some(unit) = walk(entities, next.id, depth + 1, found) {
                        return Some(unit);
                    }
                }
            }
        }
        None
    }
    walk(entities, start, 0, None)
}
