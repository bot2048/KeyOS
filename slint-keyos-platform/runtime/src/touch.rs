// SPDX-FileCopyrightText: 2026 Foundation Devices, Inc. <hello@foundation.xyz>
// SPDX-License-Identifier: GPL-3.0-or-later

//! Turns the fingers touching an app into Slint pointer and gesture events.
//!
//! One finger is the pointer. As soon as a second finger lands, the pointer press is cancelled rather
//! than released, and no press or click happens until all fingers are lifted, so that multi-finger
//! touches never activate a button by accident. Two fingers make pinch and rotation gestures, which
//! `ScaleRotateGestureHandler` turns into a scale and a rotation.

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

#[derive(Default)]
pub(crate) struct TouchRecognizer {
    fingers: Vec<Finger>,
    mode: Mode,
    /// A finger moved since the last pointer or gesture event.
    moved: bool,
}

#[derive(Clone, Copy)]
struct Finger {
    id: i32,
    position: LogicalPoint,
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
    Pending { ids: (i32, i32), distance: f32, angle: euclid::Angle<f32> },
    Active { ids: (i32, i32), initial_distance: f32, last_scale: f32, last_angle: euclid::Angle<f32> },
}

impl TouchRecognizer {
    /// Processes a touch of a finger. Moves are only applied with the next [`Self::flush`] or touch of
    /// another kind, so that a batch of moves results in a single pointer or gesture update.
    pub(crate) fn touch(
        &mut self,
        id: i32,
        position: LogicalPoint,
        phase: TouchPhase,
        out: &mut impl FnMut(MouseEvent),
    ) {
        match phase {
            TouchPhase::Started => {
                self.flush(out);
                self.started(id, position, out);
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
                self.ended(id, out);
            }
            TouchPhase::Cancelled => self.cancel(out),
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

    /// Takes all fingers away without activating anything.
    pub(crate) fn cancel(&mut self, out: &mut impl FnMut(MouseEvent)) {
        if let Mode::Multi(Gesture::Active { ids, .. }) = self.mode {
            let position = self.midpoint(ids).unwrap_or_default();
            out(MouseEvent::PinchGesture { position, delta: 0.0, phase: TouchPhase::Cancelled });
            out(MouseEvent::RotationGesture { position, delta: 0.0, phase: TouchPhase::Cancelled });
        }
        if !matches!(self.mode, Mode::Idle) {
            out(MouseEvent::Exit);
        }
        self.fingers.clear();
        self.mode = Mode::Idle;
        self.moved = false;
    }

    fn started(&mut self, id: i32, position: LogicalPoint, out: &mut impl FnMut(MouseEvent)) {
        if let Some(finger) = self.fingers.iter_mut().find(|f| f.id == id) {
            // We missed this finger being lifted; carry on with it
            finger.position = position;
            self.moved = true;
            return;
        }
        self.fingers.push(Finger { id, position });

        match self.mode {
            Mode::Idle => {
                self.mode = Mode::Pointer { id };
                out(MouseEvent::Pressed {
                    position,
                    button: PointerEventButton::Left,
                    click_count: 0,
                    touch_finger_id: id + 1,
                });
            }
            Mode::Pointer { .. } => {
                // Cancel the press, so that it doesn't turn into a click
                out(MouseEvent::Exit);
                self.mode = Mode::Multi(self.pending_gesture());
            }
            Mode::Multi(Gesture::None) => self.mode = Mode::Multi(self.pending_gesture()),
            // Further fingers don't take part in the gesture
            Mode::Multi(_) => {}
        }
    }

    fn ended(&mut self, id: i32, out: &mut impl FnMut(MouseEvent)) {
        let Some(index) = self.fingers.iter().position(|f| f.id == id) else { return };
        let finger = self.fingers[index];

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
                self.mode = Mode::Multi(Gesture::None);
            }
            Mode::Multi(Gesture::Active { ids, .. }) if ids.0 == id || ids.1 == id => {
                let position = self.midpoint(ids).unwrap_or(finger.position);
                out(MouseEvent::PinchGesture { position, delta: 0.0, phase: TouchPhase::Ended });
                out(MouseEvent::RotationGesture { position, delta: 0.0, phase: TouchPhase::Ended });
                self.mode = Mode::Multi(Gesture::None);
            }
            Mode::Multi(_) => {}
        }

        self.fingers.remove(index);
        if self.fingers.is_empty() && matches!(self.mode, Mode::Multi(_)) {
            out(MouseEvent::Exit);
            self.mode = Mode::Idle;
        }
    }

    /// A new gesture between the first two fingers that are down, if there are two.
    fn pending_gesture(&self) -> Gesture {
        let [a, b, ..] = self.fingers[..] else { return Gesture::None };
        let ids = (a.id, b.id);
        match self.geometry(ids) {
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
                if (new_distance - distance).abs() > PINCH_SLOP
                    || (new_angle - angle).signed().to_degrees().abs() > ROTATION_SLOP_DEGREES
                {
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
                }
            }
            Gesture::Active { ids, initial_distance, ref mut last_scale, ref mut last_angle } => {
                let Some((distance, angle)) = geometry(&self.fingers, ids) else { return };
                let scale = if initial_distance > 0.0 { distance / initial_distance } else { 1.0 };
                // ScaleRotateGestureHandler multiplies its scale by `1 + delta` on every event
                let scale_delta = if *last_scale > 0.0 { scale / *last_scale - 1.0 } else { 0.0 };
                // `signed()` keeps crossing the +/-180 degrees boundary from making a full turn
                let rotation_delta = (angle - *last_angle).signed().to_degrees();
                *last_scale = scale;
                *last_angle = angle;

                let position = midpoint(&self.fingers, ids).unwrap_or_default();
                out(MouseEvent::PinchGesture { position, delta: scale_delta, phase: TouchPhase::Moved });
                out(MouseEvent::RotationGesture { position, delta: rotation_delta, phase: TouchPhase::Moved });
            }
        }
    }

    fn finger(&self, id: i32) -> Option<&Finger> { self.fingers.iter().find(|f| f.id == id) }

    fn geometry(&self, ids: (i32, i32)) -> Option<(f32, euclid::Angle<f32>)> { geometry(&self.fingers, ids) }

    fn midpoint(&self, ids: (i32, i32)) -> Option<LogicalPoint> { midpoint(&self.fingers, ids) }
}

/// The distance between two fingers and the angle of the line from the first to the second.
fn geometry(fingers: &[Finger], (a, b): (i32, i32)) -> Option<(f32, euclid::Angle<f32>)> {
    let a = fingers.iter().find(|f| f.id == a)?;
    let b = fingers.iter().find(|f| f.id == b)?;
    let delta = (b.position - a.position).cast::<f32>();
    Some((delta.length(), delta.angle_from_x_axis()))
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

    fn run(recognizer: &mut TouchRecognizer, touches: &[(i32, f32, f32, TouchPhase)]) -> Vec<E> {
        let mut events = Vec::new();
        let mut out = |e: MouseEvent| {
            events.push(match e {
                MouseEvent::Pressed { position, .. } => E::Pressed(position.x, position.y),
                MouseEvent::Released { position, .. } => E::Released(position.x, position.y),
                MouseEvent::Moved { position, .. } => E::Moved(position.x, position.y),
                MouseEvent::Exit => E::Exit,
                MouseEvent::PinchGesture { phase, .. } => E::Pinch(phase),
                MouseEvent::RotationGesture { phase, .. } => E::Rotation(phase),
                other => panic!("unexpected {other:?}"),
            })
        };
        for &(id, x, y, phase) in touches {
            recognizer.touch(id, LogicalPoint::new(x, y), phase, &mut out);
        }
        recognizer.flush(&mut out);
        events
    }

    use TouchPhase::*;

    #[test]
    fn single_finger_is_the_pointer() {
        let mut r = TouchRecognizer::default();
        assert_eq!(
            run(&mut r, &[(0, 1., 1., Started), (0, 2., 2., Moved), (0, 3., 3., Moved), (0, 3., 3., Ended)]),
            [E::Pressed(1., 1.), E::Moved(3., 3.), E::Released(3., 3.), E::Exit]
        );
    }

    #[test]
    fn second_finger_cancels_the_press_and_nothing_clicks_until_all_fingers_are_up() {
        let mut r = TouchRecognizer::default();
        assert_eq!(run(&mut r, &[(0, 100., 100., Started), (1, 200., 100., Started)]), [E::Pressed(100., 100.), E::Exit]);
        // Lifting one finger and then the other never releases the pointer
        assert_eq!(run(&mut r, &[(1, 200., 100., Ended)]), []);
        assert_eq!(run(&mut r, &[(0, 120., 100., Moved), (0, 120., 100., Ended)]), [E::Exit]);
        // Back to normal afterwards
        assert_eq!(run(&mut r, &[(0, 5., 5., Started)]), [E::Pressed(5., 5.)]);
    }

    #[test]
    fn pinch_scale_follows_the_fingers() {
        let mut r = TouchRecognizer::default();
        let mut scale = 1.0f32;
        let mut started = false;
        let mut out = |e: MouseEvent| {
            if let MouseEvent::PinchGesture { delta, phase, .. } = e {
                started |= phase == Started;
                scale *= 1.0 + delta;
            }
        };
        r.touch(0, LogicalPoint::new(100., 400.), Started, &mut out);
        r.touch(1, LogicalPoint::new(200., 400.), Started, &mut out);
        // Get past the slop, then spread the fingers in many small steps
        r.touch(1, LogicalPoint::new(250., 400.), Moved, &mut out);
        r.flush(&mut out);
        for step in 1..=50 {
            r.touch(1, LogicalPoint::new(250. + 2. * step as f32, 400.), Moved, &mut out);
            r.flush(&mut out);
        }
        r.touch(1, LogicalPoint::new(350., 400.), Ended, &mut out);
        assert!(started);
        // From 150 px to 250 px apart
        assert!((scale - 250. / 150.).abs() < 1e-4, "scale {scale}");
    }

    #[test]
    fn gesture_needs_slop_and_ends_without_a_press() {
        let mut r = TouchRecognizer::default();
        run(&mut r, &[(0, 100., 400., Started), (1, 200., 400., Started)]);
        assert_eq!(run(&mut r, &[(1, 210., 400., Moved)]), []);
        assert_eq!(run(&mut r, &[(1, 240., 400., Moved)]), [E::Pinch(Started), E::Rotation(Started)]);
        assert_eq!(run(&mut r, &[(0, 100., 400., Ended)]), [E::Pinch(Ended), E::Rotation(Ended)]);
        // The remaining finger stays inert
        assert_eq!(run(&mut r, &[(1, 260., 420., Moved)]), []);
        assert_eq!(run(&mut r, &[(1, 260., 420., Ended)]), [E::Exit]);
    }

    #[test]
    fn cancel_never_releases() {
        let mut r = TouchRecognizer::default();
        run(&mut r, &[(0, 1., 1., Started)]);
        assert_eq!(run(&mut r, &[(0, 0., 0., Cancelled)]), [E::Exit]);

        run(&mut r, &[(0, 100., 400., Started), (1, 200., 400., Started), (1, 260., 400., Moved)]);
        assert_eq!(
            run(&mut r, &[(0, 0., 0., Cancelled)]),
            [E::Pinch(Cancelled), E::Rotation(Cancelled), E::Exit]
        );
        // Touches of fingers we don't know about anymore are ignored
        assert_eq!(run(&mut r, &[(1, 270., 400., Moved), (1, 270., 400., Ended)]), []);
    }

    #[test]
    fn pressing_a_finger_that_is_down_does_not_press_again() {
        let mut r = TouchRecognizer::default();
        assert_eq!(
            run(&mut r, &[(0, 1., 1., Started), (0, 4., 4., Started), (0, 4., 4., Ended)]),
            [E::Pressed(1., 1.), E::Moved(4., 4.), E::Released(4., 4.), E::Exit]
        );
    }
}
