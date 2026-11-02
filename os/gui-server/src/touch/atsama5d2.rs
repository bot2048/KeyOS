// SPDX-FileCopyrightText: 2024 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

use ft3269::{Ft3269, FrameTracker, PowerMode, TouchEventKind};
use gpio::{GpioApi, GpioPin};
use gui_server_api::touch::{Touch, TouchKind};
use i2c::Peripheral;

use crate::{gpio::GpioPermissions, Gui};

i2c::use_api!();

pub(crate) struct HwTouchState {
    ft3269: ft3269::Ft3269<I2cPeripheral>,
    gpio_api: GpioApi<GpioPermissions>,
    enabled: bool,
    tracker: FrameTracker,
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
            tracker: FrameTracker::default(),
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
        self.tracker = FrameTracker::default();
    }
}

impl Gui {
    pub(crate) fn handle_touch_irq(&mut self) {
        if !self.touch_state.hw_state.enabled {
            log::debug!("Spurious touch IRQ");
            return;
        }

        let hw_state = &mut self.touch_state.hw_state;
        let frame = match hw_state.ft3269.frame() {
            Ok(Some(frame)) => frame,
            // Skipping a frame is safe: the next one tells which fingers are down
            Ok(None) => {
                log::debug!("Skipping an invalid touch frame");
                return;
            }
            Err(e) => {
                log::warn!("Could not read touch frame: {e:?}");
                return;
            }
        };

        // Releases are reported where the finger was last seen, since the controller reports wrong
        // coordinates for lifts within the virtual button area.
        let mut touches = Vec::with_capacity(ft3269::NUM_EVENT_IDS);
        hw_state.tracker.update(&frame, |event| {
            if event.kind == TouchEventKind::Press {
                log::trace!("Touch {} pressed with weight {} and area {}", event.id, event.weight, event.area);
            }
            let kind = match event.kind {
                TouchEventKind::Press => TouchKind::Press,
                TouchEventKind::Drag => TouchKind::Drag,
                TouchEventKind::Release => TouchKind::Release,
            };
            touches.push(Touch { kind, id: event.id as usize, x: event.x as usize, y: event.y as usize });
        });

        for touch in touches {
            self.touch_dispatch(touch, false);
        }
    }
}
