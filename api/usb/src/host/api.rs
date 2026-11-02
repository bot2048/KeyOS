// SPDX-FileCopyrightText: 2023 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

pub use ehci::{descriptors, EndpointDirection};
use server::{CheckedConn, CheckedPermissions, MessageAllowed};
use xous::{MemoryRange, PAGE_SIZE};

pub use super::messages::UsbEvent;
use super::messages::{
    BulkIn, BulkOut, Claim, ControlTransfer, IsConnected, IsoHeader, IsoOut, OpenEndpoint,
    ResetHostController, Subscribe, ISO_DATA_OFFSET, MAX_ISO_PACKETS,
};
use crate::error::UsbError;

const CLEAR_FEATURE: u8 = 1;
const ENDPOINT_HALT: u16 = 0;
const RECIPIENT_ENDPOINT: u8 = 0x02;
const ENDPOINT_IN: u8 = 0x80;

#[macro_export]
macro_rules! use_host_api {
    () => {
        mod usb_host_permissions {
            use $crate::host::messages::*;
            #[derive(Clone, Default, server::Permissions)]
            #[server_name = "os/usb"]
            pub struct UsbHostPermissions;
        }
        type UsbHost = $crate::host::api::UsbHost<usb_host_permissions::UsbHostPermissions>;
        type ConnectedUsbDevice =
            $crate::host::api::ConnectedUsbDevice<usb_host_permissions::UsbHostPermissions>;
        type UsbInEndpoint = $crate::host::api::UsbInEndpoint<usb_host_permissions::UsbHostPermissions>;
        type UsbOutEndpoint = $crate::host::api::UsbOutEndpoint<usb_host_permissions::UsbHostPermissions>;
        #[allow(dead_code)]
        type UsbIsoOutEndpoint =
            $crate::host::api::UsbIsoOutEndpoint<usb_host_permissions::UsbHostPermissions>;
    };
}

#[derive(Default)]
pub struct UsbHost<P: CheckedPermissions>(CheckedConn<P>);

pub struct ConnectedUsbDevice<P: CheckedPermissions> {
    conn: CheckedConn<P>,
    handle: usize,
}

pub struct UsbInEndpoint<P: CheckedPermissions> {
    conn: CheckedConn<P>,
    handle: usize,
    endpoint: u8,
}

pub struct UsbOutEndpoint<P: CheckedPermissions> {
    conn: CheckedConn<P>,
    handle: usize,
    endpoint: u8,
}

/// An isochronous OUT endpoint. Isochronous endpoints need no opening on the host side;
/// the device starts listening once the interface's alternate setting is selected.
pub struct UsbIsoOutEndpoint<P: CheckedPermissions> {
    conn: CheckedConn<P>,
    handle: usize,
    endpoint: u8,
    max_packet_size: u16,
    interval: u8,
}

/// Page aligned memory holding one isochronous transfer: header, packet lengths, then the
/// payloads back to back. Fill it with [`IsoBuffer::push_packet`], send it with
/// [`UsbIsoOutEndpoint::send`], [`IsoBuffer::clear`] it and start over.
pub struct IsoBuffer {
    memory: MemoryRange,
    packets: usize,
    data_len: usize,
}

impl IsoBuffer {
    /// A buffer with room for `data_capacity` payload bytes.
    pub fn new(data_capacity: usize) -> Result<Self, UsbError> {
        let len = (ISO_DATA_OFFSET + data_capacity).next_multiple_of(PAGE_SIZE);
        let memory = xous::map_memory(None, None, len, xous::MemoryFlags::W)?;
        Ok(Self { memory, packets: 0, data_len: 0 })
    }

    /// Drop every packet.
    pub fn clear(&mut self) {
        self.packets = 0;
        self.data_len = 0;
    }

    /// Packets in the buffer.
    pub fn packets(&self) -> usize { self.packets }

    /// Payload bytes the buffer still has room for.
    pub fn remaining(&self) -> usize { self.memory.len() - ISO_DATA_OFFSET - self.data_len }

    /// Append a packet of `len` bytes and return it to be filled, or `None` when the
    /// buffer is full.
    pub fn push_packet(&mut self, len: usize) -> Option<&mut [u8]> {
        if self.packets == MAX_ISO_PACKETS || len > self.remaining() || len > u16::MAX as usize {
            return None;
        }
        let bytes = self.memory.as_slice_mut::<u8>();
        let length_offset = core::mem::size_of::<IsoHeader>() + 2 * self.packets;
        bytes[length_offset..length_offset + 2].copy_from_slice(&(len as u16).to_ne_bytes());
        let start = ISO_DATA_OFFSET + self.data_len;
        self.packets += 1;
        self.data_len += len;
        Some(&mut bytes[start..start + len])
    }
}

impl Drop for IsoBuffer {
    fn drop(&mut self) { xous::unmap_memory(self.memory).ok(); }
}

impl<P: CheckedPermissions> UsbIsoOutEndpoint<P> {
    /// Send the packets in `buffer` and wait for them to go out, returning how many did. A
    /// transfer starts where the previous one on the endpoint ended, so two threads taking
    /// turns keep a stream going without a gap.
    pub fn send(&mut self, buffer: &mut IsoBuffer) -> Result<usize, UsbError>
    where
        P: MessageAllowed<IsoOut>,
    {
        let header = IsoHeader {
            packet_count: buffer.packets as u16,
            max_packet_size: self.max_packet_size,
            interval: self.interval,
            reserved: [0; 3],
        };
        buffer.memory.as_slice_mut::<IsoHeader>()[0] = header;
        self.conn.lend_mut(IsoOut {
            buffer: buffer.memory,
            handle: self.handle,
            endpoint: self.endpoint,
            length: buffer.data_len,
        })
    }
}

impl<P: CheckedPermissions> UsbHost<P> {
    pub fn subscribe<S>(&self, context: &mut server::ServerContext<S>)
    where
        S: server::Server + server::ArchiveEventHandler<UsbEvent>,
        P: MessageAllowed<Subscribe>,
    {
        self.0.subscribe_archive_infallible(Subscribe, context)
    }

    pub fn claim(&self, handle: usize) -> Result<ConnectedUsbDevice<P>, UsbError>
    where
        P: MessageAllowed<Claim>,
    {
        self.0.try_send_blocking_scalar(Claim(handle))??;
        Ok(ConnectedUsbDevice { conn: self.0.clone(), handle })
    }

    pub fn reset_host_controller(&self) -> Result<(), UsbError>
    where
        P: MessageAllowed<ResetHostController>,
    {
        self.0.send_scalar_nowait(ResetHostController)?;
        Ok(())
    }

    pub fn is_connected(&self) -> Result<bool, UsbError>
    where
        P: MessageAllowed<IsConnected>,
    {
        Ok(self.0.try_send_blocking_scalar(IsConnected)?)
    }
}

impl<P: CheckedPermissions> ConnectedUsbDevice<P> {
    /// Handle of the device, as in [`UsbEvent`].
    pub fn handle(&self) -> usize { self.handle }

    /// Run a control transfer on endpoint zero, returning what a device-to-host one read.
    pub fn control_transfer(
        &mut self,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: Vec<u8>,
        read_len: usize,
    ) -> Result<Vec<u8>, UsbError>
    where
        P: MessageAllowed<ControlTransfer>,
    {
        self.conn.send_blocking_archive(ControlTransfer {
            handle: self.handle,
            request_type,
            request,
            value,
            index,
            data,
            read_len,
        })
    }

    /// An isochronous OUT endpoint with the endpoint descriptor's wMaxPacketSize and
    /// bInterval.
    pub fn isochronous_out_endpoint(
        &self,
        endpoint: u8,
        max_packet_size: u16,
        interval: u8,
    ) -> UsbIsoOutEndpoint<P> {
        UsbIsoOutEndpoint {
            conn: self.conn.clone(),
            handle: self.handle,
            endpoint,
            max_packet_size,
            interval,
        }
    }

    pub fn open_in_endpoint(
        &mut self,
        endpoint: u8,
        max_packet_length: u16,
    ) -> Result<UsbInEndpoint<P>, UsbError>
    where
        P: MessageAllowed<OpenEndpoint>,
    {
        self.conn.try_send_blocking_scalar(OpenEndpoint {
            handle: self.handle,
            endpoint,
            max_packet_length,
            direction: EndpointDirection::In,
        })??;
        Ok(UsbInEndpoint { conn: self.conn.clone(), handle: self.handle, endpoint })
    }

    pub fn open_out_endpoint(
        &mut self,
        endpoint: u8,
        max_packet_length: u16,
    ) -> Result<UsbOutEndpoint<P>, UsbError>
    where
        P: MessageAllowed<OpenEndpoint>,
    {
        self.conn.try_send_blocking_scalar(OpenEndpoint {
            handle: self.handle,
            endpoint,
            max_packet_length,
            direction: EndpointDirection::Out,
        })??;
        Ok(UsbOutEndpoint { conn: self.conn.clone(), handle: self.handle, endpoint })
    }
}

impl<P: CheckedPermissions> UsbInEndpoint<P> {
    pub fn bulk_in(&mut self, buffer: MemoryRange, length: usize) -> Result<usize, UsbError>
    where
        P: MessageAllowed<BulkIn>,
    {
        self.conn.lend_mut(BulkIn { handle: self.handle, endpoint: self.endpoint, buffer, length })
    }

    /// Clears a STALL condition on this pipe, leaving both sides on DATA0.
    pub fn clear_halt(&mut self) -> Result<(), UsbError>
    where
        P: MessageAllowed<ControlTransfer>,
    {
        self.conn.send_blocking_archive(ControlTransfer {
            handle: self.handle,
            request_type: RECIPIENT_ENDPOINT,
            request: CLEAR_FEATURE,
            value: ENDPOINT_HALT,
            index: u16::from(self.endpoint | ENDPOINT_IN),
            data: Vec::new(),
            read_len: 0,
        })?;
        Ok(())
    }
}

impl<P: CheckedPermissions> UsbOutEndpoint<P> {
    pub fn bulk_out(&mut self, buffer: MemoryRange, length: usize) -> Result<usize, UsbError>
    where
        P: MessageAllowed<BulkOut>,
    {
        self.conn.lend_mut(BulkOut { handle: self.handle, endpoint: self.endpoint, buffer, length })
    }
}
