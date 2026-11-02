// SPDX-FileCopyrightText: 2023 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

use ehci::{descriptors, EndpointDirection};
use server::{AsScalar, FromScalar};
use xous::MemoryRange;

use crate::error::UsbError;

#[derive(Debug, server::Message, Clone, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[event(UsbEvent)]
pub struct Subscribe;

#[derive(Debug, server::Message, Clone)]
#[response(Result<(), UsbError>)]
pub struct Claim(pub usize);

#[derive(Debug, server::Message, Clone)]
#[response(Result<(), UsbError>)]
pub struct OpenEndpoint {
    pub handle: usize,
    pub endpoint: u8,
    pub max_packet_length: u16,
    pub direction: EndpointDirection,
}

impl AsScalar<4> for OpenEndpoint {
    fn as_scalar(&self) -> [u32; 4] {
        [
            self.handle as u32,
            self.endpoint as u32,
            self.max_packet_length as u32,
            if self.direction == EndpointDirection::In { 0 } else { 1 },
        ]
    }
}

impl FromScalar<4> for OpenEndpoint {
    fn from_scalar(value: [u32; 4]) -> Self {
        Self {
            handle: value[0] as usize,
            endpoint: value[1] as u8,
            max_packet_length: value[2] as u16,
            direction: if value[3] == 0 { EndpointDirection::In } else { EndpointDirection::Out },
        }
    }
}

#[derive(Debug, server::Message)]
#[response(Result<usize, UsbError>)]
pub struct BulkOut {
    pub buffer: MemoryRange,
    pub handle: usize,
    pub endpoint: u8,
    pub length: usize,
}

impl From<server::SimpleMemoryMessage> for BulkOut {
    fn from(msg: server::SimpleMemoryMessage) -> Self {
        Self { buffer: msg.buf, endpoint: msg.arg1 as u8, length: msg.arg2, handle: msg.arg1 >> 8 }
    }
}

impl From<BulkOut> for server::SimpleMemoryMessage {
    fn from(val: BulkOut) -> Self {
        Self { buf: val.buffer, arg1: val.endpoint as usize | (val.handle << 8), arg2: val.length }
    }
}

#[derive(Debug, server::Message)]
#[response(Result<usize, UsbError>)]
pub struct BulkIn {
    pub buffer: MemoryRange,
    pub handle: usize,
    pub endpoint: u8,
    pub length: usize,
}

impl From<server::SimpleMemoryMessage> for BulkIn {
    fn from(msg: server::SimpleMemoryMessage) -> Self {
        Self { buffer: msg.buf, endpoint: msg.arg1 as u8, length: msg.arg2, handle: msg.arg1 >> 8 }
    }
}

impl From<BulkIn> for server::SimpleMemoryMessage {
    fn from(val: BulkIn) -> Self {
        Self { buf: val.buffer, arg1: val.endpoint as usize | (val.handle << 8), arg2: val.length }
    }
}

/// Bytes in front of the payloads of an isochronous buffer: the header, then the packet
/// lengths. See [`crate::host::api::IsoBuffer`].
pub const ISO_DATA_OFFSET: usize = 0x400;
/// Most packets one isochronous transfer carries.
pub const MAX_ISO_PACKETS: usize = ehci::MAX_ISO_PACKETS;

/// Header at the start of an isochronous buffer, followed by `packet_count` u16 lengths.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct IsoHeader {
    pub packet_count: u16,
    /// wMaxPacketSize of the endpoint, multiplier bits included.
    pub max_packet_size: u16,
    /// bInterval of the endpoint.
    pub interval: u8,
    pub reserved: [u8; 3],
}

/// An isochronous OUT transfer: one packet per service interval of the endpoint, the
/// lengths and endpoint parameters in an [`IsoHeader`] at the start of `buffer`, the
/// payloads back to back from [`ISO_DATA_OFFSET`]. Answers with the packets sent.
#[derive(Debug, server::Message)]
#[response(Result<usize, UsbError>)]
pub struct IsoOut {
    pub buffer: MemoryRange,
    pub handle: usize,
    pub endpoint: u8,
    /// Payload bytes from `ISO_DATA_OFFSET`.
    pub length: usize,
}

impl From<server::SimpleMemoryMessage> for IsoOut {
    fn from(msg: server::SimpleMemoryMessage) -> Self {
        Self { buffer: msg.buf, endpoint: msg.arg1 as u8, length: msg.arg2, handle: msg.arg1 >> 8 }
    }
}

impl From<IsoOut> for server::SimpleMemoryMessage {
    fn from(val: IsoOut) -> Self {
        Self { buf: val.buffer, arg1: val.endpoint as usize | (val.handle << 8), arg2: val.length }
    }
}

/// A control transfer on endpoint zero. The direction is the top bit of
/// `request_type`, as on the wire.
#[derive(Debug, server::Message, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[response(Result<Vec<u8>, UsbError>)]
pub struct ControlTransfer {
    pub handle: usize,
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    /// Payload sent to the device; empty for a device-to-host transfer.
    pub data: Vec<u8>,
    /// Bytes to read back; zero for a host-to-device transfer.
    pub read_len: usize,
}

/// Take the controller down and back up, so whatever is on the bus enumerates afresh.
#[derive(Debug, server::Message, Clone)]
pub struct ResetHostController;

#[derive(Debug, Clone, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum UsbEvent {
    /// `full_speed` devices run on the OHCI controller, with 1 ms service intervals.
    Connect {
        handle: usize,
        descriptors: descriptors::DescriptorSet,
        full_speed: bool,
    },
    Disconnect {
        handle: usize,
    },
}

#[derive(Debug, server::Message)]
#[response(bool)]
pub struct IsConnected;
