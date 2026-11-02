//! Turns the touch points of consecutive controller frames into a consistent stream of press, drag and
//! release events.
//!
//! The controller flags each point with a one-frame event (press-down, contact or lift-up). A lost frame
//! (an I2C error, a missed interrupt) would lose that event, leaving a finger down forever or pressing
//! a finger that is already down. Instead, frames are compared by touch id: a finger that is missing
//! from a frame is released, and a finger that appears is pressed.
//!
//! A finger that is missing from a frame right after one it was in is released. If we lost track of it
//! instead (frames were lost in between, or the controller gives its id to a new contact), it is
//! cancelled, since it was lifted some time ago and maybe somewhere else than where we last saw it.
//!
//! A finger that moves further between two frames than any finger does is taken to be another finger
//! that touched after we missed the first one being lifted, and is cancelled. If frames were lost in
//! between, a much shorter move is enough, since a finger lifted and put down elsewhere looks like a
//! move then. Contacts cancelled this way are ignored until lifted.
//!
//! After the controller is reset, fingers that are already touching are ignored until all are lifted,
//! so that touching the screen while it wakes up doesn't press anything.
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
    /// The finger is gone, but must not activate anything.
    Cancel,
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

    fn gone(kind: TouchEventKind, finger: DownFinger) -> Self {
        Self { kind, id: finger.id, x: finger.x, y: finger.y, weight: 0, area: 0 }
    }
}

/// Furthest a finger moves between two frames (in pixels, about 32 mm): further is another finger.
const MAX_FRAME_JUMP: u32 = 300;

/// Furthest a finger moves (in pixels, about 5 mm) between two frames that had frames lost in between,
/// before we take it for another finger.
const MAX_LOST_FRAMES_JUMP: u32 = 48;

/// Number of different ids `TouchEvent`s use: one more than the number of points, since id 0 is kept for
/// the first finger.
pub const NUM_EVENT_IDS: usize = MAX_POINTS + 1;

#[derive(Debug, Default)]
pub struct FrameTracker {
    /// Indexed by the controller's touch id.
    down: [Option<DownFinger>; MAX_POINTS],
    /// Contacts seen in the last frame only, indexed by the controller's touch id.
    candidates: [bool; MAX_POINTS],
    /// A frame was lost since the last one.
    frame_lost: bool,
    /// Contacts are ignored until a frame without any.
    wait_for_lift: bool,
    /// Contacts we lost track of, ignored until they are lifted, indexed by the controller's touch id.
    ignored: [bool; MAX_POINTS],
}

#[derive(Debug, Clone, Copy)]
struct DownFinger {
    id: u8,
    x: u16,
    y: u16,
    /// Was reported as an ongoing contact since it was pressed. Some firmware flags a contact as new for
    /// more than one scan, which must not count as a new finger.
    seen_contact: bool,
}

impl DownFinger {
    fn jumped_to(&self, point: &Point, frame_lost: bool) -> bool {
        let max = if frame_lost { MAX_LOST_FRAMES_JUMP } else { MAX_FRAME_JUMP };
        let dx = u32::from(self.x.abs_diff(point.x));
        let dy = u32::from(self.y.abs_diff(point.y));
        dx * dx + dy * dy > max * max
    }
}

impl FrameTracker {
    /// Compares `frame` to the previous one and emits the releases, then the drags, then the presses
    /// that lead from one to the other.
    pub fn update(&mut self, frame: &Frame, mut emit: impl FnMut(TouchEvent)) {
        let frame_lost = core::mem::take(&mut self.frame_lost);
        if self.wait_for_lift {
            self.wait_for_lift = frame.points.iter().any(Option::is_some);
            return;
        }

        for ((down, point), ignored) in self.down.iter_mut().zip(&frame.points).zip(&mut self.ignored) {
            *ignored &= point.is_some();
            if let Some(finger) = *down {
                let kind = match point {
                    None if !frame_lost => TouchEventKind::Release,
                    None => TouchEventKind::Cancel,
                    // A new contact under a known id, or one far away, means that we missed the old one being
                    // lifted
                    Some(p) if (p.new && finger.seen_contact) || finger.jumped_to(p, frame_lost) => {
                        *ignored = true;
                        TouchEventKind::Cancel
                    }
                    Some(_) => continue,
                };
                emit(TouchEvent::gone(kind, finger));
                *down = None;
            }
        }

        for (down, point) in self.down.iter_mut().zip(&frame.points) {
            if let (Some(finger), Some(point)) = (down.as_mut(), point) {
                finger.seen_contact |= !point.new;
                if (finger.x, finger.y) != (point.x, point.y) {
                    (finger.x, finger.y) = (point.x, point.y);
                    emit(TouchEvent::at(TouchEventKind::Drag, finger.id, point));
                }
            }
        }

        for hw_id in 0..MAX_POINTS {
            if self.ignored[hw_id] {
                continue;
            }
            match (self.down[hw_id], frame.points[hw_id]) {
                (None, Some(point)) if self.candidates[hw_id] => {
                    self.candidates[hw_id] = false;
                    let id = self.free_id();
                    self.down[hw_id] = Some(DownFinger { id, x: point.x, y: point.y, seen_contact: !point.new });
                    emit(TouchEvent::at(TouchEventKind::Press, id, &point));
                }
                (None, point) => self.candidates[hw_id] = point.is_some(),
                (Some(_), _) => {}
            }
        }
    }

    /// To be called when a frame couldn't be read, or was invalid.
    pub fn frame_lost(&mut self) { self.frame_lost = true; }

    /// A tracker for a controller that was just reset: it forgets all fingers, and ignores the ones that
    /// are touching until they are lifted.
    pub fn after_reset() -> Self { Self { wait_for_lift: true, ..Self::default() } }

    /// Whether any finger is pressed.
    pub fn has_fingers_down(&self) -> bool { self.down.iter().any(Option::is_some) }

    /// Whether any finger is down or about to be pressed.
    pub fn is_touched(&self) -> bool {
        self.wait_for_lift
            || self.down.iter().any(Option::is_some)
            || self.candidates.contains(&true)
            || self.ignored.contains(&true)
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
    fn missed_lift_is_cancelled_and_the_new_contact_ignored_until_lifted() {
        let mut t = FrameTracker::default();
        run(&mut t, Frame::with(&[(0, 10, 20, true)]));
        run(&mut t, Frame::with(&[(0, 10, 20, false)]));
        // The lift-up frame was lost, and the next frames are a new finger under the same id
        assert_eq!(run2(&mut t, Frame::with(&[(0, 100, 200, true)])), [(Cancel, 0, 10, 20)]);
        assert!(t.is_touched());
        assert_eq!(run(&mut t, Frame::default()), []);
        assert_eq!(run2(&mut t, Frame::with(&[(0, 100, 200, true)])), [(Press, 0, 100, 200)]);

        // A frame was lost, and the finger is gone in the next one
        t.frame_lost();
        assert_eq!(run(&mut t, Frame::default()), [(Cancel, 0, 100, 200)]);
    }

    #[test]
    fn short_jump_across_lost_frames_is_another_finger() {
        let mut t = FrameTracker::default();
        run2(&mut t, Frame::with(&[(0, 100, 400, false)]));
        assert_eq!(run(&mut t, Frame::with(&[(0, 140, 400, false)])), [(Drag, 0, 140, 400)]);
        t.frame_lost();
        assert_eq!(run(&mut t, Frame::with(&[(0, 230, 400, false)])), [(Cancel, 0, 140, 400)]);
        assert_eq!(run2(&mut t, Frame::with(&[(0, 240, 400, false)])), []);
    }

    #[test]
    fn fingers_touching_after_a_reset_are_ignored_until_lifted() {
        let mut t = FrameTracker::after_reset();
        assert!(t.is_touched());
        assert_eq!(run2(&mut t, Frame::with(&[(0, 10, 20, true)])), []);
        assert_eq!(run2(&mut t, Frame::with(&[(0, 15, 20, false), (1, 50, 50, true)])), []);
        assert_eq!(run(&mut t, Frame::default()), []);
        assert!(!t.is_touched());
        assert_eq!(run2(&mut t, Frame::with(&[(1, 50, 50, true)])), [(Press, 0, 50, 50)]);
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
    fn is_touched() {
        let mut t = FrameTracker::default();
        assert!(!t.is_touched());
        run(&mut t, Frame::with(&[(0, 10, 10, true)]));
        assert!(t.is_touched());
        run(&mut t, Frame::with(&[(0, 10, 10, false)]));
        assert!(t.is_touched());
        run(&mut t, Frame::default());
        assert!(!t.is_touched());
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

    #[test]
    fn jump_is_another_finger() {
        let mut t = FrameTracker::default();
        run2(&mut t, Frame::with(&[(0, 10, 400, true)]));
        // A fast but real move is a drag
        assert_eq!(run(&mut t, Frame::with(&[(0, 200, 400, false)])), [(Drag, 0, 200, 400)]);
        // Lift and touch again far away, without us seeing the scans in between
        assert_eq!(run(&mut t, Frame::with(&[(0, 200, 750, false)])), [(Cancel, 0, 200, 400)]);
        assert_eq!(run2(&mut t, Frame::with(&[(0, 200, 750, false)])), []);
    }

    #[test]
    fn repeated_press_down_flag_is_the_same_finger() {
        let mut t = FrameTracker::default();
        run(&mut t, Frame::with(&[(0, 10, 20, true)]));
        assert_eq!(run(&mut t, Frame::with(&[(0, 10, 20, true)])), [(Press, 0, 10, 20)]);
        assert_eq!(run(&mut t, Frame::with(&[(0, 11, 20, true)])), [(Drag, 0, 11, 20)]);
        assert_eq!(run(&mut t, Frame::with(&[(0, 12, 20, false)])), [(Drag, 0, 12, 20)]);
        // Flagged new again after it was reported as a contact: another finger
        assert_eq!(run(&mut t, Frame::with(&[(0, 12, 20, true)])), [(Cancel, 0, 12, 20)]);
    }
}
