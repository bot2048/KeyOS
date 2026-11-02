//! Periodic schedule, used for isochronous OUT streams on high-speed endpoints.
//!
//! Every transfer is a run of packets, one per service opportunity of the endpoint,
//! described by one iTD per frame it touches. A stream continues where its previous
//! transfer ended, so back to back transfers play without a gap, and starts over a few
//! frames ahead of the controller when it ran dry.
extern crate alloc;

use alloc::vec::Vec;

use log::debug;
use vcell::VolatileCell;

use crate::error::EhciError;
use crate::pool::{Pool, PoolElementHandle};
use crate::registers::{Itd, ItdTransaction};
use crate::TransferContext;

/// Entries in the periodic frame list, the size selected by a reset USBCMD.
pub const FRAME_LIST_LEN: usize = 1024;
/// Periodic frame list memory: 4 KiB aligned and uncached.
pub type FrameList = [VolatileCell<u32>; FRAME_LIST_LEN];
/// Most packets one isochronous transfer may carry.
pub const MAX_ISO_PACKETS: usize = 256;

const TERMINATE: u32 = 1;
const UFRAMES_PER_FRAME: u64 = 8;
/// FRINDEX counts microframes in its low 14 bits for a 1024 entry frame list.
const FRINDEX_MASK: u32 = 0x3fff;
/// Lead over the controller when a stream (re)starts, on top of the scheduling
/// threshold, so the first iTD is linked well before the controller reaches its frame.
const START_MARGIN_UFRAMES: u64 = 2 * UFRAMES_PER_FRAME;
/// Furthest ahead work may be queued: past a full lap of the frame list a slot would
/// come round again before its time.
const HORIZON_UFRAMES: u64 = (FRAME_LIST_LEN as u64 - 16) * UFRAMES_PER_FRAME;
const PAGE: usize = 0x1000;

struct IsoTransfer<CT> {
    address: u8,
    endpoint: u8,
    /// Frame list slot and descriptor of every frame the transfer touches.
    itds: Vec<(usize, PoolElementHandle<Itd>)>,
    /// Microframe after the last packet.
    end_uframe: u64,
    packets: usize,
    context: CT,
}

struct Stream {
    address: u8,
    endpoint: u8,
    next_uframe: u64,
}

pub(crate) struct PeriodicSchedule<CT> {
    frame_list: &'static FrameList,
    frame_list_phys: u32,
    itd_pool: Pool<Itd>,
    transfers: Vec<IsoTransfer<CT>>,
    streams: Vec<Stream>,
    cancelled: Vec<(CT, EhciError)>,
    /// Microframes since the schedule was enabled, extended past FRINDEX's wrap.
    now: u64,
    last_frindex: u32,
    threshold: u64,
}

impl<CT: TransferContext> PeriodicSchedule<CT> {
    pub fn new(
        frame_list: &'static FrameList,
        frame_list_phys: u32,
        itd_pool: Pool<Itd>,
        threshold_uframes: u32,
    ) -> Self {
        for entry in frame_list {
            entry.set(TERMINATE);
        }
        Self {
            frame_list,
            frame_list_phys,
            itd_pool,
            transfers: Vec::new(),
            streams: Vec::new(),
            cancelled: Vec::new(),
            now: 0,
            last_frindex: 0,
            threshold: threshold_uframes as u64,
        }
    }

    pub fn frame_list_phys(&self) -> u32 { self.frame_list_phys }

    /// Restart the clock after a controller reset set FRINDEX back.
    pub fn reset_clock(&mut self, frindex: u32) {
        self.last_frindex = frindex & FRINDEX_MASK;
        self.streams.clear();
    }

    /// Bring the extended microframe count up to FRINDEX. FRINDEX wraps every 2048 ms,
    /// so this has to run more often than that, which `Controller::work` does.
    pub fn advance_clock(&mut self, frindex: u32) {
        let frindex = frindex & FRINDEX_MASK;
        self.now += (frindex.wrapping_sub(self.last_frindex) & FRINDEX_MASK) as u64;
        self.last_frindex = frindex;
    }

    /// Queue one packet per `period` microframes from the lengths the context gives, the
    /// payloads packed back to back in its data buffer.
    pub fn schedule_out(
        &mut self,
        address: u8,
        endpoint: u8,
        max_packet_size: u16,
        period: u32,
        mut context: CT,
        virt_to_phys: impl Fn(*const u8) -> usize,
    ) -> Result<(), (CT, EhciError)> {
        let size = (max_packet_size & 0x7ff) as usize;
        let mult = ((max_packet_size >> 11) & 0x3) as u32 + 1;
        let period = period.max(1) as u64;
        let lengths: Vec<u16> = context.iso_packet_lengths().to_vec();
        let data = context.data_buffer();
        let (data_start, data_len) = (data.as_ptr() as usize, data.len());
        let total: usize = lengths.iter().map(|&l| l as usize).sum();
        if lengths.is_empty()
            || lengths.len() > MAX_ISO_PACKETS
            || total > data_len
            || lengths.iter().any(|&l| l as usize > size * mult as usize)
        {
            return Err((context, EhciError::InvalidTransfer));
        }

        let earliest = self.now + self.threshold + START_MARGIN_UFRAMES;
        let stream = self.streams.iter().position(|s| s.address == address && s.endpoint == endpoint);
        let start = match stream.map(|i| self.streams[i].next_uframe) {
            Some(next) if next >= earliest => next,
            _ => earliest.next_multiple_of(period.max(UFRAMES_PER_FRAME)),
        };
        let end = start + lengths.len() as u64 * period;
        if end > self.now + HORIZON_UFRAMES {
            return Err((context, EhciError::Busy));
        }

        let mut itds: Vec<(usize, PoolElementHandle<Itd>)> = Vec::new();
        let mut offset = data_start;
        let mut first = 0;
        while first < lengths.len() {
            let frame = (start + first as u64 * period) / UFRAMES_PER_FRAME;
            let mut past = first;
            while past < lengths.len() && (start + past as u64 * period) / UFRAMES_PER_FRAME == frame {
                past += 1;
            }
            let bytes: usize = lengths[first..past].iter().map(|&l| l as usize).sum();
            let base = offset & !(PAGE - 1);
            // Eight packets of at most 3072 bytes from anywhere in a page span at most seven.
            let pages = (offset + bytes.max(1) - 1 - base) / PAGE + 1;

            let itd = Itd::default();
            for page in 0..pages {
                itd.buffers[page].set(virt_to_phys((base + page * PAGE) as *const u8) as u32 & !0xfff);
            }
            itd.buffers[0].set(itd.buffers[0].get() | (endpoint as u32 & 0xf) << 8 | (address as u32 & 0x7f));
            itd.buffers[1].set(itd.buffers[1].get() | size as u32);
            itd.buffers[2].set(itd.buffers[2].get() | mult);
            for (index, &length) in lengths.iter().enumerate().take(past).skip(first) {
                let uframe = ((start + index as u64 * period) % UFRAMES_PER_FRAME) as usize;
                let mut transaction = ItdTransaction(0);
                transaction.set_offset((offset & 0xfff) as u32);
                transaction.set_page(((offset - base) / PAGE) as u32);
                transaction.set_length(length as u32);
                transaction.set_interrupt_on_complete(index == lengths.len() - 1);
                transaction.set_active(true);
                itd.transactions[uframe].set(transaction);
                offset += length as usize;
            }
            match self.itd_pool.alloc(itd) {
                Ok(handle) => itds.push(((frame % FRAME_LIST_LEN as u64) as usize, handle)),
                Err(e) => return Err((context, e)),
            }
            first = past;
        }

        // Link only once everything is built, so a failure above leaves nothing behind.
        for (slot, itd) in &itds {
            itd.next.set(self.frame_list[*slot].get());
            self.frame_list[*slot].set(itd.to_controller_ptr().ptr);
        }
        debug!("Iso OUT A={address}:EP={endpoint}: {} packets at uframe {start} (now {})", lengths.len(), self.now);

        match stream {
            Some(i) => self.streams[i].next_uframe = end,
            None => self.streams.push(Stream { address, endpoint, next_uframe: end }),
        }
        self.transfers.push(IsoTransfer {
            address,
            endpoint,
            itds,
            end_uframe: end,
            packets: lengths.len(),
            context,
        });
        Ok(())
    }

    fn unlink(&self, slot: usize, itd: &PoolElementHandle<Itd>) {
        let phys = itd.to_controller_ptr().ptr;
        let next = itd.next.get();
        if self.frame_list[slot].get() == phys {
            self.frame_list[slot].set(next);
            return;
        }
        // Not at the head of its slot: a later transfer of another stream went in front.
        for (other_slot, other) in self.transfers.iter().flat_map(|t| t.itds.iter()) {
            if *other_slot == slot && other.next.get() == phys {
                other.next.set(next);
                return;
            }
        }
    }

    /// Fail every transfer to `address`, for a device that went away.
    pub fn cancel_address(&mut self, address: u8, error: EhciError) {
        let mut i = 0;
        while i < self.transfers.len() {
            if self.transfers[i].address == address {
                let transfer = self.transfers.remove(i);
                for (slot, itd) in &transfer.itds {
                    self.unlink(*slot, itd);
                }
                self.cancelled.push((transfer.context, error));
            } else {
                i += 1;
            }
        }
        self.streams.retain(|s| s.address != address);
    }

    /// Fail everything, for a controller going down.
    pub fn flush(&mut self) -> Vec<(CT, EhciError)> {
        for entry in self.frame_list {
            entry.set(TERMINATE);
        }
        self.streams.clear();
        let mut result = core::mem::take(&mut self.cancelled);
        result.extend(self.transfers.drain(..).map(|t| (t.context, EhciError::Cancelled)));
        result
    }

    /// Collect finished transfers: the packets sent, or an error if none went out. A
    /// transfer whose time has passed is finished too; a packet the controller never got
    /// to stays active forever.
    pub fn work(&mut self) -> Vec<(CT, Result<usize, EhciError>)> {
        let mut result: Vec<(CT, Result<usize, EhciError>)> =
            self.cancelled.drain(..).map(|(context, e)| (context, Err(e))).collect();
        let mut i = 0;
        while i < self.transfers.len() {
            let transfer = &self.transfers[i];
            let idle = transfer.itds.iter().all(|(_, itd)| itd.transactions.iter().all(|t| !t.get().active()));
            let overdue = self.now >= transfer.end_uframe + UFRAMES_PER_FRAME;
            if !(idle || overdue) {
                i += 1;
                continue;
            }
            let transfer = self.transfers.remove(i);
            let mut failed = 0;
            for (slot, itd) in &transfer.itds {
                self.unlink(*slot, itd);
                for t in itd.transactions.iter().map(|t| t.get()) {
                    if t.active() || t.transaction_error() || t.babble() || t.buffer_error() {
                        failed += 1;
                    }
                }
            }
            if failed > 0 {
                debug!("Iso OUT A={}:EP={}: {failed} packets missed", transfer.address, transfer.endpoint);
            }
            let sent = transfer.packets.saturating_sub(failed);
            let outcome = if sent > 0 { Ok(sent) } else { Err(EhciError::TransferError) };
            result.push((transfer.context, outcome));
        }
        result
    }
}
