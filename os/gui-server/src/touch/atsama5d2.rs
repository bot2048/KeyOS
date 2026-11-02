// SPDX-FileCopyrightText: 2024 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::{Duration, Instant};

use ft3269::{Frame, FrameTracker, Ft3269, PowerMode, TouchEventKind, FRAME_LEN};
use gpio::{GpioApi, GpioPin};
use gui_server_api::touch::{Touch, TouchKind};
use i2c::Peripheral;
use server::{MessageId as _, ScalarHandler, ServerContext};
use xous_ticktimer::TicktimerCallback;

use crate::{gpio::GpioPermissions, handlers::TouchPollTimerCallback, Gui};

/// While a finger is down, the controller is read again if it doesn't interrupt for this long, so that a
/// missed lift interrupt doesn't leave the finger down.
const TOUCH_POLL_INTERVAL_MS: usize = 50;

/// The controller's reports right after a reset can't be trusted. Its first frame after this long tells
/// which fingers were already touching.
const RESET_QUIET_TIME: Duration = Duration::from_millis(300);

/// Not a valid frame (its point count is 10), so a frame read from the controller is never equal to it.
const NO_FRAME: [u8; FRAME_LEN] = [0xaa; FRAME_LEN];

/// After this many reads in a row fail (about a second of polling), the controller is reset.
const MAX_READ_FAILURES: u32 = 20;

/// After this many resets in a row that didn't help, we stop polling and wait for an interrupt instead.
const MAX_RESETS: u32 = 3;

/// While a finger is down the controller reports it on every scan, so a longer time between two reads
/// means that we missed scans (we were busy, or reads failed). A finger can't be lifted and put down
/// elsewhere in less time.
const MISSED_SCANS_TIME: Duration = Duration::from_millis(60);

i2c::use_api!();

pub(crate) struct HwTouchState {
    ft3269: ft3269::Ft3269<I2cPeripheral>,
    gpio_api: GpioApi<GpioPermissions>,
    enabled: bool,
    tracker: FrameTracker,
    poll_callback: Option<TicktimerCallback>,
    /// Interrupts are ignored until then, after a reset.
    quiet_until: Option<Instant>,
    /// The last frame read, to tell reading the same scan again from a new one.
    last_frame: [u8; FRAME_LEN],
    /// The last valid frame read had touch points.
    last_frame_touched: bool,
    /// When the controller was last read successfully.
    last_read: Option<Instant>,
    read_failures: u32,
    resets: u32,
}

impl Default for HwTouchState {
    fn default() -> Self {
        log::debug!("Claiming touch controller I2C peripheral");
        let i2c_api = I2cApi::default();
        let i2c_periph =
            i2c_api.claim_peripheral(Peripheral::TouchController).expect("Could not claim touch peripheral");
        let ft3269 = Ft3269::new(i2c_periph);

        let gpio_api = GpioApi::<GpioPermissions>::default();
        gpio_api
            .claim_pin(GpioPin::CtpRstB, gpio::PinSettings::OutputHigh, false)
            .expect("Could not claim the touch reset pin");

        HwTouchState {
            ft3269,
            gpio_api,
            // The bootloader resets and enables the controller for us
            enabled: true,
            // Fingers already touching while we start up are ignored until lifted
            tracker: FrameTracker::after_reset(),
            poll_callback: None,
            quiet_until: None,
            last_frame: NO_FRAME,
            last_frame_touched: false,
            last_read: None,
            read_failures: 0,
            resets: 0,
        }
    }
}

impl HwTouchState {
    pub(crate) fn enable(&mut self) {
        if self.enabled {
            return;
        }
        self.reset();
        self.enabled = true;
    }

    pub(crate) fn disable(&mut self) {
        if !self.enabled {
            return;
        }
        if let Err(e) = self.ft3269.set_power_mode(PowerMode::Hibernate) {
            log::error!("Error setting touch module state to Hibernate: {e:?}");
        }
        self.enabled = false;
        // The fingers that are down now won't be reported as lifted. The gui-server cancels them for the apps.
        self.tracker = FrameTracker::after_reset();
        if let Some(callback) = &self.poll_callback {
            callback.cancel(TouchPollTimerCallback::ID);
        }
    }

    /// Resets the controller. This is also the only way to wake it up from hibernation.
    fn reset(&mut self) {
        self.gpio_api.set_pin(GpioPin::CtpRstB, false).ok();
        std::thread::sleep(Duration::from_millis(5));
        self.gpio_api.set_pin(GpioPin::CtpRstB, true).ok();

        self.tracker = FrameTracker::after_reset();
        self.last_frame = NO_FRAME;
        self.last_frame_touched = false;
        self.last_read = None;
        self.read_failures = 0;
        self.start_quiet_time();
    }

    /// Ignores interrupts for a while, then reads the controller to see which fingers are touching.
    fn start_quiet_time(&mut self) {
        self.quiet_until = Some(Instant::now() + RESET_QUIET_TIME);
        if let Some(callback) = &self.poll_callback {
            callback.request(RESET_QUIET_TIME.as_millis() as usize, TouchPollTimerCallback::ID, 0);
        }
    }

    /// Reads a frame. Returns `Ok(None)` when the controller reports the same scan again, and `Err` for a
    /// failed read or a frame that can't be trusted.
    fn read_frame(&mut self) -> Result<Option<Frame>, ()> {
        let result = self.ft3269.read_frame().map_err(|e| log::debug!("Could not read touch frame: {e:?}"));
        let frame = result.and_then(|data| {
            // A real lift is reported with the lifted points, so a frame of only zeroes is a failed read
            if data.iter().all(|&b| b == 0) && self.tracker.has_fingers_down() {
                return Err(());
            }
            // Reading the same scan again (we were slow to handle the interrupts, or the poll came first)
            // tells nothing new, and must not count as another frame for the press debounce.
            if data == self.last_frame {
                return Ok(None);
            }
            let frame = Frame::parse(&data).ok_or_else(|| log::debug!("Skipping an invalid touch frame"))?;
            self.last_frame = data;
            self.last_frame_touched = frame != Frame::default();
            Ok(Some(frame))
        });

        match frame {
            Ok(_) => {
                self.read_failures = 0;
                self.resets = 0;
                let now = Instant::now();
                if self.last_read.is_some_and(|last| now - last > MISSED_SCANS_TIME) && self.tracker.has_fingers_down()
                {
                    self.tracker.frame_lost();
                }
                self.last_read = Some(now);
            }
            Err(()) => {
                self.read_failures += 1;
                if self.read_failures == 1 {
                    log::warn!("Could not read a valid touch frame");
                }
            }
        }
        frame
    }
}

impl Gui {
    pub(crate) fn init_touch(&mut self, context: &mut ServerContext<Gui>) {
        let hw_state = &mut self.touch_state.hw_state;
        hw_state.poll_callback = Some(TicktimerCallback::new(context.sid()).expect("Could not connect to ticktimer"));
        // The bootloader reset the controller. Find out which fingers are touching now.
        hw_state.start_quiet_time();
    }

    pub(crate) fn handle_touch_irq(&mut self) {
        let hw_state = &mut self.touch_state.hw_state;
        if !hw_state.enabled {
            log::debug!("Spurious touch IRQ");
            return;
        }
        if let Some(quiet_until) = hw_state.quiet_until {
            let now = Instant::now();
            if now < quiet_until {
                // Make sure we read the controller at the end of the quiet time, even if a timer fired early
                if let Some(callback) = &hw_state.poll_callback {
                    let wait = (quiet_until - now).as_millis() as usize + 1;
                    callback.request(wait, TouchPollTimerCallback::ID, 0);
                }
                return;
            }
            hw_state.quiet_until = None;
        }

        let frame = hw_state.read_frame();
        if hw_state.read_failures >= MAX_READ_FAILURES {
            hw_state.resets += 1;
            if hw_state.resets <= MAX_RESETS {
                log::error!("The touch controller doesn't respond, resetting it");
                hw_state.reset();
            } else {
                // Don't poll a broken controller forever: forget the fingers, and try again on the next
                // interrupt
                log::error!("The touch controller doesn't respond, waiting for an interrupt");
                hw_state.tracker = FrameTracker::default();
                hw_state.read_failures = 0;
                hw_state.resets = 0;
                if let Some(callback) = &hw_state.poll_callback {
                    callback.cancel(TouchPollTimerCallback::ID);
                }
            }
            self.touch_cancel_all();
            self.touch_state.fingers_down = 0;
            return;
        }

        let mut touches = Vec::with_capacity(ft3269::NUM_EVENT_IDS);
        match frame {
            Ok(Some(frame)) => hw_state.tracker.update(&frame, |event| {
                if event.kind == TouchEventKind::Press {
                    log::trace!("Touch {} pressed with weight {} and area {}", event.id, event.weight, event.area);
                }
                // Releases are reported where the finger was last seen, since the controller reports wrong
                // coordinates for lifts within the virtual button area.
                let kind = match event.kind {
                    TouchEventKind::Press => Some(TouchKind::Press),
                    TouchEventKind::Drag => Some(TouchKind::Drag),
                    TouchEventKind::Release => Some(TouchKind::Release),
                    TouchEventKind::Cancel => None,
                };
                touches.push((kind, Touch {
                    kind: kind.unwrap_or(TouchKind::Release),
                    id: event.id as usize,
                    x: event.x as usize,
                    y: event.y as usize,
                }));
            }),
            Ok(None) => {}
            // Skipping a frame is safe: the next one tells which fingers are down
            Err(()) => hw_state.tracker.frame_lost(),
        }

        // Make sure we read the controller again while a finger is down, or an app waits for a cancel.
        // Every interrupt pushes this back, and once no finger is down the next read stops it.
        let touched = hw_state.tracker.is_touched();
        if let Some(callback) = &hw_state.poll_callback {
            if touched || !self.touch_state.touch_resync.is_empty() {
                callback.request(TOUCH_POLL_INTERVAL_MS, TouchPollTimerCallback::ID, 0);
            }
        }

        let hw_state = &self.touch_state.hw_state;
        if touches.is_empty() && frame.is_ok() && hw_state.last_frame_touched && hw_state.tracker.has_fingers_down() {
            // A finger resting on the screen keeps it awake, like a moving one does
            self.touch_keep_awake();
        }
        for (kind, touch) in touches {
            match kind {
                Some(_) => self.touch_dispatch(touch, false),
                None => self.touch_cancel_finger(touch.id),
            }
        }
    }
}

impl ScalarHandler<TouchPollTimerCallback> for Gui {
    fn handle(&mut self, _msg: TouchPollTimerCallback, _sender: xous::PID, _context: &mut ServerContext<Self>) {
        self.touch_resync();
        if self.touch_state.hw_state.enabled {
            self.handle_touch_irq();
        }
    }
}
