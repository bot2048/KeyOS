// SPDX-FileCopyrightText: 2026 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Turns the fingers touching an app into Slint pointer and gesture events.
//!
//! One finger is the pointer. As soon as a second finger lands, the pointer press is cancelled rather
//! than released, and no press or click happens until all fingers are lifted, so that multi-finger
//! touches never activate a button by accident. Two fingers make pinch and rotation gestures, which
//! `ScaleRotateGestureHandler` turns into a scale and a rotation.
//!
//! A finger that rested on a side edge for a while is likely the hand holding the device. If it is the
//! pointer when another finger lands, it is dropped instead, and the other finger becomes the pointer, or
//! else every tap would count as a second finger while the hand holds the device.
//!
//! When we lose track of fingers (a press of a finger that is already down, or a cancel from the
//! gui-server), everything is cancelled, and nothing is pressed until a new first finger (id 0) lands.

use std::time::{Duration, Instant};

use i_slint_core::{
    graphics::euclid,
    input::{MouseEvent, TouchPhase},
    lengths::LogicalPoint,
    platform::PointerEventButton,
};

/// Distance change (in logical pixels, about 2.5 mm on Passport Prime) before two fingers make a pinch.
const PINCH_SLOP: f32 = 24.0;

/// Angle change (in degrees) before two fingers make a rotation.
const ROTATION_SLOP_DEGREES: f32 = 10.0;

/// Distance between two fingers (about 5 mm) below which their distance and angle are too noisy to use.
const MIN_PINCH_SPAN: f32 = 48.0;

/// Width of the strips along the left and right edges (about 1.7 mm) where a resting finger can be a grip.
const GRIP_EDGE_MARGIN: f32 = 16.0;

/// A finger that moved further than this (in logical pixels) since it landed is not resting.
const GRIP_SLOP: f32 = 24.0;

/// How long a finger rests on a side edge before it can be taken for the hand holding the device.
const GRIP_MIN_REST: Duration = Duration::from_millis(300);

/// Slint holds a press inside a `Flickable` back for 100 ms, to tell taps from scrolls, and ignores an
/// exit that comes before the held press is delivered. The exit that cancels a press is sent again after
/// this long, so that such a press doesn't stay pressed.
const LATE_EXIT_DELAY: Duration = Duration::from_millis(150);

pub(crate) struct TouchRecognizer {
    /// Width of the window in logical pixels, to tell where its side edges are.
    width: f32,
    fingers: Vec<Finger>,
    mode: Mode,
    /// A finger moved since the last pointer or gesture event.
    moved: bool,
    /// We lost track of the fingers: ignore all until a new first finger lands.
    wait_for_first_finger: bool,
    /// When to send the exit that cancelled the last press again.
    late_exit: Option<Instant>,
}

#[derive(Clone, Copy)]
struct Finger {
    id: i32,
    position: LogicalPoint,
    start: LogicalPoint,
    landed: Instant,
    /// Taken to be the hand holding the device, and left out of everything until lifted.
    grip: bool,
}

impl Finger {
    fn is_resting_on_side_edge(&self, width: f32, now: Instant) -> bool {
        (self.start.x < GRIP_EDGE_MARGIN || self.start.x >= width - GRIP_EDGE_MARGIN)
            && (self.position - self.start).length() <= GRIP_SLOP
            && now.saturating_duration_since(self.landed) >= GRIP_MIN_REST
    }
}

#[derive(Default)]
enum Mode {
    #[default]
    Idle,
    /// A single finger, forwarded as the pointer.
    Pointer { id: i32 },
    /// More than one finger has been down since all were last lifted.
    Multi(Gesture),
}

enum Gesture {
    None,
    /// Two fingers are down but haven't moved enough to make a gesture yet.
    Pending { ids: (i32, i32), distance: f32, angle: Option<euclid::Angle<f32>> },
    Active { ids: (i32, i32), initial_distance: f32, last_scale: f32, last_angle: Option<euclid::Angle<f32>> },
}

impl TouchRecognizer {
    pub(crate) fn new(width: f32) -> Self {
        Self {
            width,
            fingers: Vec::new(),
            mode: Mode::Idle,
            moved: false,
            wait_for_first_finger: false,
            late_exit: None,
        }
    }

    pub(crate) fn set_width(&mut self, width: f32) { self.width = width; }

    /// Processes a touch of a finger. Moves are only applied with the next [`Self::flush`] or touch of
    /// another kind, so that a batch of moves results in a single pointer or gesture update.
    pub(crate) fn touch(
        &mut self,
        id: i32,
        position: LogicalPoint,
        phase: TouchPhase,
        now: Instant,
        out: &mut impl FnMut(MouseEvent),
    ) {
        match phase {
            TouchPhase::Started => {
                self.flush(out);
                self.started(id, position, now, out);
            }
            TouchPhase::Moved => {
                if let Some(finger) = self.fingers.iter_mut().find(|f| f.id == id) {
                    if finger.position != position {
                        finger.position = position;
                        self.moved = true;
                    }
                }
            }
            TouchPhase::Ended => {
                if let Some(finger) = self.fingers.iter_mut().find(|f| f.id == id) {
                    finger.position = position;
                }
                self.flush(out);
                self.ended(id, now, out);
            }
            TouchPhase::Cancelled => self.cancel(now, out),
        }
    }

    /// Emits the pointer move or gesture update for the moves processed so far.
    pub(crate) fn flush(&mut self, out: &mut impl FnMut(MouseEvent)) {
        if !std::mem::take(&mut self.moved) {
            return;
        }
        match self.mode {
            Mode::Idle => {}
            Mode::Pointer { id } => {
                if let Some(finger) = self.finger(id) {
                    out(MouseEvent::Moved { position: finger.position, touch_finger_id: id + 1 });
                }
            }
            Mode::Multi(_) => self.update_gesture(out),
        }
    }

    /// Takes all fingers away without activating anything, and ignores them until a new first finger lands.
    pub(crate) fn cancel(&mut self, now: Instant, out: &mut impl FnMut(MouseEvent)) {
        if let Mode::Multi(Gesture::Active { ids, .. }) = self.mode {
            let position = self.midpoint(ids).unwrap_or_default();
            out(MouseEvent::PinchGesture { position, delta: 0.0, phase: TouchPhase::Cancelled });
            out(MouseEvent::RotationGesture { position, delta: 0.0, phase: TouchPhase::Cancelled });
        }
        // Even when no finger is known: Slint may hold a press we don't know about, and exiting is harmless
        self.exit(now, out);
        self.fingers.clear();
        self.mode = Mode::Idle;
        self.moved = false;
        self.wait_for_first_finger = true;
    }

    /// The first finger of a new touch sequence (id 0) landed, even if we don't get its touches because
    /// the swipe back gesture took it.
    pub(crate) fn first_finger_landed(&mut self) { self.wait_for_first_finger = false; }

    /// Sends the exit that cancelled the last press again once it is due. Returns when to call again.
    ///
    /// An active gesture is not disturbed by it: `ScaleRotateGestureHandler` keeps the grab on an exit.
    pub(crate) fn poll(&mut self, now: Instant, out: &mut impl FnMut(MouseEvent)) -> Option<Instant> {
        let due = self.late_exit?;
        if now < due {
            return Some(due);
        }
        self.late_exit = None;
        out(MouseEvent::Exit);
        None
    }

    fn started(&mut self, id: i32, position: LogicalPoint, now: Instant, out: &mut impl FnMut(MouseEvent)) {
        if self.fingers.iter().any(|f| f.id == id) {
            // We missed this finger being lifted, so we don't know what happened in between
            self.cancel(now, out);
        }
        if id == 0 {
            self.wait_for_first_finger = false;
        }
        if self.wait_for_first_finger {
            return;
        }
        self.fingers.push(Finger { id, position, start: position, landed: now, grip: false });

        match self.mode {
            Mode::Pointer { id: pointer_id }
                if self.finger(pointer_id).is_some_and(|f| f.is_resting_on_side_edge(self.width, now)) =>
            {
                // The pointer is the hand holding the device: drop it without a click, and take the new
                // finger as the pointer
                if let Some(grip) = self.fingers.iter_mut().find(|f| f.id == pointer_id) {
                    grip.grip = true;
                }
                self.exit(now, out);
                self.press(id, position, now, out);
            }
            Mode::Idle => self.press(id, position, now, out),
            Mode::Pointer { .. } => {
                // Cancel the press, so that it doesn't turn into a click
                self.exit(now, out);
                self.mode = Mode::Multi(self.pending_gesture());
            }
            Mode::Multi(Gesture::None) => self.mode = Mode::Multi(self.pending_gesture()),
            // Further fingers don't take part in the gesture
            Mode::Multi(_) => {}
        }
    }

    fn ended(&mut self, id: i32, now: Instant, out: &mut impl FnMut(MouseEvent)) {
        let Some(index) = self.fingers.iter().position(|f| f.id == id) else { return };
        let finger = self.fingers.remove(index);
        if finger.grip {
            return;
        }

        match self.mode {
            Mode::Idle => {}
            Mode::Pointer { id: pointer_id } => {
                if pointer_id == id {
                    out(MouseEvent::Released {
                        position: finger.position,
                        button: PointerEventButton::Left,
                        click_count: 0,
                        touch_finger_id: id + 1,
                    });
                    out(MouseEvent::Exit);
                    self.mode = Mode::Idle;
                }
            }
            Mode::Multi(Gesture::Pending { ids, .. }) if ids.0 == id || ids.1 == id => {
                self.mode = Mode::Multi(self.pending_gesture());
            }
            Mode::Multi(Gesture::Active { ids, .. }) if ids.0 == id || ids.1 == id => {
                let other = if ids.0 == id { ids.1 } else { ids.0 };
                let position =
                    self.finger(other).map_or(finger.position, |o| finger.position.lerp(o.position, 0.5));
                out(MouseEvent::PinchGesture { position, delta: 0.0, phase: TouchPhase::Ended });
                out(MouseEvent::RotationGesture { position, delta: 0.0, phase: TouchPhase::Ended });
                // Two fingers that are still down can make a new gesture
                self.mode = Mode::Multi(self.pending_gesture());
            }
            Mode::Multi(_) => {}
        }

        if matches!(self.mode, Mode::Multi(_)) && self.fingers.iter().all(|f| f.grip) {
            self.exit(now, out);
            self.mode = Mode::Idle;
        }
    }

    fn press(&mut self, id: i32, position: LogicalPoint, now: Instant, out: &mut impl FnMut(MouseEvent)) {
        self.mode = Mode::Pointer { id };
        // Slint may have delivered a press it held back since the pending exit was sent, which would
        // otherwise get this finger's release
        if self.late_exit.take().is_some_and(|due| due < now + LATE_EXIT_DELAY) {
            out(MouseEvent::Exit);
        }
        out(MouseEvent::Pressed { position, button: PointerEventButton::Left, click_count: 0, touch_finger_id: id + 1 });
    }

    /// Sends an exit that cancels a press, and again later, see [`LATE_EXIT_DELAY`].
    fn exit(&mut self, now: Instant, out: &mut impl FnMut(MouseEvent)) {
        out(MouseEvent::Exit);
        self.late_exit = Some(now + LATE_EXIT_DELAY);
    }

    /// A new gesture between the first two fingers that are down, if there are two.
    fn pending_gesture(&self) -> Gesture {
        let mut fingers = self.fingers.iter().filter(|f| !f.grip);
        let (Some(a), Some(b)) = (fingers.next(), fingers.next()) else { return Gesture::None };
        let ids = (a.id, b.id);
        match geometry(&self.fingers, ids) {
            Some((distance, angle)) => Gesture::Pending { ids, distance, angle },
            None => Gesture::None,
        }
    }

    fn update_gesture(&mut self, out: &mut impl FnMut(MouseEvent)) {
        let Mode::Multi(gesture) = &mut self.mode else { return };
        match *gesture {
            Gesture::None => {}
            Gesture::Pending { ids, distance, angle } => {
                let Some((new_distance, new_angle)) = geometry(&self.fingers, ids) else { return };
                let rotated = match (angle, new_angle) {
                    (Some(angle), Some(new_angle)) => {
                        (new_angle - angle).signed().to_degrees().abs() > ROTATION_SLOP_DEGREES
                    }
                    _ => false,
                };
                if (new_distance - distance).abs() > PINCH_SLOP || rotated {
                    // Start from the current geometry, so that the slop doesn't count as gesture
                    *gesture = Gesture::Active {
                        ids,
                        initial_distance: new_distance,
                        last_scale: 1.0,
                        last_angle: new_angle,
                    };
                    let position = midpoint(&self.fingers, ids).unwrap_or_default();
                    out(MouseEvent::PinchGesture { position, delta: 0.0, phase: TouchPhase::Started });
                    out(MouseEvent::RotationGesture { position, delta: 0.0, phase: TouchPhase::Started });
                } else if angle.is_none() {
                    // Take the angle once the fingers are far enough apart to tell it
                    *gesture = Gesture::Pending { ids, distance, angle: new_angle };
                }
            }
            Gesture::Active { ids, initial_distance, ref mut last_scale, ref mut last_angle } => {
                let Some((distance, angle)) = geometry(&self.fingers, ids) else { return };
                // ScaleRotateGestureHandler multiplies its scale by `1 + delta` on every event
                let scale = distance / initial_distance;
                let scale_delta = scale / *last_scale - 1.0;
                // `signed()` keeps crossing the +/-180 degrees boundary from making a full turn
                let rotation_delta = match (*last_angle, angle) {
                    (Some(last), Some(angle)) => (angle - last).signed().to_degrees(),
                    _ => 0.0,
                };
                *last_scale = scale;
                // Fingers passing through each other turn the angle around, which is no rotation
                *last_angle = angle;

                let position = midpoint(&self.fingers, ids).unwrap_or_default();
                out(MouseEvent::PinchGesture { position, delta: scale_delta, phase: TouchPhase::Moved });
                out(MouseEvent::RotationGesture { position, delta: rotation_delta, phase: TouchPhase::Moved });
            }
        }
    }

    fn finger(&self, id: i32) -> Option<&Finger> { self.fingers.iter().find(|f| f.id == id) }

    fn midpoint(&self, ids: (i32, i32)) -> Option<LogicalPoint> { midpoint(&self.fingers, ids) }
}

/// The distance between two fingers, never less than [`MIN_PINCH_SPAN`], and the angle of the line from
/// the first to the second, if they are far enough apart to tell it.
fn geometry(fingers: &[Finger], (a, b): (i32, i32)) -> Option<(f32, Option<euclid::Angle<f32>>)> {
    let a = fingers.iter().find(|f| f.id == a)?;
    let b = fingers.iter().find(|f| f.id == b)?;
    let delta = (b.position - a.position).cast::<f32>();
    let length = delta.length();
    let angle = (length >= MIN_PINCH_SPAN).then(|| delta.angle_from_x_axis());
    Some((length.max(MIN_PINCH_SPAN), angle))
}

fn midpoint(fingers: &[Finger], (a, b): (i32, i32)) -> Option<LogicalPoint> {
    let a = fingers.iter().find(|f| f.id == a)?;
    let b = fingers.iter().find(|f| f.id == b)?;
    Some(a.position.lerp(b.position, 0.5))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    enum E {
        Pressed(f32, f32),
        Released(f32, f32),
        Moved(f32, f32),
        Exit,
        Pinch(TouchPhase),
        Rotation(TouchPhase),
    }

    fn convert(e: MouseEvent) -> E {
        match e {
            MouseEvent::Pressed { position, .. } => E::Pressed(position.x, position.y),
            MouseEvent::Released { position, .. } => E::Released(position.x, position.y),
            MouseEvent::Moved { position, .. } => E::Moved(position.x, position.y),
            MouseEvent::Exit => E::Exit,
            MouseEvent::PinchGesture { phase, .. } => E::Pinch(phase),
            MouseEvent::RotationGesture { phase, .. } => E::Rotation(phase),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A recognizer and a clock that moves on by 10 ms with every batch.
    struct Harness {
        r: TouchRecognizer,
        now: Instant,
    }

    impl Harness {
        fn new() -> Self { Self { r: TouchRecognizer::new(480.), now: Instant::now() } }

        fn wait(&mut self, ms: u64) { self.now += Duration::from_millis(ms); }

        fn run(&mut self, touches: &[(i32, f32, f32, TouchPhase)]) -> Vec<E> {
            self.wait(10);
            let mut events = Vec::new();
            let mut out = |e| events.push(convert(e));
            for &(id, x, y, phase) in touches {
                self.r.touch(id, LogicalPoint::new(x, y), phase, self.now, &mut out);
            }
            self.r.flush(&mut out);
            events
        }

        fn poll(&mut self) -> Vec<E> {
            let mut events = Vec::new();
            self.r.poll(self.now, &mut |e| events.push(convert(e)));
            events
        }
    }

    use TouchPhase::*;

    #[test]
    fn single_finger_is_the_pointer() {
        let mut h = Harness::new();
        assert_eq!(
            h.run(&[(0, 1., 1., Started), (0, 2., 2., Moved), (0, 3., 3., Moved), (0, 3., 3., Ended)]),
            [E::Pressed(1., 1.), E::Moved(3., 3.), E::Released(3., 3.), E::Exit]
        );
        // No late exit after a normal tap
        h.wait(1000);
        assert_eq!(h.poll(), []);
    }

    #[test]
    fn second_finger_cancels_the_press_and_nothing_clicks_until_all_fingers_are_up() {
        let mut h = Harness::new();
        assert_eq!(h.run(&[(0, 100., 100., Started), (1, 200., 100., Started)]), [E::Pressed(100., 100.), E::Exit]);
        // Lifting one finger and then the other never releases the pointer
        assert_eq!(h.run(&[(1, 200., 100., Ended)]), []);
        assert_eq!(h.run(&[(0, 120., 100., Moved), (0, 120., 100., Ended)]), [E::Exit]);
        // Back to normal afterwards, after exiting again in case Slint delivered a press it held back
        assert_eq!(h.run(&[(0, 5., 5., Started)]), [E::Exit, E::Pressed(5., 5.)]);
    }

    #[test]
    fn cancelling_exit_is_sent_again_unless_a_new_press_came() {
        let mut h = Harness::new();
        h.run(&[(0, 100., 100., Started), (1, 200., 100., Started), (1, 200., 100., Ended), (0, 100., 100., Ended)]);
        assert_eq!(h.poll(), []);
        h.wait(200);
        assert_eq!(h.poll(), [E::Exit]);
        assert_eq!(h.poll(), []);

        h.run(&[(0, 100., 100., Started), (1, 200., 100., Started), (1, 200., 100., Ended), (0, 100., 100., Ended)]);
        h.run(&[(0, 5., 5., Started)]);
        h.wait(200);
        assert_eq!(h.poll(), []);
    }

    /// Pinches from `from` to `to` pixels apart in many small steps, and returns the accumulated scale
    /// divided by the change of the span, so 1 when the scale follows the fingers.
    fn pinch_scale_error(from: f32, to: f32) -> f32 {
        let mut h = Harness::new();
        let mut scale = 1.0f32;
        let mut started = false;
        let mut out = |e: MouseEvent| {
            if let MouseEvent::PinchGesture { delta, phase, .. } = e {
                started |= phase == Started;
                scale *= 1.0 + delta;
            }
        };
        let now = h.now;
        h.r.touch(0, LogicalPoint::new(100., 400.), Started, now, &mut out);
        h.r.touch(1, LogicalPoint::new(100. + from, 400.), Started, now, &mut out);
        // Get past the slop, then move in many small steps
        let mut x = 100. + from + if to > from { 30. } else { -30. };
        let start_span = x - 100.;
        h.r.touch(1, LogicalPoint::new(x, 400.), Moved, now, &mut out);
        h.r.flush(&mut out);
        for _ in 0..50 {
            x += (to - start_span) / 50.;
            h.r.touch(1, LogicalPoint::new(x, 400.), Moved, now, &mut out);
            h.r.flush(&mut out);
        }
        h.r.touch(1, LogicalPoint::new(x, 400.), Ended, now, &mut out);
        assert!(started);
        scale * start_span.max(MIN_PINCH_SPAN) / to.max(MIN_PINCH_SPAN)
    }

    #[test]
    fn pinch_scale_follows_the_fingers() {
        assert!((pinch_scale_error(120., 250.) - 1.).abs() < 1e-3);
        assert!((pinch_scale_error(300., 100.) - 1.).abs() < 1e-3);
        // Fingers closing to nothing don't freeze or flip the scale
        assert!((pinch_scale_error(150., 0.) - 1.).abs() < 1e-3);
    }

    #[test]
    fn gesture_needs_slop_and_ends_without_a_press() {
        let mut h = Harness::new();
        h.run(&[(0, 100., 400., Started), (1, 200., 400., Started)]);
        assert_eq!(h.run(&[(1, 210., 400., Moved)]), []);
        assert_eq!(h.run(&[(1, 240., 400., Moved)]), [E::Pinch(Started), E::Rotation(Started)]);
        assert_eq!(h.run(&[(0, 100., 400., Ended)]), [E::Pinch(Ended), E::Rotation(Ended)]);
        // The remaining finger stays inert
        assert_eq!(h.run(&[(1, 260., 420., Moved)]), []);
        assert_eq!(h.run(&[(1, 260., 420., Ended)]), [E::Exit]);
    }

    #[test]
    fn close_fingers_jittering_make_no_rotation() {
        let mut h = Harness::new();
        h.run(&[(0, 100., 400., Started), (1, 103., 400., Started)]);
        assert_eq!(h.run(&[(1, 100., 403., Moved)]), []);
        assert_eq!(h.run(&[(1, 97., 400., Moved)]), []);
    }

    #[test]
    fn gesture_continues_with_the_remaining_two_fingers() {
        let mut h = Harness::new();
        h.run(&[(0, 100., 400., Started), (1, 200., 400., Started), (2, 300., 300., Started)]);
        h.run(&[(1, 260., 400., Moved)]);
        assert_eq!(h.run(&[(0, 100., 400., Ended)]), [E::Pinch(Ended), E::Rotation(Ended)]);
        assert_eq!(h.run(&[(2, 400., 300., Moved)]), [E::Pinch(Started), E::Rotation(Started)]);
    }

    #[test]
    fn cancel_never_releases_and_waits_for_a_new_first_finger() {
        let mut h = Harness::new();
        h.run(&[(0, 1., 1., Started)]);
        assert_eq!(h.run(&[(0, 0., 0., Cancelled)]), [E::Exit]);
        // A cancel with nothing known still exits
        assert_eq!(h.run(&[(0, 0., 0., Cancelled)]), [E::Exit]);

        h.run(&[(0, 100., 400., Started), (1, 200., 400., Started), (1, 260., 400., Moved)]);
        assert_eq!(h.run(&[(0, 0., 0., Cancelled)]), [E::Pinch(Cancelled), E::Rotation(Cancelled), E::Exit]);
        // Fingers we don't know anymore are ignored, and so are new fingers until a first one lands
        assert_eq!(h.run(&[(1, 270., 400., Moved), (1, 270., 400., Ended)]), []);
        assert_eq!(h.run(&[(2, 50., 50., Started), (2, 50., 50., Ended)]), []);
        assert_eq!(h.run(&[(0, 5., 5., Started)]), [E::Exit, E::Pressed(5., 5.)]);
    }

    #[test]
    fn pressing_a_finger_that_is_down_starts_over() {
        let mut h = Harness::new();
        assert_eq!(h.run(&[(0, 1., 1., Started)]), [E::Pressed(1., 1.)]);
        // We missed the lift: what was pressed is cancelled, not dragged to the new position
        assert_eq!(h.run(&[(0, 400., 1., Started), (0, 400., 1., Ended)]), [
            E::Exit,
            E::Pressed(400., 1.),
            E::Released(400., 1.),
            E::Exit
        ]);
    }

    #[test]
    fn finger_resting_on_a_side_edge_is_dropped_for_a_new_one() {
        let mut h = Harness::new();
        // A thumb holding the device on the right edge, then a tap in the middle
        assert_eq!(h.run(&[(0, 475., 400., Started)]), [E::Pressed(475., 400.)]);
        h.wait(500);
        assert_eq!(h.run(&[(1, 240., 300., Started)]), [E::Exit, E::Pressed(240., 300.)]);
        assert_eq!(h.run(&[(1, 240., 300., Ended)]), [E::Released(240., 300.), E::Exit]);
        // Taps keep working while the thumb rests, and the thumb lifts silently
        assert_eq!(h.run(&[(1, 100., 100., Started), (1, 100., 100., Ended)]), [
            E::Pressed(100., 100.),
            E::Released(100., 100.),
            E::Exit
        ]);
        assert_eq!(h.run(&[(0, 475., 400., Ended)]), []);
    }

    #[test]
    fn edge_finger_is_not_a_grip_if_it_moved_or_just_landed() {
        let mut h = Harness::new();
        h.run(&[(0, 5., 400., Started), (0, 100., 400., Moved)]);
        h.wait(500);
        assert_eq!(h.run(&[(1, 240., 300., Started)]), [E::Exit]);

        let mut h = Harness::new();
        h.run(&[(0, 5., 400., Started)]);
        assert_eq!(h.run(&[(1, 240., 300., Started)]), [E::Exit]);
    }

    #[test]
    fn late_exit_comes_during_a_gesture_and_before_a_new_press() {
        let mut h = Harness::new();
        h.run(&[(0, 100., 400., Started), (1, 200., 400., Started), (1, 260., 400., Moved)]);
        h.wait(200);
        assert_eq!(h.poll(), [E::Exit]);

        h.run(&[(0, 100., 400., Ended), (1, 260., 400., Ended)]);
        assert_eq!(h.run(&[(0, 5., 5., Started)]), [E::Exit, E::Pressed(5., 5.)]);
    }

    /// Random touch sequences never click after a second finger or a cancel, and every press ends.
    #[test]
    fn invariants_hold_for_random_touches() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut random = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };

        for _ in 0..2000 {
            let mut h = Harness::new();
            // Fingers on the screen, numbered like the gui-server does: 0 for the first one after none,
            // the lowest free id above 0 for the others
            let mut down: Vec<i32> = Vec::new();
            // Whether Slint has a press, and whether a second finger or a cancel happened since all
            // fingers were last up
            let mut pressed = false;
            let mut tainted = false;
            for _ in 0..30 {
                // Away from the side edges, where resting fingers are taken for a grip
                let (x, y) = (16. + random(448) as f32, random(800) as f32);
                let (id, phase) = match random(10) {
                    0 => (0, Cancelled),
                    1..=3 if !down.is_empty() => (down[random(down.len() as u64) as usize], Ended),
                    4..=7 if !down.is_empty() => (down[random(down.len() as u64) as usize], Moved),
                    // A press of the only finger down, whose lift we missed
                    8 if down.len() == 1 => (down[0], Started),
                    _ if down.len() < 4 => {
                        let id = if down.is_empty() { 0 } else { (1..).find(|i| !down.contains(i)).unwrap() };
                        (id, Started)
                    }
                    _ => continue,
                };
                match phase {
                    // The finger was lifted and put down again: a new touch sequence
                    Started if down.contains(&id) => tainted = false,
                    Started => down.push(id),
                    Ended => down.retain(|&d| d != id),
                    _ => {}
                }
                if down.len() > 1 || phase == Cancelled {
                    tainted = true;
                }

                h.wait(random(400));
                for e in h.run(&[(id, x, y, phase)]).into_iter().chain(h.poll()) {
                    match e {
                        E::Pressed(..) => {
                            assert!(!pressed, "pressed twice");
                            assert!(!tainted, "pressed after a second finger or a cancel");
                            pressed = true;
                        }
                        E::Released(..) => {
                            assert!(pressed, "released without a press");
                            assert!(!tainted, "released after a second finger or a cancel");
                            pressed = false;
                        }
                        E::Exit => pressed = false,
                        _ => {}
                    }
                }
                if down.is_empty() {
                    assert!(!pressed, "pressed after all fingers were lifted");
                    tainted = false;
                }
            }
        }
    }
}
