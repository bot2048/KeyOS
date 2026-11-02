//! Register and descriptor layouts, OpenHCI 1.0a chapters 4 and 7.

use core::array;

use bitfield::bitfield;
use vcell::VolatileCell;

#[repr(C)]
pub struct Registers {
    pub revision: VolatileCell<u32>,
    pub control: VolatileCell<Control>,
    pub command_status: VolatileCell<u32>,
    pub interrupt_status: VolatileCell<u32>,
    pub interrupt_enable: VolatileCell<u32>,
    pub interrupt_disable: VolatileCell<u32>,
    pub hcca: VolatileCell<u32>,
    pub period_current_ed: VolatileCell<u32>,
    pub control_head_ed: VolatileCell<u32>,
    pub control_current_ed: VolatileCell<u32>,
    pub bulk_head_ed: VolatileCell<u32>,
    pub bulk_current_ed: VolatileCell<u32>,
    pub done_head: VolatileCell<u32>,
    pub fm_interval: VolatileCell<u32>,
    pub fm_remaining: VolatileCell<u32>,
    pub fm_number: VolatileCell<u32>,
    pub periodic_start: VolatileCell<u32>,
    pub ls_threshold: VolatileCell<u32>,
    pub rh_descriptor_a: VolatileCell<u32>,
    pub rh_descriptor_b: VolatileCell<u32>,
    pub rh_status: VolatileCell<u32>,
    pub rh_port_status: [VolatileCell<u32>; 15],
}

bitfield! {
    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct Control(u32);
    impl Debug;

    pub control_bulk_ratio, set_control_bulk_ratio: 1, 0;
    pub periodic_list_enable, set_periodic_list_enable: 2;
    pub isochronous_enable, set_isochronous_enable: 3;
    pub control_list_enable, set_control_list_enable: 4;
    pub bulk_list_enable, set_bulk_list_enable: 5;
    pub functional_state, set_functional_state: 7, 6;
    pub interrupt_routing, set_interrupt_routing: 8;
}

pub const FUNCTIONAL_STATE_RESET: u32 = 0b00;
pub const FUNCTIONAL_STATE_OPERATIONAL: u32 = 0b10;

pub const COMMAND_HOST_CONTROLLER_RESET: u32 = 1 << 0;
pub const COMMAND_CONTROL_LIST_FILLED: u32 = 1 << 1;

pub const INTERRUPT_SCHEDULING_OVERRUN: u32 = 1 << 0;
pub const INTERRUPT_WRITEBACK_DONE_HEAD: u32 = 1 << 1;
pub const INTERRUPT_UNRECOVERABLE_ERROR: u32 = 1 << 4;
pub const INTERRUPT_ROOT_HUB_STATUS_CHANGE: u32 = 1 << 6;
pub const INTERRUPT_MASTER_ENABLE: u32 = 1 << 31;

/// HcFmInterval: 12000 bit times per frame, less one.
pub const FRAME_INTERVAL: u32 = 11999;
/// HcFmInterval FSLargestDataPacket, the value 5.4 of the spec derives from the interval.
pub const FS_LARGEST_DATA_PACKET: u32 = (FRAME_INTERVAL - 210) * 6 / 7;
pub const FRAME_INTERVAL_TOGGLE: u32 = 1 << 31;
/// HcPeriodicStart: periodic lists get the first 90% of every frame.
pub const PERIODIC_START: u32 = FRAME_INTERVAL * 9 / 10;

/// HcRhStatus write: SetGlobalPower.
pub const RH_STATUS_SET_GLOBAL_POWER: u32 = 1 << 16;

pub const PORT_CURRENT_CONNECT_STATUS: u32 = 1 << 0;
pub const PORT_ENABLE_STATUS: u32 = 1 << 1;
/// Read: reset in progress. Write: SetPortReset.
pub const PORT_RESET_STATUS: u32 = 1 << 4;
/// Write: SetPortPower.
pub const PORT_SET_POWER: u32 = 1 << 8;
pub const PORT_LOW_SPEED_DEVICE: u32 = 1 << 9;
/// The change bits, written as ones to clear them.
pub const PORT_CHANGE_BITS: u32 = 0x1f << 16;
pub const PORT_RESET_STATUS_CHANGE: u32 = 1 << 20;

/// Host Controller Communications Area (OpenHCI 4.4): 256 bytes of uncached memory,
/// 256 byte aligned, that the controller and the driver share.
#[repr(C, align(256))]
pub struct Hcca {
    pub(crate) interrupt_table: [VolatileCell<u32>; 32],
    pub(crate) frame_number: VolatileCell<u16>,
    pub(crate) pad: VolatileCell<u16>,
    pub(crate) done_head: VolatileCell<u32>,
    pub(crate) reserved: [VolatileCell<u32>; 30],
}

/// Endpoint descriptor, 4.2.
#[repr(C, align(16))]
pub struct Ed {
    pub info: VolatileCell<EdInfo>,
    pub tail: VolatileCell<u32>,
    pub head: VolatileCell<u32>,
    pub next: VolatileCell<u32>,
}

bitfield! {
    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct EdInfo(u32);
    impl Debug;

    pub u8, function_address, set_function_address: 6, 0;
    pub u8, endpoint_number, set_endpoint_number: 10, 7;
    pub direction, set_direction: 12, 11;
    pub low_speed, set_low_speed: 13;
    pub skip, set_skip: 14;
    pub isochronous_format, set_isochronous_format: 15;
    pub u16, max_packet_size, set_max_packet_size: 26, 16;
}

/// ED direction field: take it from each TD.
pub const ED_DIRECTION_FROM_TD: u32 = 0b00;
pub const ED_DIRECTION_OUT: u32 = 0b01;

pub const ED_HEAD_HALTED: u32 = 1 << 0;
pub const ED_POINTER_MASK: u32 = !0xf;

/// General transfer descriptor, 4.3.1.
#[repr(C, align(16))]
pub struct Td {
    pub info: VolatileCell<TdInfo>,
    pub current_buffer: VolatileCell<u32>,
    pub next: VolatileCell<u32>,
    pub buffer_end: VolatileCell<u32>,
}

bitfield! {
    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct TdInfo(u32);
    impl Debug;

    pub buffer_rounding, set_buffer_rounding: 18;
    pub pid, set_pid: 20, 19;
    pub delay_interrupt, set_delay_interrupt: 23, 21;
    pub data_toggle, set_data_toggle: 25, 24;
    pub error_count, _: 27, 26;
    pub condition_code, set_condition_code: 31, 28;
}

pub const PID_SETUP: u32 = 0b00;
pub const PID_OUT: u32 = 0b01;
pub const PID_IN: u32 = 0b10;
/// Data toggle taken from the TD (bit 1 set), DATA0 or DATA1 in bit 0.
pub const TOGGLE_DATA0: u32 = 0b10;
pub const TOGGLE_DATA1: u32 = 0b11;
/// DelayInterrupt value that asks for no interrupt at all.
pub const NO_INTERRUPT: u32 = 0b111;

pub const CC_NO_ERROR: u32 = 0x0;
pub const CC_STALL: u32 = 0x4;
pub const CC_NOT_ACCESSED: u32 = 0xf;

impl Default for Td {
    fn default() -> Self {
        Self {
            info: VolatileCell::new(TdInfo(0)),
            current_buffer: VolatileCell::new(0),
            next: VolatileCell::new(0),
            buffer_end: VolatileCell::new(0),
        }
    }
}

/// Isochronous transfer descriptor, 4.3.2.
#[repr(C, align(32))]
pub struct IsoTd {
    pub info: VolatileCell<IsoTdInfo>,
    pub buffer_page_0: VolatileCell<u32>,
    pub next: VolatileCell<u32>,
    pub buffer_end: VolatileCell<u32>,
    pub packet_status: [VolatileCell<u16>; 8],
}

bitfield! {
    #[repr(C)]
    #[derive(Copy, Clone)]
    pub struct IsoTdInfo(u32);
    impl Debug;

    pub u16, starting_frame, set_starting_frame: 15, 0;
    pub delay_interrupt, set_delay_interrupt: 23, 21;
    pub frame_count, set_frame_count: 26, 24;
    pub condition_code, set_condition_code: 31, 28;
}

/// Initial packet status word: the offset with the condition code at NOT ACCESSED.
pub const PSW_NOT_ACCESSED: u16 = 0xe000;
/// Offset bit selecting the page of BufferEnd instead of BufferPage0.
pub const PSW_SECOND_PAGE: u16 = 0x1000;

impl Default for IsoTd {
    fn default() -> Self {
        Self {
            info: VolatileCell::new(IsoTdInfo(0)),
            buffer_page_0: VolatileCell::new(0),
            next: VolatileCell::new(0),
            buffer_end: VolatileCell::new(0),
            packet_status: array::from_fn(|_| VolatileCell::new(0)),
        }
    }
}
