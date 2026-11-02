// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Builders for the control requests that set a playback stream up.
//!
//! UAC1 5.2 and UAC2 5.2 define the class requests; SET_INTERFACE is USB 2.0 9.4.10.

use alloc::vec::Vec;

use crate::descriptors::UacVersion;

const DIR_IN: u8 = 0x80;
const TYPE_STANDARD: u8 = 0x00;
const TYPE_CLASS: u8 = 0x20;
const RECIPIENT_INTERFACE: u8 = 0x01;
const RECIPIENT_ENDPOINT: u8 = 0x02;

const SET_INTERFACE: u8 = 0x0b;

/// UAC1 SET_CUR and UAC2 CUR share the code.
const CUR: u8 = 0x01;
/// UAC2 RANGE.
const RANGE: u8 = 0x02;
const UAC1_GET_CUR: u8 = 0x81;
const UAC1_GET_MIN: u8 = 0x82;
const UAC1_GET_MAX: u8 = 0x83;
const UAC1_GET_RES: u8 = 0x84;

/// UAC1 endpoint control selector.
const UAC1_SAMPLING_FREQ_CONTROL: u16 = 0x01;
/// UAC2 clock source control selector.
const UAC2_CS_SAM_FREQ_CONTROL: u16 = 0x01;
/// Feature unit control selectors, the same in both releases.
const FU_MUTE_CONTROL: u16 = 0x01;
const FU_VOLUME_CONTROL: u16 = 0x02;

/// A control transfer on endpoint zero, laid out like the USB host API's
/// `ControlTransfer` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlRequest {
    /// bmRequestType; bit 7 is the direction.
    pub request_type: u8,
    /// bRequest.
    pub request: u8,
    /// wValue.
    pub value: u16,
    /// wIndex.
    pub index: u16,
    /// Data stage sent to the device; empty for a read.
    pub data: Vec<u8>,
    /// Bytes to read back; zero for a write.
    pub read_len: usize,
}

impl ControlRequest {
    fn write(request_type: u8, request: u8, value: u16, index: u16, data: &[u8]) -> Self {
        Self { request_type, request, value, index, data: data.to_vec(), read_len: 0 }
    }

    fn read(request_type: u8, request: u8, value: u16, index: u16, read_len: usize) -> Self {
        Self { request_type: request_type | DIR_IN, request, value, index, data: Vec::new(), read_len }
    }
}

fn entity_index(control_interface: u8, entity: u8) -> u16 { (entity as u16) << 8 | control_interface as u16 }

/// SET_INTERFACE: select `alt_setting` on `interface`. Alternate setting zero has no
/// endpoints and stops a stream.
pub fn set_interface(interface: u8, alt_setting: u8) -> ControlRequest {
    ControlRequest::write(
        TYPE_STANDARD | RECIPIENT_INTERFACE,
        SET_INTERFACE,
        alt_setting as u16,
        interface as u16,
        &[],
    )
}

/// UAC1 SET_CUR of the sampling frequency on an isochronous endpoint (UAC1 5.2.3.2.3.1).
pub fn uac1_set_sample_rate(endpoint_address: u8, rate: u32) -> ControlRequest {
    ControlRequest::write(
        TYPE_CLASS | RECIPIENT_ENDPOINT,
        CUR,
        UAC1_SAMPLING_FREQ_CONTROL << 8,
        endpoint_address as u16,
        &rate.to_le_bytes()[..3],
    )
}

/// UAC1 GET_CUR of an endpoint's sampling frequency; the answer is 3 bytes.
pub fn uac1_get_sample_rate(endpoint_address: u8) -> ControlRequest {
    ControlRequest::read(
        TYPE_CLASS | RECIPIENT_ENDPOINT,
        UAC1_GET_CUR,
        UAC1_SAMPLING_FREQ_CONTROL << 8,
        endpoint_address as u16,
        3,
    )
}

/// UAC2 CUR of a clock source's sampling frequency (UAC2 5.2.5.1.1).
pub fn uac2_set_sample_rate(control_interface: u8, clock_id: u8, rate: u32) -> ControlRequest {
    ControlRequest::write(
        TYPE_CLASS | RECIPIENT_INTERFACE,
        CUR,
        UAC2_CS_SAM_FREQ_CONTROL << 8,
        entity_index(control_interface, clock_id),
        &rate.to_le_bytes(),
    )
}

/// UAC2 GET CUR of a clock source's sampling frequency; the answer is 4 bytes.
pub fn uac2_get_sample_rate(control_interface: u8, clock_id: u8) -> ControlRequest {
    ControlRequest::read(
        TYPE_CLASS | RECIPIENT_INTERFACE,
        CUR,
        UAC2_CS_SAM_FREQ_CONTROL << 8,
        entity_index(control_interface, clock_id),
        4,
    )
}

/// UAC2 GET RANGE of a clock source's sampling frequency. `read_len` bounds the answer:
/// two bytes of count and 12 per subrange. A short read is fine, the device sends the
/// first subranges and [`parse_uac2_rate_ranges`] takes what arrived.
pub fn uac2_get_sample_rate_range(control_interface: u8, clock_id: u8, read_len: usize) -> ControlRequest {
    ControlRequest::read(
        TYPE_CLASS | RECIPIENT_INTERFACE,
        RANGE,
        UAC2_CS_SAM_FREQ_CONTROL << 8,
        entity_index(control_interface, clock_id),
        read_len,
    )
}

/// One subrange of a UAC2 RANGE answer (layout 3, UAC2 5.2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateRange {
    /// dMIN in Hz.
    pub min: u32,
    /// dMAX in Hz.
    pub max: u32,
    /// dRES in Hz; zero for a single rate.
    pub res: u32,
}

impl RateRange {
    /// Whether `rate` is one of the rates the subrange describes.
    pub fn contains(&self, rate: u32) -> bool {
        (self.min..=self.max).contains(&rate) && (self.res == 0 || (rate - self.min) % self.res == 0)
    }
}

/// Parse the answer to [`uac2_get_sample_rate_range`].
pub fn parse_uac2_rate_ranges(answer: &[u8]) -> Vec<RateRange> {
    let Some(count) = answer.get(..2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize) else {
        return Vec::new();
    };
    let word = |c: &[u8], i: usize| u32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
    answer[2..]
        .chunks_exact(12)
        .take(count)
        .map(|c| RateRange { min: word(c, 0), max: word(c, 4), res: word(c, 8) })
        .filter(|r| r.min > 0 && r.min <= r.max)
        .collect()
}

/// Mute or unmute `channel` (0 is the master channel) of a feature unit.
pub fn set_mute(control_interface: u8, unit_id: u8, channel: u8, mute: bool) -> ControlRequest {
    ControlRequest::write(
        TYPE_CLASS | RECIPIENT_INTERFACE,
        CUR,
        FU_MUTE_CONTROL << 8 | channel as u16,
        entity_index(control_interface, unit_id),
        &[mute as u8],
    )
}

/// Set the volume of `channel` of a feature unit, in 1/256 dB. Both releases share the
/// encoding; a device clamps values outside its range or stalls the request.
pub fn set_volume(control_interface: u8, unit_id: u8, channel: u8, db_256: i16) -> ControlRequest {
    ControlRequest::write(
        TYPE_CLASS | RECIPIENT_INTERFACE,
        CUR,
        FU_VOLUME_CONTROL << 8 | channel as u16,
        entity_index(control_interface, unit_id),
        &db_256.to_le_bytes(),
    )
}

/// Which end of a UAC1 volume control to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeAttribute {
    /// GET_CUR.
    Current,
    /// GET_MIN.
    Min,
    /// GET_MAX.
    Max,
    /// GET_RES.
    Resolution,
}

/// Read one attribute of a feature unit volume control. UAC1 has a request for each; a
/// UAC2 device answers RANGE with all of them at once (two bytes of count, then min, max
/// and resolution as i16), parsed by [`parse_uac2_volume_range`]. `Current` is CUR in
/// both.
pub fn get_volume(
    version: UacVersion,
    control_interface: u8,
    unit_id: u8,
    channel: u8,
    attribute: VolumeAttribute,
) -> ControlRequest {
    let (request, read_len) = match (version, attribute) {
        (UacVersion::Uac1, VolumeAttribute::Current) => (UAC1_GET_CUR, 2),
        (UacVersion::Uac1, VolumeAttribute::Min) => (UAC1_GET_MIN, 2),
        (UacVersion::Uac1, VolumeAttribute::Max) => (UAC1_GET_MAX, 2),
        (UacVersion::Uac1, VolumeAttribute::Resolution) => (UAC1_GET_RES, 2),
        (UacVersion::Uac2, VolumeAttribute::Current) => (CUR, 2),
        (UacVersion::Uac2, _) => (RANGE, 8),
    };
    ControlRequest::read(
        TYPE_CLASS | RECIPIENT_INTERFACE,
        request,
        FU_VOLUME_CONTROL << 8 | channel as u16,
        entity_index(control_interface, unit_id),
        read_len,
    )
}

/// Parse the first subrange of a UAC2 volume RANGE answer into (min, max, resolution) in
/// 1/256 dB.
pub fn parse_uac2_volume_range(answer: &[u8]) -> Option<(i16, i16, i16)> {
    let half = |i: usize| answer.get(i..i + 2).map(|b| i16::from_le_bytes([b[0], b[1]]));
    if half(0)? < 1 {
        return None;
    }
    Some((half(2)?, half(4)?, half(6)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uac1_rate_goes_to_the_endpoint() {
        let r = uac1_set_sample_rate(0x01, 44_100);
        assert_eq!((r.request_type, r.request, r.value, r.index), (0x22, 0x01, 0x0100, 0x0001));
        assert_eq!(r.data, [0x44, 0xac, 0x00]);
        let r = uac1_get_sample_rate(0x01);
        assert_eq!((r.request_type, r.request, r.read_len), (0xa2, 0x81, 3));
    }

    #[test]
    fn uac2_rate_goes_to_the_clock() {
        let r = uac2_set_sample_rate(0, 9, 48_000);
        assert_eq!((r.request_type, r.request, r.value, r.index), (0x21, 0x01, 0x0100, 0x0900));
        assert_eq!(r.data, 48_000u32.to_le_bytes());
        let r = uac2_get_sample_rate_range(0, 9, 62);
        assert_eq!((r.request_type, r.request, r.index, r.read_len), (0xa1, 0x02, 0x0900, 62));
    }

    #[test]
    fn rate_ranges() {
        let mut answer = alloc::vec![3, 0];
        for (min, max, res) in [(44_100, 44_100, 0), (48_000, 96_000, 48_000), (8_000, 16_000, 8_000)] {
            answer.extend_from_slice(&u32::to_le_bytes(min));
            answer.extend_from_slice(&u32::to_le_bytes(max));
            answer.extend_from_slice(&u32::to_le_bytes(res));
        }
        let ranges = parse_uac2_rate_ranges(&answer);
        assert_eq!(ranges.len(), 3);
        assert!(ranges[0].contains(44_100));
        assert!(ranges[1].contains(96_000) && !ranges[1].contains(72_000));
        // A short read keeps the complete subranges.
        assert_eq!(parse_uac2_rate_ranges(&answer[..20]).len(), 1);
        assert!(parse_uac2_rate_ranges(&[]).is_empty());
    }

    #[test]
    fn feature_unit_requests() {
        let r = set_mute(0, 2, 0, false);
        assert_eq!((r.request_type, r.request, r.value, r.index), (0x21, 0x01, 0x0100, 0x0200));
        assert_eq!(r.data, [0]);
        let r = set_volume(0, 2, 1, -10 * 256);
        assert_eq!((r.value, r.index), (0x0201, 0x0200));
        assert_eq!(r.data, (-2560i16).to_le_bytes());
        let r = get_volume(UacVersion::Uac1, 0, 10, 0, VolumeAttribute::Min);
        assert_eq!((r.request_type, r.request, r.value, r.index, r.read_len), (0xa1, 0x82, 0x0200, 0x0a00, 2));
        let r = get_volume(UacVersion::Uac2, 0, 2, 0, VolumeAttribute::Max);
        assert_eq!((r.request, r.read_len), (0x02, 8));
        let answer = [1, 0, 0x00, 0x80, 0x00, 0x00, 0x00, 0x01];
        assert_eq!(parse_uac2_volume_range(&answer), Some((i16::MIN, 0, 256)));
    }

    #[test]
    fn set_interface_is_standard() {
        let r = set_interface(1, 2);
        assert_eq!((r.request_type, r.request, r.value, r.index), (0x01, 0x0b, 2, 1));
    }
}
