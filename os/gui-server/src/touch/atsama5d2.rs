// SPDX-FileCopyrightText: 2024 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use ft3269::{Ft3269, FrameTracker, PowerMode, TouchEventKind};
use gpio::{GpioApi, GpioPin};
use gui_server_api::touch::{Touch, TouchKind};
use i2c::Peripheral;
use server::{MessageId as _, ScalarHandler, ServerContext};
use xous_ticktimer::TicktimerCallback;

use crate::{gpio::GpioPermissions, handlers::TouchPollTimerCallback, Gui};

/// While a finger is down, the controller is read again if it doesn't interrupt for this long, so that a
/// missed lift interrupt doesn't leave the finger down.
const TOUCH_POLL_INTERVAL_MS: usize = 50;

i2c::use_api!();

pub(crate) struct HwTouchState {
    ft3269: ft3269::Ft3269<I2cPeripheral>,
    gpio_api: GpioApi<GpioPermissions>,
    enabled: bool,
    tracker: FrameTracker,
    poll_callback: Option<TicktimerCallback>,
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
        }
    }
}
impl HwTouchState {
    pub(crate) fn enable(&mut self) {
        if self.enabled {
            return;
        }
        // We can only wake up from hibernate state with a hard reset to the controller.
        self.gpio_api.set_pin(GpioPin::CtpRstB, false).ok();
        std::thread::sleep(Duration::from_millis(5));
        self.gpio_api.set_pin(GpioPin::CtpRstB, true).ok();
        self.enabled = true;
        self.tracker = FrameTracker::after_reset();
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
}

impl Gui {
    pub(crate) fn init_touch(&mut self, context: &mut ServerContext<Gui>) {
        self.touch_state.hw_state.poll_callback =
            Some(TicktimerCallback::new(context.sid()).expect("Could not connect to ticktimer"));
    }

    pub(crate) fn handle_touch_irq(&mut self) {
        if !self.touch_state.hw_state.enabled {
            log::debug!("Spurious touch IRQ");
            return;
        }

        let hw_state = &mut self.touch_state.hw_state;
        let frame = match hw_state.ft3269.frame() {
            Ok(Some(frame)) => Some(frame),
            Ok(None) => {
                log::debug!("Skipping an invalid touch frame");
                None
            }
            Err(e) => {
                log::warn!("Could not read touch frame: {e:?}");
                None
            }
        };

        // Make sure we read the controller again while a finger is down. Every interrupt pushes this back,
        // and once no finger is down the next read stops it.
        if let Some(callback) = &hw_state.poll_callback {
            if hw_state.tracker.is_touched() {
                callback.request(TOUCH_POLL_INTERVAL_MS, TouchPollTimerCallback::ID, 0);
            }
        }

        // Skipping a frame is safe: the next one tells which fingers are down
        let Some(frame) = frame else {
            hw_state.tracker.frame_lost();
            return;
        };

        // Releases are reported where the finger was last seen, since the controller reports wrong
        // coordinates for lifts within the virtual button area.
        let mut touches = Vec::with_capacity(ft3269::NUM_EVENT_IDS);
        hw_state.tracker.update(&frame, |event| {
            if event.kind == TouchEventKind::Press {
                log::trace!("Touch {} pressed with weight {} and area {}", event.id, event.weight, event.area);
            }
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
        });

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
        if self.touch_state.hw_state.enabled {
            self.handle_touch_irq();
        }
    }
}
