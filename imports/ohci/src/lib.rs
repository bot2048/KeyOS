#![no_std]
#![warn(missing_docs)]
//! A simplified driver for the Open Host Controller Interface, the USB 1.1 companion of an
//! EHCI controller. Full and low-speed devices plugged into the root ports land here once
//! the EHCI driver hands their port over.
//!
//! Only what USB audio needs is implemented: enumeration, control transfers and
//! isochronous OUT streams. Bulk and interrupt endpoints are not supported.
//!
//! Descriptor, request, error and transfer context types are shared with the `ehci` crate,
//! so a host stack can treat devices on either controller alike.

mod controller;
mod registers;

pub use controller::{max_enumeration_ms, Controller, EventHandler, MAX_CONTROL_DATA_LEN};
pub use ehci::{BufferPool, BufferPoolElement};
use ehci::{Pool, PoolElement};
pub use registers::Hcca;
use registers::{Ed, IsoTd, Td};

/// Element type of the endpoint descriptor pool.
pub type EdPoolElement = PoolElement<Ed>;
/// Endpoint descriptor pool: one per device for its control pipe, one per isochronous
/// stream, one for address zero, and the ones removed less than a frame ago.
pub type EdPool = Pool<Ed>;

/// Element type of the general transfer descriptor pool.
pub type TdPoolElement = PoolElement<Td>;
/// General transfer descriptor pool: three per control transfer and one per endpoint.
pub type TdPool = Pool<Td>;

/// Element type of the isochronous transfer descriptor pool.
pub type IsoTdPoolElement = PoolElement<IsoTd>;
/// Isochronous transfer descriptor pool: one per eight queued packets and one per stream.
pub type IsoTdPool = Pool<IsoTd>;
