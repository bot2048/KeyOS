// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! The service on hardware: claims the first USB audio device with a playback stream,
//! sets it up, and keeps its isochronous endpoint fed from the queue apps fill.
//!
//! Two worker threads take turns: while one transfer plays, the other thread fills and
//! submits the next, which the host schedules right behind it. Each thread fills its
//! buffer and submits it within microseconds, and only once its previous transfer is
//! done, while the other thread's transfer still has most of its time left, so they
//! cannot overtake each other. When the queue has stayed empty for a while the workers
//! stop submitting and wait, so an idle device costs no wakeups.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use audio::{messages::*, AudioError, AudioStatus, SAMPLE_RATE};
use server::{BlockingScalarHandler, LendMutHandler, ScalarHandler};
use uac::pcm::{self, Resampler, StreamFormat};
use uac::requests::{self, ControlRequest};
use uac::schedule::{period_microframes, PacketSchedule};
use uac::{AudioFunction, OutputChoice, Speed, StreamingAlt, UacVersion};
use usb::host::api::{IsoBuffer, UsbEvent};
use usb::UsbError;

usb::use_host_api!();

/// Frames apps can queue ahead of the device: a quarter of a second.
const QUEUE_FRAMES: usize = SAMPLE_RATE as usize / 4;
/// Length of one isochronous transfer. Two are in flight, so this is also about the
/// latency the service adds.
const TRANSFER_MS: u32 = 16;
/// Transfers of silence sent after the queue ran dry before the workers go idle. Short
/// gaps between an app's writes stay inaudible this way.
const IDLE_AFTER_EMPTY_TRANSFERS: usize = 8;
/// How often an idle worker checks whether its device went away.
const IDLE_POLL: Duration = Duration::from_millis(250);
/// Volume until an app sets one: about -12 dB, so a full-scale test tone is not too loud.
const DEFAULT_VOLUME: u8 = 50;
/// Longest RANGE answer asked for: five subranges, within one control data stage.
const RANGE_READ_LEN: usize = 2 + 5 * 12;

/// State the workers share with the server thread.
struct Shared {
    stream: Mutex<StreamState>,
    wake: Condvar,
    volume: AtomicU8,
}

struct StreamState {
    queue: VecDeque<[i16; 2]>,
    resampler: Resampler,
    schedule: PacketSchedule,
}

/// What a worker needs to know about the stream.
#[derive(Clone)]
struct StreamPlan {
    format: StreamFormat,
    packets_per_transfer: usize,
    max_frames_per_packet: usize,
}

struct Output {
    handle: usize,
    status: AudioStatus,
    running: Arc<AtomicBool>,
    // Held so the claim lives as long as the output.
    _device: ConnectedUsbDevice,
}

#[derive(server::Server)]
#[name = "os/audio"]
pub struct AudioServer {
    usb: UsbHost,
    shared: Arc<Shared>,
    output: Option<Output>,
}

impl Default for AudioServer {
    fn default() -> Self {
        Self {
            usb: UsbHost::default(),
            shared: Arc::new(Shared {
                stream: Mutex::new(StreamState {
                    queue: VecDeque::with_capacity(QUEUE_FRAMES),
                    resampler: Resampler::new(SAMPLE_RATE, SAMPLE_RATE),
                    schedule: PacketSchedule::new(SAMPLE_RATE, 8),
                }),
                wake: Condvar::new(),
                volume: AtomicU8::new(DEFAULT_VOLUME),
            }),
            output: None,
        }
    }
}

impl server::Server for AudioServer {
    fn on_start(&mut self, context: &mut server::ServerContext<Self>) { self.usb.subscribe(context); }
}

fn control(device: &mut ConnectedUsbDevice, request: ControlRequest) -> Result<Vec<u8>, UsbError> {
    device.control_transfer(
        request.request_type,
        request.request,
        request.value,
        request.index,
        request.data,
        request.read_len,
    )
}

/// Pick the setting to play on, asking a UAC2 clock which rates it has.
fn negotiate(
    device: &mut ConnectedUsbDevice,
    function: &AudioFunction,
    speed: Speed,
) -> Option<OutputChoice> {
    let first = uac::choose_output(function, speed, None)?;
    let alt = first.alt(function);
    let (UacVersion::Uac2, Some(clock)) = (function.version, alt.clock_source) else {
        return Some(first);
    };
    let request = requests::uac2_get_sample_rate_range(function.control_interface, clock, RANGE_READ_LEN);
    match control(device, request) {
        Ok(answer) => {
            let ranges = requests::parse_uac2_rate_ranges(&answer);
            log::info!("Clock {clock} offers {ranges:?}");
            if ranges.is_empty() {
                Some(first)
            } else {
                uac::choose_output(function, speed, Some(&ranges)).or(Some(first))
            }
        }
        Err(e) => {
            log::info!("Clock {clock} did not answer RANGE ({e:?}), assuming {}", first.rate);
            Some(first)
        }
    }
}

/// Select the alternate setting and rate, and open the path to the output terminal.
fn start_stream(
    device: &mut ConnectedUsbDevice,
    function: &AudioFunction,
    alt: &StreamingAlt,
    rate: u32,
) -> Result<(), UsbError> {
    control(device, requests::set_interface(alt.interface, 0))?;
    match function.version {
        UacVersion::Uac1 => {
            control(device, requests::set_interface(alt.interface, alt.alt_setting))?;
            if alt.sampling_frequency_control {
                if let Err(e) = control(device, requests::uac1_set_sample_rate(alt.endpoint_address, rate)) {
                    log::warn!("Could not set the rate to {rate}: {e:?}");
                }
            }
        }
        UacVersion::Uac2 => {
            if let Some(clock) = alt.clock_source {
                let request = requests::uac2_set_sample_rate(function.control_interface, clock, rate);
                if let Err(e) = control(device, request) {
                    log::warn!("Could not set clock {clock} to {rate}: {e:?}");
                }
            }
            control(device, requests::set_interface(alt.interface, alt.alt_setting))?;
        }
    }
    if let Some(unit) = &alt.feature_unit {
        // Unmuted at 0 dB; the volume apps set is applied to the samples. A device that
        // has neither control, or refuses the value, plays as it is.
        let interface = function.control_interface;
        if unit.master_mute {
            control(device, requests::set_mute(interface, unit.unit_id, 0, false)).ok();
        }
        if unit.master_volume {
            control(device, requests::set_volume(interface, unit.unit_id, 0, 0)).ok();
        }
        for &channel in &unit.channel_volume {
            control(device, requests::set_volume(interface, unit.unit_id, channel, 0)).ok();
        }
    }
    Ok(())
}

impl AudioServer {
    fn connect(&mut self, handle: usize, bytes: &[u8], full_speed: bool) {
        let function = match uac::parse_audio_function(bytes) {
            Ok(function) => function,
            Err(e) => {
                log::debug!("Device {handle} is no audio output: {e:?}");
                return;
            }
        };
        if self.output.is_some() {
            log::info!("Already playing on a device, ignoring audio device {handle}");
            return;
        }
        let speed = if full_speed { Speed::Full } else { Speed::High };
        let mut device = match self.usb.claim(handle) {
            Ok(device) => device,
            Err(e) => {
                log::warn!("Could not claim audio device {handle}: {e:?}");
                return;
            }
        };
        let Some(choice) = negotiate(&mut device, &function, speed) else {
            log::warn!("No playback setting of device {handle} fits its endpoint");
            return;
        };
        let alt = choice.alt(&function).clone();
        let rate = choice.rate;
        if let Err(e) = start_stream(&mut device, &function, &alt, rate) {
            log::warn!("Could not start the stream on device {handle}: {e:?}");
            return;
        }
        let Some(period) = period_microframes(speed, alt.interval) else { return };
        log::info!(
            "Playing on device {handle}: {:?}, interface {} alt {}, {} ch {}-bit at {rate} Hz, {:?}",
            function.version,
            alt.interface,
            alt.alt_setting,
            alt.channels,
            alt.bit_resolution,
            alt.sync,
        );

        let schedule = PacketSchedule::new(rate, period);
        let packets_per_transfer =
            ((TRANSFER_MS * 8 / period).max(1) as usize).min(usb::host::messages::MAX_ISO_PACKETS);
        let plan = StreamPlan {
            format: StreamFormat::of(&alt),
            packets_per_transfer,
            max_frames_per_packet: schedule.max_frames(),
        };
        {
            let mut stream = self.shared.stream.lock().unwrap();
            stream.queue.clear();
            stream.resampler = Resampler::new(SAMPLE_RATE, rate);
            stream.schedule = schedule;
        }

        let running = Arc::new(AtomicBool::new(true));
        let endpoint_number = alt.endpoint_address & 0x0f;
        for worker in 0..2 {
            let endpoint =
                device.isochronous_out_endpoint(endpoint_number, alt.max_packet_size, alt.interval);
            let (shared, running, plan) = (self.shared.clone(), running.clone(), plan.clone());
            std::thread::spawn(move || {
                xous::set_thread_priority(xous::ThreadPriority::System5).ok();
                // The second worker queues behind the first one's opening transfer.
                std::thread::sleep(Duration::from_millis(2 * worker));
                stream(shared, running, endpoint, plan)
            });
        }
        self.output = Some(Output {
            handle,
            status: AudioStatus { connected: true, device_rate: rate, full_speed },
            running,
            _device: device,
        });
    }

    fn disconnect(&mut self, handle: usize) {
        if self.output.as_ref().map(|o| o.handle) != Some(handle) {
            return;
        }
        log::info!("Audio device {handle} went away");
        let output = self.output.take().unwrap();
        output.running.store(false, Ordering::SeqCst);
        self.shared.stream.lock().unwrap().queue.clear();
        self.shared.wake.notify_all();
    }
}

/// Feed one endpoint until the device goes away.
fn stream(shared: Arc<Shared>, running: Arc<AtomicBool>, mut endpoint: UsbIsoOutEndpoint, plan: StreamPlan) {
    let frame_bytes = plan.format.frame_bytes();
    let mut buffer =
        match IsoBuffer::new(plan.packets_per_transfer * plan.max_frames_per_packet * frame_bytes) {
            Ok(buffer) => buffer,
            Err(e) => {
                log::error!("Could not allocate a transfer buffer: {e:?}");
                return;
            }
        };
    let mut frames = vec![[0i16; 2]; plan.max_frames_per_packet];
    let mut empty_transfers = 0;

    while running.load(Ordering::SeqCst) {
        {
            let mut guard = shared.stream.lock().unwrap();
            if empty_transfers >= IDLE_AFTER_EMPTY_TRANSFERS {
                while guard.queue.is_empty() && running.load(Ordering::SeqCst) {
                    guard = shared.wake.wait_timeout(guard, IDLE_POLL).unwrap().0;
                }
                empty_transfers = 0;
            }
            let gain = pcm::gain_for_volume(shared.volume.load(Ordering::Relaxed));
            let state = &mut *guard;
            let mut played = 0;
            buffer.clear();
            for _ in 0..plan.packets_per_transfer {
                let count = state.schedule.next_frames();
                let Some(packet) = buffer.push_packet(count * frame_bytes) else { break };
                for frame in &mut frames[..count] {
                    *frame = state.resampler.next_frame(|| match state.queue.pop_front() {
                        Some(f) => {
                            played += 1;
                            f
                        }
                        None => [0; 2],
                    });
                }
                pcm::apply_gain(&mut frames[..count], gain);
                pcm::encode_stereo(&frames[..count], plan.format, packet);
            }
            empty_transfers = if played == 0 { empty_transfers + 1 } else { 0 };
        }
        if !running.load(Ordering::SeqCst) {
            break;
        }
        match endpoint.send(&mut buffer) {
            Ok(_) => {}
            Err(UsbError::NotFound) | Err(UsbError::NotClaimed) => break,
            Err(e) => {
                log::warn!("Isochronous transfer failed: {e:?}");
                std::thread::sleep(Duration::from_millis(TRANSFER_MS as u64));
            }
        }
    }
    log::debug!("Audio worker done");
}

impl server::ArchiveEventHandler<UsbEvent> for AudioServer {
    fn handle(
        &mut self,
        msg: server::Owned<UsbEvent>,
        _sender: xous::PID,
        _context: &mut server::ServerContext<Self>,
    ) {
        let Ok(event) = msg.deserialize() else { return };
        match event {
            UsbEvent::Connect { handle, descriptors, full_speed } => {
                self.connect(handle, descriptors.bytes(), full_speed)
            }
            UsbEvent::Disconnect { handle } => self.disconnect(handle),
        }
    }
}

impl LendMutHandler<Play> for AudioServer {
    fn handle(
        &mut self,
        msg: Play,
        _sender: xous::PID,
        _context: &mut server::ServerContext<Self>,
    ) -> Result<usize, AudioError> {
        if self.output.is_none() {
            return Err(AudioError::NotConnected);
        }
        let available = msg.buf.as_slice::<[i16; 2]>();
        if msg.frames > available.len() {
            return Err(AudioError::InvalidBuffer);
        }
        let mut stream = self.shared.stream.lock().unwrap();
        let taken = msg.frames.min(QUEUE_FRAMES.saturating_sub(stream.queue.len()));
        stream.queue.extend(&available[..taken]);
        drop(stream);
        self.shared.wake.notify_all();
        Ok(taken)
    }
}

impl ScalarHandler<SetVolume> for AudioServer {
    fn handle(&mut self, msg: SetVolume, _sender: xous::PID, _context: &mut server::ServerContext<Self>) {
        self.shared.volume.store(msg.0.min(100), Ordering::Relaxed);
    }
}

impl ScalarHandler<Stop> for AudioServer {
    fn handle(&mut self, _msg: Stop, _sender: xous::PID, _context: &mut server::ServerContext<Self>) {
        self.shared.stream.lock().unwrap().queue.clear();
    }
}

impl BlockingScalarHandler<GetStatus> for AudioServer {
    fn handle(
        &mut self,
        _msg: GetStatus,
        _sender: xous::PID,
        _context: &mut server::ServerContext<Self>,
    ) -> AudioStatus {
        self.output.as_ref().map(|o| o.status).unwrap_or_default()
    }
}
