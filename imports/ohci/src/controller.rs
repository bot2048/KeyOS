extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;

use ehci::descriptors::DescriptorSet;
use ehci::{BufferPool, ControlRequest, EhciError, PoolElementHandle, TransferContext, MAX_ISO_PACKETS};
use log::{debug, info, warn};
use vcell::VolatileCell;

use crate::registers::*;
use crate::{EdPool, IsoTdPool, TdPool};

/// Largest data stage a control transfer can carry, set by the pool element size.
pub const MAX_CONTROL_DATA_LEN: usize = 0x40;

/// USB 2.0 Table 7-13: TATTDB.
const ATTACH_DEBOUNCE_MS: usize = 100;
/// The root hub drives reset for 10ms (OpenHCI 7.4.4) and reports when it is done.
const RESET_TIMEOUT_MS: usize = 50;
/// USB 2.0 Table 7-14: TRSTRCY, the device's recovery after reset.
const RESET_RECOVERY_MS: usize = 10;
/// USB 2.0 9.2.6.3: SetAddress completes within 50ms.
const SET_ADDRESS_TIMEOUT_MS: usize = 100;
/// USB 2.0 9.2.6.3: 2ms of recovery once SetAddress completed.
const SET_ADDRESS_RECOVERY_MS: usize = 2;
/// USB 2.0 9.2.6.4: 500ms to the first data packet, then 50ms for the status stage.
const DATA_STAGE_TIMEOUT_MS: usize = 600;
/// Generous, as in the EHCI driver: devices do their internal setup here.
const SET_CONFIGURATION_TIMEOUT_MS: usize = 1000;
/// Descriptor reads during enumeration: device head, device, configuration head, all of it.
const DESCRIPTOR_READS: usize = 4;

/// Lead over the frame being run when an isochronous stream (re)starts.
const ISO_START_MARGIN_FRAMES: u32 = 3;
/// Furthest ahead isochronous work may be queued. StartingFrame is compared to the frame
/// number in 16 bits, so anything past half of that range would look late.
const ISO_HORIZON_FRAMES: u32 = 0x4000;
const PAGE: usize = 0x1000;

const DESCRIPTOR_TYPE_DEVICE: u16 = 0x01;
const DESCRIPTOR_TYPE_CONFIG: u16 = 0x02;
const DEVICE_DESCRIPTOR_LEN: usize = 18;
const CONFIG_HEADER_LEN: usize = 9;

/// Longest a device can take from turning up on a port to being configured, for a
/// caller driving [`Controller::work`] every `work_period_ms`. See
/// `ehci::max_enumeration_ms`.
pub const fn max_enumeration_ms(work_period_ms: usize) -> usize {
    /// Steps that end on a timer or a register the caller has to come back and read.
    const POLLED_STEPS: usize = 6;

    ATTACH_DEBOUNCE_MS
        + RESET_TIMEOUT_MS
        + RESET_RECOVERY_MS
        + SET_ADDRESS_TIMEOUT_MS
        + SET_ADDRESS_RECOVERY_MS
        + DESCRIPTOR_READS * DATA_STAGE_TIMEOUT_MS
        + SET_CONFIGURATION_TIMEOUT_MS
        + POLLED_STEPS * work_period_ms
}

/// User-defined callbacks for the events that happen during USB processing. The same
/// events as `ehci::EventHandler`.
pub trait EventHandler<CT> {
    /// A device has been connected, set up, and ready to use.
    fn device_connected(&mut self, controller: &mut Controller<CT>, address: u8, descriptors: DescriptorSet);

    /// A device was disconnected or deconfigured (including reset)
    fn device_disconnected(&mut self, controller: &mut Controller<CT>, address: u8);

    /// A transfer has either succeeded or not; sent exactly once for every transfer
    /// scheduled successfully.
    fn transfer_result(&mut self, controller: &mut Controller<CT>, result: Result<usize, EhciError>, context: CT);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    SetAddress,
    DeviceHead,
    Device,
    ConfigHead,
    Config,
    SetConfiguration,
}

enum Purpose<CT> {
    Builtin { port: usize, step: Step },
    User(CT),
}

enum DataBuffer {
    None,
    Pool(PoolElementHandle<[u8; 0x40]>),
    /// The descriptor buffer, which the controller owns for its whole life.
    Descriptor(*mut u8),
}

struct ControlTransfer<CT> {
    tds: Vec<PoolElementHandle<Td>>,
    setup: PoolElementHandle<[u8; 0x40]>,
    data: DataBuffer,
    data_len: usize,
    data_phys: usize,
    device_to_host: bool,
    purpose: Purpose<CT>,
}

struct IsoTransfer<CT> {
    tds: Vec<PoolElementHandle<IsoTd>>,
    packets: usize,
    context: CT,
}

enum Queue<CT> {
    Control { transfers: VecDeque<ControlTransfer<CT>>, dummy: PoolElementHandle<Td> },
    Iso { transfers: VecDeque<IsoTransfer<CT>>, dummy: PoolElementHandle<IsoTd>, next_frame: Option<u32> },
}

struct Endpoint<CT> {
    ed: PoolElementHandle<Ed>,
    address: u8,
    number: u8,
    queue: Queue<CT>,
}

/// Descriptors unlinked from a list, kept until the controller has moved past them.
struct Retired {
    _ed: PoolElementHandle<Ed>,
    _tds: Vec<PoolElementHandle<Td>>,
    _iso_tds: Vec<PoolElementHandle<IsoTd>>,
    _buffers: Vec<PoolElementHandle<[u8; 0x40]>>,
    since: usize,
}

/// Uncached memory a whole configuration descriptor is read into.
struct DescriptorBuffer {
    buffer: &'static mut [u8],
    busy: bool,
}

enum Finished<CT> {
    Builtin { port: usize, step: Step, result: Result<Vec<u8>, EhciError> },
    User { context: CT, result: Result<usize, EhciError> },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
enum PortState {
    #[default]
    Disconnected,
    Debouncing {
        since: usize,
    },
    Resetting {
        since: usize,
    },
    ResetRecovery {
        since: usize,
    },
    Addressing {
        since: usize,
    },
    AddressRecovery {
        since: usize,
    },
    /// Descriptor reads in flight; `bytes` holds the device descriptor once read.
    Describing {
        since: usize,
        bytes: Vec<u8>,
    },
    Configuring {
        since: usize,
        descriptor: DescriptorSet,
    },
    Configured {
        descriptor: DescriptorSet,
    },
}

/// Main OHCI controller struct.
pub struct Controller<CT> {
    regs: *mut Registers,
    hcca: &'static Hcca,
    ed_pool: EdPool,
    td_pool: TdPool,
    iso_td_pool: IsoTdPool,
    buffer_pool: BufferPool,
    descriptor_buffer: Option<DescriptorBuffer>,
    /// Control endpoints in list order, address zero's first, then isochronous ones.
    endpoints: Vec<Endpoint<CT>>,
    retired: Vec<Retired>,
    ports: Vec<PortState>,
    frame: u32,
    last_frame_number: u32,
    virt_to_phys: fn(*const u8) -> usize,
    enabled: bool,
}

unsafe impl<CT> Send for Controller<CT> {}

impl<CT: TransferContext> Controller<CT> {
    /// Initialize internal data. The controller has to be clocked by this point.
    ///
    /// `hcca` and every pool must be uncached memory, each pool within a single page.
    pub fn new(
        base: usize,
        hcca: &'static Hcca,
        mut ed_pool: EdPool,
        mut td_pool: TdPool,
        iso_td_pool: IsoTdPool,
        buffer_pool: BufferPool,
        virt_to_phys: fn(*const u8) -> usize,
    ) -> Result<Self, EhciError> {
        let regs = base as *mut Registers;
        let ports = unsafe { (*regs).rh_descriptor_a.get() } as usize & 0xff;
        let ports = ports.min(15);
        let default_pipe = Self::new_control_endpoint(&mut ed_pool, &mut td_pool, 0, false, 8)?;
        Ok(Self {
            regs,
            hcca,
            ed_pool,
            td_pool,
            iso_td_pool,
            buffer_pool,
            descriptor_buffer: None,
            endpoints: vec![default_pipe],
            retired: Vec::new(),
            ports: vec![PortState::Disconnected; ports],
            frame: 0,
            last_frame_number: 0,
            virt_to_phys,
            enabled: false,
        })
    }

    /// Give the controller uncached memory, within one page, to read whole configuration
    /// descriptors into. Without it only their first 64 bytes are read.
    pub fn set_descriptor_buffer(&mut self, buffer: &'static mut [u8]) {
        self.descriptor_buffer = Some(DescriptorBuffer { buffer, busy: false });
    }

    fn regs(&self) -> &Registers { unsafe { &*self.regs } }

    fn new_control_endpoint(
        ed_pool: &mut EdPool,
        td_pool: &mut TdPool,
        address: u8,
        low_speed: bool,
        max_packet_size: u16,
    ) -> Result<Endpoint<CT>, EhciError> {
        let dummy = td_pool.alloc(Td::default())?;
        let mut info = EdInfo(0);
        info.set_function_address(address);
        info.set_direction(ED_DIRECTION_FROM_TD);
        info.set_low_speed(low_speed);
        info.set_max_packet_size(max_packet_size);
        let dummy_phys = dummy.physical_address() as u32;
        let ed = ed_pool.alloc(Ed {
            info: VolatileCell::new(info),
            tail: VolatileCell::new(dummy_phys),
            head: VolatileCell::new(dummy_phys),
            next: VolatileCell::new(0),
        })?;
        Ok(Endpoint { ed, address, number: 0, queue: Queue::Control { transfers: VecDeque::new(), dummy } })
    }

    /// Rewrite the list links from `endpoints`: the control list in order, and the
    /// isochronous EDs chained behind every slot of the interrupt table.
    fn relink(&mut self) {
        let control: Vec<u32> = self
            .endpoints
            .iter()
            .filter(|e| matches!(e.queue, Queue::Control { .. }))
            .map(|e| e.ed.physical_address() as u32)
            .collect();
        let iso: Vec<u32> = self
            .endpoints
            .iter()
            .filter(|e| matches!(e.queue, Queue::Iso { .. }))
            .map(|e| e.ed.physical_address() as u32)
            .collect();
        for endpoint in &self.endpoints {
            let phys = endpoint.ed.physical_address() as u32;
            let list = if matches!(endpoint.queue, Queue::Control { .. }) { &control } else { &iso };
            let position = list.iter().position(|&p| p == phys).unwrap();
            endpoint.ed.next.set(list.get(position + 1).copied().unwrap_or(0));
        }
        self.regs().control_head_ed.set(control.first().copied().unwrap_or(0));
        let periodic_head = iso.first().copied().unwrap_or(0);
        for slot in &self.hcca.interrupt_table {
            slot.set(periodic_head);
        }
    }

    /// Enable the controller and power the root ports.
    pub fn enable(&mut self) -> Result<(), EhciError> {
        if self.enabled {
            return Ok(());
        }
        let regs = self.regs();
        regs.command_status.set(COMMAND_HOST_CONTROLLER_RESET);
        // OpenHCI 5.1.1.4: the reset takes at most 10us.
        let mut spins = 0;
        while regs.command_status.get() & COMMAND_HOST_CONTROLLER_RESET != 0 {
            spins += 1;
            if spins > 1_000_000 {
                log::error!("OHCI did not come out of reset");
                return Err(EhciError::ControllerDisabled);
            }
        }
        // Everything below has to be done within 2ms of the reset (5.1.1.4).
        regs.fm_interval.set(FRAME_INTERVAL_TOGGLE | FS_LARGEST_DATA_PACKET << 16 | FRAME_INTERVAL);
        regs.periodic_start.set(PERIODIC_START);
        regs.hcca.set((self.virt_to_phys)(self.hcca as *const Hcca as *const u8) as u32);
        regs.bulk_head_ed.set(0);
        regs.interrupt_status.set(!0);
        self.relink();
        let regs = self.regs();
        let mut control = Control(0);
        control.set_control_bulk_ratio(3);
        control.set_periodic_list_enable(true);
        control.set_isochronous_enable(true);
        control.set_control_list_enable(true);
        control.set_functional_state(FUNCTIONAL_STATE_OPERATIONAL);
        regs.control.set(control);

        regs.rh_status.set(RH_STATUS_SET_GLOBAL_POWER);
        for port in 0..self.ports.len() {
            regs.rh_port_status[port].set(PORT_SET_POWER);
        }
        self.last_frame_number = regs.fm_number.get() & 0xffff;
        self.enabled = true;
        Ok(())
    }

    /// Disable the controller. Disconnects devices and fails every unfinished transfer.
    pub fn disable(&mut self, handler: &mut impl EventHandler<CT>) -> Result<(), EhciError> {
        if !self.enabled {
            return Ok(());
        }
        let mut control = self.regs().control.get();
        control.set_functional_state(FUNCTIONAL_STATE_RESET);
        self.regs().control.set(control);
        self.enabled = false;
        if let Some(buffer) = self.descriptor_buffer.as_mut() {
            buffer.busy = false;
        }

        // The controller is stopped, so nothing has to wait for it to move on.
        self.retired.clear();
        let mut cancelled = Vec::new();
        for endpoint in core::mem::take(&mut self.endpoints) {
            Self::drain(endpoint, &mut cancelled);
        }
        let default_pipe = Self::new_control_endpoint(&mut self.ed_pool, &mut self.td_pool, 0, false, 8)?;
        self.endpoints.push(default_pipe);
        for context in cancelled {
            handler.transfer_result(self, Err(EhciError::Cancelled), context);
        }

        let mut disconnects = Vec::new();
        for (port_number, port) in self.ports.iter_mut().enumerate() {
            if matches!(port, PortState::Configured { .. }) {
                disconnects.push(port_number);
            }
            *port = PortState::Disconnected;
        }
        for port_number in disconnects {
            handler.device_disconnected(self, Self::port_to_address(port_number));
        }
        Ok(())
    }

    /// Take an endpoint apart, collecting the contexts of its user transfers.
    fn drain(endpoint: Endpoint<CT>, contexts: &mut Vec<CT>) -> Retired {
        let mut tds = Vec::new();
        let mut iso_tds = Vec::new();
        let mut buffers = Vec::new();
        match endpoint.queue {
            Queue::Control { transfers, dummy } => {
                tds.push(dummy);
                for transfer in transfers {
                    tds.extend(transfer.tds);
                    buffers.push(transfer.setup);
                    if let DataBuffer::Pool(buffer) = transfer.data {
                        buffers.push(buffer);
                    }
                    if let Purpose::User(context) = transfer.purpose {
                        contexts.push(context);
                    }
                }
            }
            Queue::Iso { transfers, dummy, .. } => {
                iso_tds.push(dummy);
                for transfer in transfers {
                    iso_tds.extend(transfer.tds);
                    contexts.push(transfer.context);
                }
            }
        }
        Retired { _ed: endpoint.ed, _tds: tds, _iso_tds: iso_tds, _buffers: buffers, since: 0 }
    }

    /// Unlink every endpoint of `address` and fail its transfers.
    fn remove_address(&mut self, address: u8, tick_count: usize, cancelled: &mut Vec<CT>) {
        if address == 0 {
            return;
        }
        let mut i = 0;
        let mut removed = false;
        while i < self.endpoints.len() {
            if self.endpoints[i].address == address {
                let endpoint = self.endpoints.remove(i);
                let mut info = endpoint.ed.info.get();
                info.set_skip(true);
                endpoint.ed.info.set(info);
                let mut retired = Self::drain(endpoint, cancelled);
                retired.since = tick_count;
                self.retired.push(retired);
                removed = true;
            } else {
                i += 1;
            }
        }
        if removed {
            // A retired ED keeps its next pointer, so a controller standing on it still
            // finds the rest of the list.
            self.relink();
        }
    }

    /// Should be called on every USB interrupt, and periodically at least every 100ms.
    /// `tick_count` is a monotonic clock in ms.
    pub fn work(&mut self, tick_count: usize, handler: &mut impl EventHandler<CT>) -> Result<(), EhciError> {
        if !self.enabled {
            return Err(EhciError::ControllerDisabled);
        }
        let frame_number = self.regs().fm_number.get() & 0xffff;
        self.frame = self.frame.wrapping_add(frame_number.wrapping_sub(self.last_frame_number) & 0xffff);
        self.last_frame_number = frame_number;
        // A frame has certainly passed once a millisecond has.
        self.retired.retain(|r| tick_count.wrapping_sub(r.since) < 2);

        let mut finished = Vec::new();
        for endpoint in &mut self.endpoints {
            Self::harvest(endpoint, &mut finished);
        }
        for f in finished {
            match f {
                Finished::User { context, result } => handler.transfer_result(self, result, context),
                Finished::Builtin { port, step, result } => {
                    if let Err(e) = self.advance_enumeration(port, step, result, tick_count, handler) {
                        warn!("Enumeration step {step:?} failed on port {port}: {e:?}");
                        self.ports[port] = PortState::Disconnected;
                    }
                }
            }
        }
        self.update_ports(tick_count, handler)
    }

    fn harvest(endpoint: &mut Endpoint<CT>, finished: &mut Vec<Finished<CT>>) {
        let head = endpoint.ed.head.get();
        let mut halted = head & ED_HEAD_HALTED != 0;
        let head = head & ED_POINTER_MASK;
        match &mut endpoint.queue {
            Queue::Control { transfers, dummy } => {
                while let Some(front) = transfers.front() {
                    let in_progress = front.tds.iter().any(|td| td.physical_address() as u32 == head);
                    if in_progress && !halted {
                        break;
                    }
                    let transfer = transfers.pop_front().unwrap();
                    let failed = transfer
                        .tds
                        .iter()
                        .any(|td| !matches!(td.info.get().condition_code(), CC_NO_ERROR | CC_NOT_ACCESSED));
                    if halted && failed {
                        // OpenHCI 6.4.4: the failed TD was retired and HeadP moved past it;
                        // the rest of its transfer goes too, then the queue runs on.
                        let next = transfers.front().map_or(dummy.physical_address(), |t| t.tds[0].physical_address());
                        endpoint.ed.head.set(next as u32);
                        halted = false;
                    }
                    finished.push(Self::control_result(transfer));
                }
            }
            Queue::Iso { transfers, .. } => {
                while let Some(front) = transfers.front() {
                    if front.tds.iter().any(|td| td.physical_address() as u32 == head) {
                        break;
                    }
                    let transfer = transfers.pop_front().unwrap();
                    let mut failed = 0;
                    for td in &transfer.tds {
                        let count = td.info.get().frame_count() as usize + 1;
                        failed += td.packet_status[..count].iter().filter(|s| s.get() >> 12 != 0).count();
                    }
                    let sent = transfer.packets.saturating_sub(failed);
                    if failed > 0 {
                        debug!("Iso OUT A={}:EP={}: {failed} packets missed", endpoint.address, endpoint.number);
                    }
                    let result = if sent > 0 { Ok(sent) } else { Err(EhciError::TransferError) };
                    finished.push(Finished::User { context: transfer.context, result });
                }
            }
        }
    }

    fn control_result(transfer: ControlTransfer<CT>) -> Finished<CT> {
        let mut error = None;
        for td in &transfer.tds {
            match td.info.get().condition_code() {
                CC_NO_ERROR | CC_NOT_ACCESSED => {}
                CC_STALL => error = error.or(Some(EhciError::Stalled)),
                _ => error = error.or(Some(EhciError::TransferError)),
            }
        }
        // A failed transfer leaves TDs the controller never got to.
        if error.is_none() && transfer.tds.iter().any(|td| td.info.get().condition_code() == CC_NOT_ACCESSED) {
            error = Some(EhciError::TransferError);
        }
        let transferred = if transfer.data_len == 0 {
            0
        } else {
            // The data stage is the second TD; CurrentBufferPointer is zero once all of it
            // moved, else it points at the first byte that did not.
            let current = transfer.tds[1].current_buffer.get() as usize;
            if current == 0 {
                transfer.data_len
            } else if current & !(PAGE - 1) == transfer.data_phys & !(PAGE - 1) {
                current - transfer.data_phys
            } else {
                (PAGE - (transfer.data_phys & (PAGE - 1))) + (current & (PAGE - 1))
            }
        };
        let result = match error {
            Some(e) => Err(e),
            None => Ok(transferred),
        };
        let data: &[u8] = match &transfer.data {
            DataBuffer::None => &[],
            DataBuffer::Pool(buffer) => &buffer[..transfer.data_len],
            DataBuffer::Descriptor(ptr) => unsafe { core::slice::from_raw_parts(*ptr, transfer.data_len) },
        };
        match transfer.purpose {
            Purpose::Builtin { port, step } => {
                Finished::Builtin { port, step, result: result.map(|len| data[..len.min(data.len())].to_vec()) }
            }
            Purpose::User(mut context) => {
                if let (Ok(len), true) = (result, transfer.device_to_host) {
                    let destination = context.data_buffer();
                    let len = len.min(destination.len());
                    destination[..len].copy_from_slice(&data[..len]);
                }
                Finished::User { context, result }
            }
        }
    }

    fn control_endpoint_index(&self, address: u8) -> Option<usize> {
        self.endpoints.iter().position(|e| e.address == address && matches!(e.queue, Queue::Control { .. }))
    }

    /// Queue a control transfer on the default pipe of `address`.
    fn queue_control(
        &mut self,
        address: u8,
        request: &ControlRequest,
        write_data: &[u8],
        use_descriptor_buffer: bool,
        purpose: Purpose<CT>,
    ) -> Result<(), (Purpose<CT>, EhciError)> {
        let Some(index) = self.control_endpoint_index(address) else {
            return Err((purpose, EhciError::EndpointNotOpen));
        };
        let device_to_host = request.typ & 0x80 != 0;
        let length = request.length as usize;
        let mut setup_bytes = [0u8; 0x40];
        let (value, index_field) = (request.value, request.index);
        setup_bytes[..8].copy_from_slice(&[
            request.typ,
            request.request,
            value as u8,
            (value >> 8) as u8,
            index_field as u8,
            (index_field >> 8) as u8,
            length as u8,
            (length >> 8) as u8,
        ]);
        let setup = match self.buffer_pool.alloc(setup_bytes) {
            Ok(b) => b,
            Err(e) => return Err((purpose, e)),
        };
        let (data, data_len, data_ptr) = if length == 0 {
            (DataBuffer::None, 0, core::ptr::null())
        } else if use_descriptor_buffer {
            let Some(buffer) = self.descriptor_buffer.as_mut().filter(|b| !b.busy) else {
                return Err((purpose, EhciError::OutOfPoolItems));
            };
            buffer.busy = true;
            let len = length.min(buffer.buffer.len());
            let ptr = buffer.buffer.as_mut_ptr();
            (DataBuffer::Descriptor(ptr), len, ptr as *const u8)
        } else {
            let mut staging = [0u8; 0x40];
            let len = length.min(staging.len());
            if !device_to_host {
                let n = write_data.len().min(len);
                staging[..n].copy_from_slice(&write_data[..n]);
            }
            match self.buffer_pool.alloc(staging) {
                Ok(b) => {
                    let ptr = b.as_ptr();
                    (DataBuffer::Pool(b), len, ptr)
                }
                Err(e) => return Err((purpose, e)),
            }
        };
        let v2p = self.virt_to_phys;
        let data_phys = if data_len > 0 { v2p(data_ptr) } else { 0 };

        let td_info = |pid: u32, toggle: u32, interrupt: bool, rounding: bool| {
            let mut info = TdInfo(0);
            info.set_pid(pid);
            info.set_data_toggle(toggle);
            info.set_delay_interrupt(if interrupt { 0 } else { NO_INTERRUPT });
            info.set_buffer_rounding(rounding);
            info.set_condition_code(CC_NOT_ACCESSED);
            info
        };
        let setup_phys = v2p(setup.as_ptr());
        let mut contents = vec![(td_info(PID_SETUP, TOGGLE_DATA0, false, false), setup_phys, setup_phys + 7)];
        if data_len > 0 {
            let pid = if device_to_host { PID_IN } else { PID_OUT };
            let end = v2p(data_ptr.wrapping_add(data_len - 1));
            contents.push((td_info(pid, TOGGLE_DATA1, false, true), data_phys, end));
        }
        let status_pid = if device_to_host && data_len > 0 { PID_OUT } else { PID_IN };
        contents.push((td_info(status_pid, TOGGLE_DATA1, true, false), 0, 0));

        // The current dummy takes the first TD, a fresh one becomes the dummy.
        let mut fresh = Vec::new();
        for _ in 0..contents.len() {
            match self.td_pool.alloc(Td::default()) {
                Ok(td) => fresh.push(td),
                Err(e) => {
                    if let DataBuffer::Descriptor(_) = data {
                        self.descriptor_buffer.as_mut().unwrap().busy = false;
                    }
                    return Err((purpose, e));
                }
            }
        }
        let endpoint = &mut self.endpoints[index];
        let Queue::Control { transfers, dummy } = &mut endpoint.queue else { unreachable!() };
        let new_dummy = fresh.pop().unwrap();
        let mut tds = vec![core::mem::replace(dummy, new_dummy)];
        tds.extend(fresh);
        for (i, (info, buffer, end)) in contents.into_iter().enumerate() {
            let next = tds.get(i + 1).map_or(dummy.physical_address(), |t| t.physical_address());
            tds[i].current_buffer.set(buffer as u32);
            tds[i].buffer_end.set(end as u32);
            tds[i].next.set(next as u32);
            tds[i].info.set(info);
        }
        endpoint.ed.tail.set(dummy.physical_address() as u32);
        transfers.push_back(ControlTransfer { tds, setup, data, data_len, data_phys, device_to_host, purpose });
        self.regs().command_status.set(COMMAND_CONTROL_LIST_FILLED);
        Ok(())
    }

    /// Schedule a Control transfer. Result only indicates that the scheduling itself was
    /// successful, the transfer result will be returned via the EventHandler. The
    /// direction is the top bit of `request.typ`; a read fills the context's data buffer.
    pub fn schedule_control(
        &mut self,
        address: u8,
        request: ControlRequest,
        data: &[u8],
        context: CT,
    ) -> Result<(), (CT, EhciError)> {
        if !self.enabled {
            return Err((context, EhciError::ControllerDisabled));
        }
        if !self.address_to_port(address).is_ok_and(|p| matches!(self.ports[p], PortState::Configured { .. })) {
            return Err((context, EhciError::Disconnected));
        }
        let mut request = request;
        if request.length as usize > MAX_CONTROL_DATA_LEN {
            request.length = MAX_CONTROL_DATA_LEN as u16;
        }
        self.queue_control(address, &request, data, false, Purpose::User(context)).map_err(|(p, e)| match p {
            Purpose::User(context) => (context, e),
            Purpose::Builtin { .. } => unreachable!(),
        })
    }

    /// Schedule an isochronous OUT transfer, as `ehci::Controller::schedule_isochronous_out`
    /// does. Packets have to be at least one byte long.
    pub fn schedule_isochronous_out(
        &mut self,
        address: u8,
        endpoint: u8,
        max_packet_size: u16,
        interval: u8,
        mut context: CT,
    ) -> Result<(), (CT, EhciError)> {
        if !self.enabled {
            return Err((context, EhciError::ControllerDisabled));
        }
        if !self.address_to_port(address).is_ok_and(|p| matches!(self.ports[p], PortState::Configured { .. })) {
            return Err((context, EhciError::Disconnected));
        }
        let size = (max_packet_size & 0x3ff) as usize;
        let lengths: Vec<u16> = context.iso_packet_lengths().to_vec();
        let data = context.data_buffer();
        let (data_start, data_len) = (data.as_ptr() as usize, data.len());
        let total: usize = lengths.iter().map(|&l| l as usize).sum();
        if lengths.is_empty()
            || lengths.len() > MAX_ISO_PACKETS
            || total > data_len
            || lengths.iter().any(|&l| l == 0 || l as usize > size)
        {
            return Err((context, EhciError::InvalidTransfer));
        }

        let index = match self.endpoints.iter().position(|e| {
            e.address == address && e.number == endpoint && matches!(e.queue, Queue::Iso { .. })
        }) {
            Some(index) => index,
            None => match self.open_iso_endpoint(address, endpoint, size as u16) {
                Ok(index) => index,
                Err(e) => return Err((context, e)),
            },
        };

        let period = 1u32 << (interval.clamp(1, 16) - 1);
        let earliest = self.frame.wrapping_add(ISO_START_MARGIN_FRAMES);
        let Queue::Iso { next_frame, .. } = &self.endpoints[index].queue else { unreachable!() };
        let start = match *next_frame {
            Some(next) if next.wrapping_sub(earliest) < ISO_HORIZON_FRAMES => next,
            _ => earliest,
        };
        let end = start.wrapping_add(lengths.len() as u32 * period);
        if end.wrapping_sub(self.frame) >= ISO_HORIZON_FRAMES {
            return Err((context, EhciError::Busy));
        }

        // Group packets into TDs: consecutive frames only, at most eight, and data
        // crossing at most one page boundary.
        let per_td = if period == 1 { 8 } else { 1 };
        let mut groups: Vec<(usize, usize, usize)> = Vec::new();
        let mut offset = data_start;
        let mut first = 0;
        while first < lengths.len() {
            let first_page = offset & !(PAGE - 1);
            let mut past = first;
            let mut group_end = offset;
            while past < lengths.len() && past - first < per_td {
                let last_byte = group_end + lengths[past] as usize - 1;
                if last_byte & !(PAGE - 1) > first_page + PAGE {
                    break;
                }
                group_end += lengths[past] as usize;
                past += 1;
            }
            groups.push((first, past, offset));
            offset = group_end;
            first = past;
        }

        let mut fresh = Vec::new();
        for _ in 0..groups.len() {
            match self.iso_td_pool.alloc(IsoTd::default()) {
                Ok(td) => fresh.push(td),
                Err(e) => return Err((context, e)),
            }
        }
        let v2p = self.virt_to_phys;
        let endpoint_entry = &mut self.endpoints[index];
        let Queue::Iso { transfers, dummy, next_frame } = &mut endpoint_entry.queue else { unreachable!() };
        let new_dummy = fresh.pop().unwrap();
        let mut tds = vec![core::mem::replace(dummy, new_dummy)];
        tds.extend(fresh);
        let group_count = groups.len();
        for (i, (first, past, start_va)) in groups.into_iter().enumerate() {
            let td = &tds[i];
            let first_page = start_va & !(PAGE - 1);
            let mut va = start_va;
            for (slot, &length) in lengths[first..past].iter().enumerate() {
                let in_second_page = va & !(PAGE - 1) != first_page;
                let psw = PSW_NOT_ACCESSED
                    | if in_second_page { PSW_SECOND_PAGE } else { 0 }
                    | (va & (PAGE - 1)) as u16;
                td.packet_status[slot].set(psw);
                va += length as usize;
            }
            let mut info = IsoTdInfo(0);
            info.set_starting_frame(start.wrapping_add(first as u32 * period) as u16);
            info.set_frame_count((past - first - 1) as u32);
            info.set_delay_interrupt(if i == group_count - 1 { 0 } else { NO_INTERRUPT });
            info.set_condition_code(CC_NOT_ACCESSED);
            td.buffer_page_0.set(v2p(start_va as *const u8) as u32 & !(PAGE as u32 - 1));
            td.buffer_end.set(v2p((va - 1) as *const u8) as u32);
            let next = tds.get(i + 1).map_or(dummy.physical_address(), |t| t.physical_address());
            td.next.set(next as u32);
            td.info.set(info);
        }
        endpoint_entry.ed.tail.set(dummy.physical_address() as u32);
        *next_frame = Some(end);
        debug!("Iso OUT A={address}:EP={endpoint}: {} packets at frame {start} (now {})", lengths.len(), self.frame);
        transfers.push_back(IsoTransfer { tds, packets: lengths.len(), context });
        Ok(())
    }

    fn open_iso_endpoint(&mut self, address: u8, endpoint: u8, max_packet_size: u16) -> Result<usize, EhciError> {
        let dummy = self.iso_td_pool.alloc(IsoTd::default())?;
        let mut info = EdInfo(0);
        info.set_function_address(address);
        info.set_endpoint_number(endpoint);
        info.set_direction(ED_DIRECTION_OUT);
        info.set_isochronous_format(true);
        info.set_max_packet_size(max_packet_size);
        let dummy_phys = dummy.physical_address() as u32;
        let ed = self.ed_pool.alloc(Ed {
            info: VolatileCell::new(info),
            tail: VolatileCell::new(dummy_phys),
            head: VolatileCell::new(dummy_phys),
            next: VolatileCell::new(0),
        })?;
        debug!("Opening isochronous endpoint A={address}:EP={endpoint}");
        self.endpoints.push(Endpoint {
            ed,
            address,
            number: endpoint,
            queue: Queue::Iso { transfers: VecDeque::new(), dummy, next_frame: None },
        });
        self.relink();
        Ok(self.endpoints.len() - 1)
    }

    /// Bulk and interrupt endpoints are not supported on this controller.
    pub fn open_endpoint(&mut self, _address: u8, _endpoint: u8, _max_packet_length: u16) -> Result<(), EhciError> {
        Err(EhciError::NotSupported)
    }

    fn schedule_builtin(&mut self, port: usize, step: Step, request: ControlRequest) -> Result<(), EhciError> {
        let address = if step == Step::SetAddress { 0 } else { Self::port_to_address(port) };
        let use_descriptor_buffer = step == Step::Config && request.length as usize > MAX_CONTROL_DATA_LEN;
        self.queue_control(address, &request, &[], use_descriptor_buffer, Purpose::Builtin { port, step })
            .map_err(|(_, e)| e)
    }

    fn get_descriptor(descriptor_type: u16, length: usize) -> ControlRequest {
        ControlRequest { typ: 0x80, request: 6, value: descriptor_type << 8, index: 0, length: length as u16 }
    }

    fn advance_enumeration(
        &mut self,
        port: usize,
        step: Step,
        result: Result<Vec<u8>, EhciError>,
        tick_count: usize,
        handler: &mut impl EventHandler<CT>,
    ) -> Result<(), EhciError> {
        if step == Step::Config {
            if let Some(buffer) = self.descriptor_buffer.as_mut() {
                buffer.busy = false;
            }
        }
        let expected = match (&self.ports[port], step) {
            (PortState::Addressing { .. }, Step::SetAddress) => true,
            (PortState::Describing { .. }, Step::DeviceHead | Step::Device | Step::ConfigHead | Step::Config) => true,
            (PortState::Configuring { .. }, Step::SetConfiguration) => true,
            _ => false,
        };
        if !expected {
            debug!("Spurious {step:?} result on port {port}");
            return Ok(());
        }
        let data = result.map_err(|_| EhciError::SetupUnsuccessful)?;
        let address = Self::port_to_address(port);
        match step {
            Step::SetAddress => {
                self.ports[port] = PortState::AddressRecovery { since: tick_count };
            }
            Step::DeviceHead => {
                let max_packet_size = *data.get(7).ok_or(EhciError::DescriptorError)?;
                if !matches!(max_packet_size, 8 | 16 | 32 | 64) {
                    return Err(EhciError::DescriptorError);
                }
                let index = self.control_endpoint_index(address).ok_or(EhciError::EndpointNotOpen)?;
                let ed = &self.endpoints[index].ed;
                let mut info = ed.info.get();
                info.set_max_packet_size(max_packet_size as u16);
                ed.info.set(info);
                self.schedule_builtin(port, Step::Device, Self::get_descriptor(DESCRIPTOR_TYPE_DEVICE, DEVICE_DESCRIPTOR_LEN))?;
                self.ports[port] = PortState::Describing { since: tick_count, bytes: Vec::new() };
            }
            Step::Device => {
                if data.len() < DEVICE_DESCRIPTOR_LEN {
                    return Err(EhciError::DescriptorError);
                }
                self.schedule_builtin(port, Step::ConfigHead, Self::get_descriptor(DESCRIPTOR_TYPE_CONFIG, CONFIG_HEADER_LEN))?;
                self.ports[port] = PortState::Describing { since: tick_count, bytes: data };
            }
            Step::ConfigHead => {
                if data.len() < 4 {
                    return Err(EhciError::DescriptorError);
                }
                let total = u16::from_le_bytes([data[2], data[3]]) as usize;
                let available = match self.descriptor_buffer.as_ref() {
                    Some(b) if !b.busy => b.buffer.len(),
                    _ => MAX_CONTROL_DATA_LEN,
                };
                if total > available {
                    warn!("Configuration on port {port} is {total} bytes, keeping the first {available}");
                }
                self.schedule_builtin(port, Step::Config, Self::get_descriptor(DESCRIPTOR_TYPE_CONFIG, total.min(available)))?;
                let PortState::Describing { bytes, .. } = &self.ports[port] else { unreachable!() };
                self.ports[port] = PortState::Describing { since: tick_count, bytes: bytes.clone() };
            }
            Step::Config => {
                let PortState::Describing { bytes, .. } = &self.ports[port] else { unreachable!() };
                let mut bytes = bytes.clone();
                bytes.extend_from_slice(&data);
                let descriptor = DescriptorSet::new(bytes)?;
                let request = ControlRequest { typ: 0, request: 9 /* SET_CONFIGURATION */, value: 1, index: 0, length: 0 };
                self.schedule_builtin(port, Step::SetConfiguration, request)?;
                self.ports[port] = PortState::Configuring { since: tick_count, descriptor };
            }
            Step::SetConfiguration => {
                let PortState::Configuring { descriptor, .. } = &self.ports[port] else { unreachable!() };
                let descriptor = descriptor.clone();
                info!("Full-speed port {port} configured successfully");
                debug!("Descriptor: {descriptor:#?}");
                self.ports[port] = PortState::Configured { descriptor: descriptor.clone() };
                handler.device_connected(self, address, descriptor);
            }
        }
        Ok(())
    }

    fn update_ports(&mut self, tick_count: usize, handler: &mut impl EventHandler<CT>) -> Result<(), EhciError> {
        let mut disconnects = Vec::new();
        let mut cancelled = Vec::new();
        for port_number in 0..self.ports.len() {
            let status = self.regs().rh_port_status[port_number].get();
            let connected = status & PORT_CURRENT_CONNECT_STATUS != 0;
            let address = Self::port_to_address(port_number);
            let state = self.ports[port_number].clone();
            if !connected && !matches!(state, PortState::Disconnected | PortState::Debouncing { .. }) {
                info!("Full-speed port {port_number} disconnected");
                if matches!(state, PortState::Configured { .. }) {
                    disconnects.push(port_number);
                }
                self.remove_address(address, tick_count, &mut cancelled);
                self.ports[port_number] = PortState::Disconnected;
                continue;
            }
            let addressing_elsewhere = self
                .ports
                .iter()
                .enumerate()
                .any(|(i, p)| i != port_number && matches!(p, PortState::Addressing { .. }));
            let next = match state {
                PortState::Disconnected => {
                    if connected {
                        // Clear the change bits so the root hub interrupt goes quiet.
                        self.regs().rh_port_status[port_number].set(PORT_CHANGE_BITS);
                        Some(PortState::Debouncing { since: tick_count })
                    } else {
                        None
                    }
                }
                PortState::Debouncing { since } => {
                    if !connected {
                        Some(PortState::Disconnected)
                    } else if tick_count.wrapping_sub(since) >= ATTACH_DEBOUNCE_MS {
                        info!("Resetting full-speed port {port_number}");
                        self.regs().rh_port_status[port_number].set(PORT_RESET_STATUS);
                        Some(PortState::Resetting { since: tick_count })
                    } else {
                        None
                    }
                }
                PortState::Resetting { since } => {
                    if status & PORT_RESET_STATUS_CHANGE != 0 && status & PORT_ENABLE_STATUS != 0 {
                        self.regs().rh_port_status[port_number].set(PORT_RESET_STATUS_CHANGE);
                        Some(PortState::ResetRecovery { since: tick_count })
                    } else if tick_count.wrapping_sub(since) >= RESET_TIMEOUT_MS {
                        debug!("Full-speed port {port_number} did not come out of reset enabled: {status:#x}");
                        Some(PortState::Disconnected)
                    } else {
                        None
                    }
                }
                PortState::ResetRecovery { since } => {
                    if tick_count.wrapping_sub(since) < RESET_RECOVERY_MS || addressing_elsewhere {
                        None
                    } else {
                        let low_speed = status & PORT_LOW_SPEED_DEVICE != 0;
                        let ed = &self.endpoints[0].ed;
                        let mut info = ed.info.get();
                        info.set_low_speed(low_speed);
                        info.set_max_packet_size(8);
                        ed.info.set(info);
                        let request =
                            ControlRequest { typ: 0, request: 5 /* SET_ADDRESS */, value: address as u16, index: 0, length: 0 };
                        self.schedule_builtin(port_number, Step::SetAddress, request)?;
                        Some(PortState::Addressing { since: tick_count })
                    }
                }
                PortState::AddressRecovery { since } => {
                    if tick_count.wrapping_sub(since) < SET_ADDRESS_RECOVERY_MS {
                        None
                    } else {
                        let low_speed = status & PORT_LOW_SPEED_DEVICE != 0;
                        if self.control_endpoint_index(address).is_none() {
                            let endpoint =
                                Self::new_control_endpoint(&mut self.ed_pool, &mut self.td_pool, address, low_speed, 8)?;
                            self.endpoints.push(endpoint);
                            self.relink();
                        }
                        self.schedule_builtin(port_number, Step::DeviceHead, Self::get_descriptor(DESCRIPTOR_TYPE_DEVICE, 8))?;
                        Some(PortState::Describing { since: tick_count, bytes: Vec::new() })
                    }
                }
                PortState::Addressing { since } if tick_count.wrapping_sub(since) >= SET_ADDRESS_TIMEOUT_MS => {
                    debug!("Timed out addressing full-speed port {port_number}");
                    Some(PortState::Disconnected)
                }
                PortState::Describing { since, .. } if tick_count.wrapping_sub(since) >= DATA_STAGE_TIMEOUT_MS => {
                    debug!("Timed out reading descriptors on full-speed port {port_number}");
                    self.remove_address(address, tick_count, &mut cancelled);
                    Some(PortState::Disconnected)
                }
                PortState::Configuring { since, .. }
                    if tick_count.wrapping_sub(since) >= SET_CONFIGURATION_TIMEOUT_MS =>
                {
                    debug!("Timed out configuring full-speed port {port_number}");
                    self.remove_address(address, tick_count, &mut cancelled);
                    Some(PortState::Disconnected)
                }
                _ => None,
            };
            if let Some(next) = next {
                self.ports[port_number] = next;
            }
        }
        for context in cancelled {
            handler.transfer_result(self, Err(EhciError::Disconnected), context);
        }
        for port_number in disconnects {
            handler.device_disconnected(self, Self::port_to_address(port_number));
        }
        Ok(())
    }

    // As in the EHCI driver, one device per root port and its address is the port's.
    fn port_to_address(port: usize) -> u8 { port as u8 + 1 }

    fn address_to_port(&self, address: u8) -> Result<usize, EhciError> {
        if address < 1 || address as usize > self.ports.len() {
            Err(EhciError::InvalidAddress)
        } else {
            Ok(address as usize - 1)
        }
    }

    /// Enable interrupts: done queue writebacks, root hub changes and fatal errors.
    pub fn enable_interrupts(&self) {
        self.regs().interrupt_enable.set(
            INTERRUPT_MASTER_ENABLE
                | INTERRUPT_WRITEBACK_DONE_HEAD
                | INTERRUPT_ROOT_HUB_STATUS_CHANGE
                | INTERRUPT_UNRECOVERABLE_ERROR,
        );
    }

    /// Clear all triggered interrupts so the interrupt signal can go back to low. The
    /// done queue itself is not used: completions are read off the endpoint queues.
    pub fn acknowledge_interrupts(&self) {
        let regs = self.regs();
        let status = regs.interrupt_status.get();
        if status & INTERRUPT_UNRECOVERABLE_ERROR != 0 {
            log::error!("OHCI unrecoverable error");
        }
        if status & INTERRUPT_SCHEDULING_OVERRUN != 0 {
            log::warn!("OHCI scheduling overrun");
        }
        regs.interrupt_status.set(status);
    }
}

impl<CT> core::fmt::Debug for Controller<CT> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let regs = unsafe { &*self.regs };
        f.debug_struct("OhciController")
            .field("control", &regs.control.get())
            .field("interrupt_status", &regs.interrupt_status.get())
            .field("ports", &self.ports)
            .finish()
    }
}
