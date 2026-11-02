// SPDX-FileCopyrightText: 2024 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(keyos)]
mod atsama5d2;

use gui_server_api::consts::{
    KEYBOARD_TOP_BAR_MARGIN, VIRT_BUTTON_PHYS_HEIGHT, VIRT_BUTTON_PHYS_ORIGIN_X, VIRT_BUTTON_PHYS_ORIGIN_Y,
    VIRT_BUTTON_PHYS_WIDTH,
};
use {
    crate::{control_center::ControlCenterWindowState, switcher::SwitcherGestureState, Gui},
    gui_server_api::{
        consts::{SCREEN_HEIGHT, SCREEN_WIDTH},
        touch::{Touch, TouchKind},
        InputMessage,
    },
};

/// Most fingers that touch the screen at once on purpose. More are taken to be a palm or a grip.
const MAX_INTENTIONAL_TOUCHES: u32 = 3;

/// Width of the strips along the left and right edges (about 1.7 mm) where fingers other than the first
/// one are ignored.
const SECONDARY_TOUCH_EDGE_MARGIN: usize = 16;

pub(crate) struct TouchState {
    pub(crate) origin: TouchGestureOrigin,
    pub(crate) switcher_gesture_state: SwitcherGestureState,
    pub(crate) offset: i32,

    #[cfg(keyos)]
    pub(crate) hw_state: crate::touch::atsama5d2::HwTouchState,
    last_x: usize,
    last_y: usize,

    /// The app that the finger with id `n` (n > 0) is being forwarded to, and its last position.
    secondary_touches: [(Option<xous::PID>, usize, usize); 16],

    /// The app that the first finger is being forwarded to.
    primary_target: Option<xous::PID>,
    /// Apps that missed a press or release because their input queue was full, and how many times we
    /// tried since. They are sent a `TouchCancel` as soon as their queue has room.
    pub(crate) touch_resync: Vec<(xous::PID, u32)>,

    /// Bit `n` is set while the finger with id `n` is down.
    pub(crate) fingers_down: u16,
    /// All fingers are ignored until they are lifted, see [`Gui::touch_ignore_sequence`].
    ignore_sequence: bool,
}

impl TouchState {
    pub fn init() -> Self {
        TouchState {
            origin: TouchGestureOrigin::None,
            // See SFT-5550
            // This initial value is a fallback, it will be controlled by system settings,
            // and updated by the gui-server's subscription to the TouchOffset setting
            offset: -30,
            #[cfg(keyos)]
            hw_state: Default::default(),
            switcher_gesture_state: SwitcherGestureState::default(),
            last_x: 0,
            last_y: 0,
            secondary_touches: [(None, 0, 0); 16],
            primary_target: None,
            touch_resync: Vec::new(),
            fingers_down: 0,
            ignore_sequence: false,
        }
    }
}

/// Allows to track where the touch gesture started and dispatch further touch events
/// there instead of whatever area the touch events happen over after that.
/// E.g. if user touches the Control Center and then drags it down, all touch events shall be
/// processed by the Control Center app.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum TouchGestureOrigin {
    None,
    ControlCenter,
    App,
    Keyboard,
    VirtualButton,
    Modal,
    AppSwitcherGesture,
}

impl Gui {
    /// Sends incoming touch event to the currently active app, Control Center or the
    /// keyboard depending on the touch coordinates and the current state.
    ///
    /// When `injected` is true (debug bridge), the auto-lock dimming is still reset
    /// but the touch is **not** swallowed — so a single tap is enough to interact
    /// with the device even when the screen has dimmed.
    pub fn touch_dispatch(&mut self, touch: Touch, injected: bool) {
        self.touch_resync();
        if self.touch_ignore_sequence(&touch, injected) {
            return;
        }

        // Only the first finger drives system gestures, since the code below assumes a certain
        // Press->drag->release order. Further fingers go to the app that the first finger is in.
        if touch.id > 0 {
            self.secondary_touch_dispatch(touch, injected);
            return;
        }

        if self.is_control_center_animating() {
            // An app that got the press of this finger must not wait for its release forever
            if touch.kind != TouchKind::Press && self.touch_state.origin == TouchGestureOrigin::App {
                self.touch_cancel_all();
            }
            return;
        }

        let is_within_home_button = touch.is_within_area(
            VIRT_BUTTON_PHYS_ORIGIN_X,
            VIRT_BUTTON_PHYS_ORIGIN_Y,
            VIRT_BUTTON_PHYS_WIDTH,
            VIRT_BUTTON_PHYS_HEIGHT,
        );

        #[cfg(feature = "recovery-os")]
        let is_locked = true;
        #[cfg(not(feature = "recovery-os"))]
        let is_locked = self.is_locked();

        if touch.kind == TouchKind::Press {
            let keyboard_height =
                self.with_active_app_mut(|w| w.keyboard_state.height().unwrap_or(0)).unwrap_or(0)
                    - KEYBOARD_TOP_BAR_MARGIN;
            let is_within_keyboard =
                touch.is_within_area(0, SCREEN_HEIGHT - keyboard_height, SCREEN_WIDTH, keyboard_height);
            self.touch_state.origin = if is_within_home_button {
                if is_locked {
                    // Redirect touches to the lock screen app if the home button is used while locked
                    TouchGestureOrigin::App
                } else {
                    TouchGestureOrigin::VirtualButton
                }
            } else if !touch.is_within_area(0, 0, SCREEN_WIDTH, SCREEN_HEIGHT) {
                TouchGestureOrigin::None
            } else if self.is_touch_within_control_center(touch) {
                TouchGestureOrigin::ControlCenter
            } else if is_within_keyboard {
                TouchGestureOrigin::Keyboard
            } else if self.is_modal_active() {
                TouchGestureOrigin::Modal
            } else if self.touch_state.switcher_gesture_state.started {
                TouchGestureOrigin::AppSwitcherGesture
            } else if self
                .control_center_window
                .as_ref()
                .map(|w| w.state == ControlCenterWindowState::Expanded)
                .unwrap_or(false)
            {
                TouchGestureOrigin::None
            } else {
                TouchGestureOrigin::App
            };
        }

        if !self.is_control_center_collapsed()
            && !self.is_control_center_animating()
            && self.touch_state.origin != TouchGestureOrigin::ControlCenter
            && matches!(touch.kind, TouchKind::Drag | TouchKind::Press)
        {
            let is_expanded = self
                .control_center_window
                .as_ref()
                .map(|w| w.state == ControlCenterWindowState::Expanded)
                .unwrap_or(false);

            let should_collapse = if is_expanded {
                touch.kind == TouchKind::Press || touch.y < self.touch_state.last_y
            } else {
                false
            };

            if should_collapse {
                self.control_center_collapse();
                // Swallow the gesture, like above except in case of the virtual button,
                // which is a more deliberate action.
                if self.touch_state.origin != TouchGestureOrigin::VirtualButton {
                    self.touch_state.origin = TouchGestureOrigin::None;
                    return;
                }
            }
        }

        // Filter out redundant drag events that are continuously sent by the chip
        if touch.kind == TouchKind::Drag
            && self.touch_state.last_x == touch.x
            && self.touch_state.last_y == touch.y
        {
            return;
        }

        // Allow the swipe up gesture to be uninterrupted by the Release that's generated when
        // crossing a gap between the home button and the screen
        if is_locked && touch.kind == TouchKind::Release && is_within_home_button {
            return;
        }

        self.touch_state.last_x = touch.x;
        self.touch_state.last_y = touch.y;

        // Apply touch offset once for all touch origins.
        // Skip for injected (debug bridge) touches — their coordinates are already
        // in screen-visual space and don't need hardware calibration.
        // Simulator is touch-perfect and has a locked default offset of 0.
        let offset = if injected { 0 } else { self.touch_state.offset };
        let touch = touch.with_offset(0, offset);

        match self.touch_state.origin {
            TouchGestureOrigin::ControlCenter => {
                self.control_center_process_touch(touch);
            }
            TouchGestureOrigin::Keyboard => {
                if let Some(kw) = &self.keyboard_window {
                    let h = self.with_active_app(|w| w.keyboard_state.height().unwrap_or(0)).unwrap_or(0);
                    let touch = touch.translate_pos(0, SCREEN_HEIGHT - h);
                    log::trace!("Touching keyboard: {touch:?}");
                    xous::try_send_message(
                        kw.input_cid,
                        touch.as_input_message(InputMessage::Touch as usize),
                    )
                    .ok();
                }
            }
            TouchGestureOrigin::App => {
                let mut touch = touch;
                // Shift touch to the lockscreen app if it comes from the home button (unlock swipe)
                if is_locked && is_within_home_button {
                    touch = touch.translate_pos(
                        0,
                        VIRT_BUTTON_PHYS_ORIGIN_Y + VIRT_BUTTON_PHYS_HEIGHT - SCREEN_HEIGHT,
                    );
                }

                // The finger stays with the app it pressed in. An app that shows up in the meantime (a modal,
                // a switch) must not get the rest of the touch, or it would see a release it never saw
                // pressed and could activate whatever is under the finger.
                let active_pid = self.active_app_pid();
                if touch.kind == TouchKind::Press {
                    self.touch_state.primary_target = active_pid;
                }
                if let Some(target) = self.touch_state.primary_target {
                    if active_pid == Some(target) {
                        self.send_app_touch(target, touch);
                    } else {
                        self.send_touch_cancel(target);
                        self.touch_state.primary_target = None;
                    }
                }
            }

            TouchGestureOrigin::VirtualButton => {
                self.virtbutton_process_touch(touch);
            }

            TouchGestureOrigin::Modal => {
                self.modal_process_touch(touch);
            }

            TouchGestureOrigin::AppSwitcherGesture => {
                crate::switcher::process_touch(self, touch);
            }

            TouchGestureOrigin::None => {}
        }

        if touch.kind == TouchKind::Release {
            if let Some(w) = &mut self.control_center_window {
                if matches!(w.state, crate::control_center::ControlCenterWindowState::Dragged) {
                    self.control_center_expand();
                }
            }

            self.touch_state.origin = TouchGestureOrigin::None;
            self.touch_state.primary_target = None;
        }
    }

    /// Takes a finger whose lift was missed away, without activating anything where it was last seen.
    pub(crate) fn touch_cancel_finger(&mut self, id: usize) {
        let state = &mut self.touch_state;
        state.fingers_down &= !1u16.checked_shl(id as u32).unwrap_or(0);
        if state.ignore_sequence {
            return;
        }

        if id == 0 {
            if let Some(target) = state.primary_target.take() {
                self.send_touch_cancel(target);
            }
            self.touch_end_system_gesture();
            self.touch_state.origin = TouchGestureOrigin::None;
        } else if let Some(slot) = state.secondary_touches.get_mut(id) {
            if let Some(target) = slot.0.take() {
                self.send_touch_cancel(target);
            }
        }
    }

    /// Sends a touch to an app, and remembers to resynchronize it if it missed a press or release.
    fn send_app_touch(&mut self, pid: xous::PID, touch: Touch) {
        let Some(window) = self.windows.get(&pid) else { return };
        let sent =
            xous::try_send_message(window.input_cid, touch.as_input_message(InputMessage::Touch as usize)).is_ok();
        // A missed drag is made up for by the next one
        if !sent && touch.kind != TouchKind::Drag && window.multitouch {
            log::warn!("{pid} missed a touch, cancelling its touches");
            self.touch_resync_later(pid);
        }
    }

    /// Tells an app that the fingers touching it were taken away, if it handles that.
    fn send_touch_cancel(&mut self, pid: xous::PID) {
        // Apps that don't handle cancellation just don't hear about the fingers again
        let Some(window) = self.windows.get(&pid).filter(|w| w.multitouch) else { return };
        let msg = xous::Message::new_scalar(InputMessage::TouchCancel as usize, 0, 0, 0, 0);
        if xous::try_send_message(window.input_cid, msg).is_err() {
            self.touch_resync_later(pid);
        }
    }

    fn touch_resync_later(&mut self, pid: xous::PID) {
        if !self.touch_state.touch_resync.iter().any(|&(p, _)| p == pid) {
            self.touch_state.touch_resync.push((pid, 0));
        }
    }

    /// Sends the cancellations that didn't fit into the apps' input queues before. Gives up on an app
    /// that doesn't read its input for a while: a press it gets later cancels what it missed anyway.
    pub(crate) fn touch_resync(&mut self) {
        const MAX_RESYNC_ATTEMPTS: u32 = 40;

        let windows = &self.windows;
        self.touch_state.touch_resync.retain_mut(|(pid, attempts)| {
            let Some(window) = windows.get(pid) else { return false };
            let msg = xous::Message::new_scalar(InputMessage::TouchCancel as usize, 0, 0, 0, 0);
            *attempts += 1;
            xous::try_send_message(window.input_cid, msg).is_err() && *attempts < MAX_RESYNC_ATTEMPTS
        });
    }

    /// Decides whether to ignore the touch sequence (from the first finger landing to the last one being
    /// lifted) that `touch` belongs to, and returns whether to ignore `touch`. Fingers of an ignored
    /// sequence that apps already got are cancelled for them.
    ///
    /// A sequence is ignored when it woke the screen up from being dimmed, since the user couldn't see
    /// what they touched, and when more fingers touch at once than anyone would use on purpose on this
    /// screen, since that is a palm or a hand gripping the device.
    fn touch_ignore_sequence(&mut self, touch: &Touch, injected: bool) -> bool {
        let state = &mut self.touch_state;
        let starts_sequence = touch.kind == TouchKind::Press && state.fingers_down == 0;
        let bit = 1u16.checked_shl(touch.id as u32).unwrap_or(0);
        match touch.kind {
            TouchKind::Press => state.fingers_down |= bit,
            TouchKind::Drag => {}
            TouchKind::Release => state.fingers_down &= !bit,
        }
        if starts_sequence {
            state.ignore_sequence = false;
        }

        // Injected (debug bridge) touches still undim, but are never ignored for it
        if self.reset_auto_lock() && !injected && !self.touch_state.ignore_sequence {
            if !starts_sequence {
                // The screen dimmed while fingers were resting on it
                self.touch_cancel_all();
            }
            self.touch_state.ignore_sequence = true;
        }

        if !self.touch_state.ignore_sequence && self.touch_state.fingers_down.count_ones() > MAX_INTENTIONAL_TOUCHES {
            log::info!("Too many fingers on the touch panel, ignoring it until they are lifted");
            self.touch_cancel_all();
            self.touch_state.ignore_sequence = true;
        }

        self.touch_state.ignore_sequence
    }

    /// Keeps the screen awake while fingers rest on it without moving.
    #[cfg_attr(not(keyos), allow(dead_code))]
    pub(crate) fn touch_keep_awake(&mut self) {
        if self.reset_auto_lock() && !self.touch_state.ignore_sequence {
            // The screen dimmed anyway, so the user may not have seen what the fingers touch
            self.touch_cancel_all();
            self.touch_state.ignore_sequence = true;
        }
    }

    /// Forwards a touch of a finger other than the first one, so that apps can
    /// recognize multi-finger gestures. A finger is forwarded only if it lands while the first
    /// finger is in an unlocked app, and then to that same app until it is lifted, so that the app
    /// always sees the finger lifted again.
    fn secondary_touch_dispatch(&mut self, touch: Touch, injected: bool) {
        let Some(&(target, last_x, last_y)) = self.touch_state.secondary_touches.get(touch.id) else {
            return;
        };

        #[cfg(feature = "recovery-os")]
        let is_locked = true;
        #[cfg(not(feature = "recovery-os"))]
        let is_locked = self.is_locked();

        let target = match touch.kind {
            TouchKind::Press => {
                self.touch_state.secondary_touches[touch.id].0 = None;
                // Only into the app that the first finger is in, and is still active
                let Some(pid) = self.touch_state.primary_target.filter(|&pid| self.active_app_pid() == Some(pid))
                else {
                    return;
                };
                if is_locked {
                    return;
                }
                // A further finger that lands on the side edges is likely the hand holding the device,
                // one below the screen is on the virtual button, and ones on the keyboard or the
                // Control Center are not in the app either.
                let keyboard_height = self
                    .with_active_app(|w| w.keyboard_state.height().unwrap_or(0))
                    .unwrap_or(0)
                    .saturating_sub(KEYBOARD_TOP_BAR_MARGIN);
                if touch.x < SECONDARY_TOUCH_EDGE_MARGIN
                    || touch.x >= SCREEN_WIDTH - SECONDARY_TOUCH_EDGE_MARGIN
                    || touch.y >= SCREEN_HEIGHT - keyboard_height
                    || self.is_touch_within_control_center(touch)
                {
                    return;
                }
                // Apps that don't handle several fingers would take them for the first one
                if !self.windows.get(&pid).is_some_and(|w| w.multitouch) {
                    return;
                }
                pid
            }
            // Same filtering of redundant drag events as for the first finger
            TouchKind::Drag if (last_x, last_y) == (touch.x, touch.y) => return,
            TouchKind::Drag | TouchKind::Release => {
                let Some(pid) = target else { return };
                pid
            }
        };
        self.touch_state.secondary_touches[touch.id] =
            (if touch.kind == TouchKind::Release { None } else { Some(target) }, touch.x, touch.y);

        let offset = if injected { 0 } else { self.touch_state.offset };
        self.send_app_touch(target, touch.with_offset(0, offset));
    }

    pub(crate) fn touch_on(&mut self) {
        #[cfg(keyos)]
        self.touch_state.hw_state.enable();
    }

    pub(crate) fn touch_off(&mut self) {
        #[cfg(keyos)]
        self.touch_state.hw_state.disable();

        // The fingers that are down now won't be reported as lifted
        self.touch_cancel_all();
        self.touch_state.fingers_down = 0;
        self.touch_state.ignore_sequence = false;
    }

    /// Ends a system gesture of the first finger as if it was lifted.
    fn touch_end_system_gesture(&mut self) {
        if self.touch_state.origin == TouchGestureOrigin::ControlCenter {
            if let Some(w) = &self.control_center_window {
                if matches!(w.state, ControlCenterWindowState::Dragged) {
                    self.control_center_expand();
                }
            }
        }
        // The swipe up from the virtual button that opens the switcher
        if self.touch_state.switcher_gesture_state.started {
            self.touch_state.switcher_gesture_state.reset();
            self.abort_switching_animation();
        }
    }

    /// Tells every app that fingers are touching that they were taken away, and forgets them. A system
    /// gesture in progress ends as if the finger was lifted.
    pub(crate) fn touch_cancel_all(&mut self) {
        self.touch_end_system_gesture();

        let mut apps: Vec<xous::PID> =
            self.touch_state.secondary_touches.iter().filter_map(|&(target, _, _)| target).collect();
        apps.extend(self.touch_state.primary_target.take());
        apps.sort_unstable_by_key(|pid| pid.get());
        apps.dedup();
        for pid in apps {
            self.send_touch_cancel(pid);
        }

        self.touch_state.secondary_touches = [(None, 0, 0); 16];
        self.touch_state.origin = TouchGestureOrigin::None;
    }
}
