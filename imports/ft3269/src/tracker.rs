//! Turns the touch points of consecutive controller frames into a consistent stream of press, drag and
//! release events.
//!
//! The controller flags each point with a one-frame event (press-down, contact or lift-up). A lost frame
//! (an I2C error, a missed interrupt) would lose that event, leaving a finger down forever or pressing
//! a finger that is already down. Instead, frames are compared by touch id: a finger that is missing
//! from a frame is released, and a finger that appears is pressed.
//!
//! A contact also has to be seen in two consecutive frames before it is pressed, so that noise that shows
//! up as a contact for a single frame can't tap anything. While a finger is down the controller reports it
//! on every scan, so this delays a press by one scan.

use crate::reg::{Touch, TouchKind, NUM_REGS_PER_TOUCH};

/// Number of simultaneous touch points the controller reports.
pub const MAX_POINTS: usize = 5;

/// Size of a frame read starting at the status register: the status byte, then the touch points.
pub const FRAME_LEN: usize = 1 + MAX_POINTS * NUM_REGS_PER_TOUCH as usize;

/// One validated report of the fingers that are down.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Frame {
    points: [Option<Point>; MAX_POINTS],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Point {
    x: u16,
    y: u16,
    /// The controller reports this as a new contact.
    new: bool,
    weight: u8,
    area: u8,
}

impl Frame {
    /// Parses a frame read starting at the status register. Returns `None` for a frame that can't be
    /// trusted (too many points, invalid or duplicate ids, unknown event flags).
    pub fn parse(data: &[u8; FRAME_LEN]) -> Option<Self> {
        let count = (data[0] & 0x0f) as usize;
        if count > MAX_POINTS {
            return None;
        }

        let mut frame = Frame::default();
        for chunk in data[1..].chunks(NUM_REGS_PER_TOUCH as usize).take(count) {
            let touch = Touch::from_data(chunk);
            let new = match touch.kind {
                TouchKind::Press => true,
                TouchKind::Drag => false,
                // A finger that is being lifted is not down anymore
                TouchKind::Release => continue,
                TouchKind::Reserved => return None,
            };
            let slot = frame.points.get_mut(touch.id as usize)?;
            if slot.is_some() {
                return None;
            }
            // As on FT5x06: the 5th byte is the touch weight, the upper nibble of the 6th the touch area
            *slot = Some(Point { x: touch.x, y: touch.y, new, weight: chunk[4], area: chunk[5] >> 4 });
        }

        Some(frame)
    }

    #[cfg(test)]
    fn with(points: &[(u8, u16, u16, bool)]) -> Self {
        let mut frame = Frame::default();
        for &(id, x, y, new) in points {
            frame.points[id as usize] = Some(Point { x, y, new, weight: 0, area: 0 });
        }
        frame
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchEventKind {
    Press,
    Drag,
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TouchEvent {
    pub kind: TouchEventKind,
    /// 0 for the first finger to touch the screen after no finger did, and the lowest free id above 0 for
    /// fingers that touch it while another one is down.
    pub id: u8,
    pub x: u16,
    pub y: u16,
    /// Touch weight and area as reported by the controller, for presses and drags. Large contacts are
    /// likely palms; the values that tell them apart are yet to be measured.
    pub weight: u8,
    pub area: u8,
}

impl TouchEvent {
    fn at(kind: TouchEventKind, id: u8, point: &Point) -> Self {
        Self { kind, id, x: point.x, y: point.y, weight: point.weight, area: point.area }
    }

    fn release(finger: DownFinger) -> Self {
        Self { kind: TouchEventKind::Release, id: finger.id, x: finger.x, y: finger.y, weight: 0, area: 0 }
    }
}

/// Number of different ids `TouchEvent`s use: one more than the number of points, since id 0 is kept for
/// the first finger.
pub const NUM_EVENT_IDS: usize = MAX_POINTS + 1;

#[derive(Debug, Default)]
pub struct FrameTracker {
    /// Indexed by the controller's touch id.
    down: [Option<DownFinger>; MAX_POINTS],
    /// Contacts seen in the last frame only, indexed by the controller's touch id.
    candidates: [bool; MAX_POINTS],
}

#[derive(Debug, Clone, Copy)]
struct DownFinger {
    id: u8,
    x: u16,
    y: u16,
}

impl FrameTracker {
    /// Compares `frame` to the previous one and emits the releases, then the drags, then the presses
    /// that lead from one to the other.
    pub fn update(&mut self, frame: &Frame, mut emit: impl FnMut(TouchEvent)) {
        for (down, point) in self.down.iter_mut().zip(&frame.points) {
            if let Some(finger) = *down {
                // A new contact under a known id means that we missed the old one being lifted
                if point.map_or(true, |p| p.new) {
                    emit(TouchEvent::release(finger));
                    *down = None;
                }
            }
        }

        for (down, point) in self.down.iter_mut().zip(&frame.points) {
            if let (Some(finger), Some(point)) = (down.as_mut(), point) {
                if (finger.x, finger.y) != (point.x, point.y) {
                    (finger.x, finger.y) = (point.x, point.y);
                    emit(TouchEvent::at(TouchEventKind::Drag, finger.id, point));
                }
            }
        }

        for hw_id in 0..MAX_POINTS {
            match (self.down[hw_id], frame.points[hw_id]) {
                (None, Some(point)) if self.candidates[hw_id] => {
                    self.candidates[hw_id] = false;
                    let id = self.free_id();
                    self.down[hw_id] = Some(DownFinger { id, x: point.x, y: point.y });
                    emit(TouchEvent::at(TouchEventKind::Press, id, &point));
                }
                (None, point) => self.candidates[hw_id] = point.is_some(),
                (Some(_), _) => {}
            }
        }
    }

    /// Forgets all fingers, emitting a release for each, e.g. when the controller is reset or put to sleep.
    pub fn release_all(&mut self, mut emit: impl FnMut(TouchEvent)) {
        self.candidates = Default::default();
        for down in &mut self.down {
            if let Some(finger) = down.take() {
                emit(TouchEvent::release(finger));
            }
        }
    }

    fn free_id(&self) -> u8 {
        let in_use = |id| self.down.iter().flatten().any(|f| f.id == id);
        if self.down.iter().all(Option::is_none) {
            return 0;
        }
        (1..NUM_EVENT_IDS as u8).find(|&id| !in_use(id)).unwrap_or(NUM_EVENT_IDS as u8 - 1)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use std::vec::Vec;

    use super::*;

    fn run(tracker: &mut FrameTracker, frame: Frame) -> Vec<(TouchEventKind, u8, u16, u16)> {
        let mut events = Vec::new();
        tracker.update(&frame, |e| events.push((e.kind, e.id, e.x, e.y)));
        events
    }

    use TouchEventKind::*;

    /// Runs a frame twice, so that its new contacts get past the debounce.
    fn run2(tracker: &mut FrameTracker, frame: Frame) -> Vec<(TouchEventKind, u8, u16, u16)> {
        let mut events = run(tracker, frame);
        events.extend(run(tracker, frame));
        events
    }

    #[test]
    fn single_finger() {
        let mut t = FrameTracker::default();
        assert_eq!(run(&mut t, Frame::with(&[(0, 10, 20, true)])), []);
        assert_eq!(run(&mut t, Frame::with(&[(0, 10, 20, false)])), [(Press, 0, 10, 20)]);
        assert_eq!(run(&mut t, Frame::with(&[(0, 10, 20, false)])), []);
        assert_eq!(run(&mut t, Frame::with(&[(0, 15, 25, false)])), [(Drag, 0, 15, 25)]);
        // Released where it was last seen, not where the lift-up frame says
        assert_eq!(run(&mut t, Frame::default()), [(Release, 0, 15, 25)]);
    }

    #[test]
    fn single_frame_contacts_are_ignored() {
        let mut t = FrameTracker::default();
        assert_eq!(run(&mut t, Frame::with(&[(0, 10, 20, true)])), []);
        assert_eq!(run(&mut t, Frame::default()), []);
        assert_eq!(run(&mut t, Frame::with(&[(1, 30, 40, true)])), []);
        assert_eq!(run(&mut t, Frame::with(&[(2, 30, 40, true)])), []);
        assert_eq!(run(&mut t, Frame::default()), []);
    }

    #[test]
    fn missed_lift_is_released_on_the_next_frame() {
        let mut t = FrameTracker::default();
        run2(&mut t, Frame::with(&[(0, 10, 20, true)]));
        // The lift-up frame was lost, and the next frames are a new finger under the same id
        assert_eq!(
            run2(&mut t, Frame::with(&[(0, 100, 200, true)])),
            [(Release, 0, 10, 20), (Press, 0, 100, 200)]
        );
    }

    #[test]
    fn missed_press_still_presses() {
        let mut t = FrameTracker::default();
        assert_eq!(run2(&mut t, Frame::with(&[(0, 10, 20, false)])), [(Press, 0, 10, 20)]);
    }

    #[test]
    fn first_finger_keeps_id_zero_and_later_fingers_get_free_ids() {
        let mut t = FrameTracker::default();
        // The controller numbers the first finger 1, e.g. because id 0 was used just before
        assert_eq!(run2(&mut t, Frame::with(&[(1, 10, 10, true)])), [(Press, 0, 10, 10)]);
        assert_eq!(run2(&mut t, Frame::with(&[(0, 50, 50, true), (1, 10, 10, false)])), [(Press, 1, 50, 50)]);
        // The first finger lifts, and a new finger reuses its controller id: it is not the first finger
        assert_eq!(run(&mut t, Frame::with(&[(0, 50, 50, false)])), [(Release, 0, 10, 10)]);
        assert_eq!(run2(&mut t, Frame::with(&[(0, 50, 50, false), (1, 90, 90, true)])), [(Press, 2, 90, 90)]);
        // All fingers up: the next one is the first finger again
        run(&mut t, Frame::default());
        assert_eq!(run2(&mut t, Frame::with(&[(3, 1, 1, true)])), [(Press, 0, 1, 1)]);
    }

    #[test]
    fn releases_come_before_drags_and_presses() {
        let mut t = FrameTracker::default();
        run2(&mut t, Frame::with(&[(0, 10, 10, true), (1, 20, 20, true)]));
        run(&mut t, Frame::with(&[(1, 20, 20, false), (2, 30, 30, true)]));
        assert_eq!(
            run(&mut t, Frame::with(&[(1, 25, 25, false), (2, 30, 30, false)])),
            [(Drag, 1, 25, 25), (Press, 2, 30, 30)]
        );
        assert_eq!(
            run(&mut t, Frame::with(&[(1, 26, 26, false), (3, 50, 50, true)])),
            [(Release, 2, 30, 30), (Drag, 1, 26, 26)]
        );
    }

    #[test]
    fn release_all() {
        let mut t = FrameTracker::default();
        run2(&mut t, Frame::with(&[(0, 10, 10, true), (1, 20, 20, true)]));
        run(&mut t, Frame::with(&[(0, 10, 10, false), (1, 20, 20, false), (2, 5, 5, true)]));
        let mut events = Vec::new();
        t.release_all(|e| events.push((e.kind, e.id)));
        assert_eq!(events, [(Release, 0), (Release, 1)]);
        // The contact that was only seen once is forgotten too
        assert_eq!(run(&mut t, Frame::with(&[(2, 5, 5, false)])), []);
    }

    fn raw(count: u8, points: &[[u8; 6]]) -> [u8; FRAME_LEN] {
        let mut data = [0xff; FRAME_LEN];
        data[0] = count;
        for (i, p) in points.iter().enumerate() {
            data[1 + i * 6..1 + (i + 1) * 6].copy_from_slice(p);
        }
        data
    }

    #[test]
    fn parse() {
        // Point 0: contact, x = 0x123, id 2, y = 0x345
        let frame = Frame::parse(&raw(1, &[[0x81, 0x23, 0x23, 0x45, 0, 0]])).unwrap();
        assert_eq!(frame, Frame::with(&[(2, 0x123, 0x345, false)]));

        // Points past the count are ignored, and so are fingers being lifted
        let frame = Frame::parse(&raw(1, &[[0x41, 0x23, 0x03, 0x45, 0, 0], [0x00, 1, 0x10, 1, 0, 0]])).unwrap();
        assert_eq!(frame, Frame::default());

        assert_eq!(Frame::parse(&raw(0, &[])), Some(Frame::default()));
        // Too many points, an invalid id, duplicate ids
        assert_eq!(Frame::parse(&raw(0x0f, &[])), None);
        assert_eq!(Frame::parse(&raw(1, &[[0x80, 0, 0xf0, 0, 0, 0]])), None);
        assert_eq!(Frame::parse(&raw(2, &[[0x80, 0, 0x10, 0, 0, 0], [0x80, 5, 0x10, 5, 0, 0]])), None);
    }
}
