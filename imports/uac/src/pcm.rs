// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Conversion of 16-bit stereo PCM into a device's stream format, and software volume.
//!
//! Type I PCM puts the significant bits of a sample at the top of its subslot (UAC2
//! Formats 2.3.1.1), so a 16-bit sample goes into a wider subslot shifted left, and a
//! device with fewer significant bits ignores the low ones.

use crate::descriptors::StreamingAlt;

/// Layout of the frames a device takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamFormat {
    /// Channels per frame.
    pub channels: u8,
    /// Bytes per sample: 2, 3 or 4.
    pub subframe_size: u8,
}

impl StreamFormat {
    /// The format of an alternate setting.
    pub fn of(alt: &StreamingAlt) -> Self { Self { channels: alt.channels, subframe_size: alt.subframe_size } }

    /// Bytes per frame.
    pub fn frame_bytes(&self) -> usize { self.channels as usize * self.subframe_size as usize }
}

fn put_sample(sample: i16, subframe_size: u8, out: &mut [u8]) {
    match subframe_size {
        2 => out[..2].copy_from_slice(&sample.to_le_bytes()),
        3 => out[..3].copy_from_slice(&((sample as i32) << 8).to_le_bytes()[..3]),
        4 => out[..4].copy_from_slice(&((sample as i32) << 16).to_le_bytes()),
        _ => out[..subframe_size as usize].fill(0),
    }
}

/// Write stereo frames into `out` in `format`, as many as fit. A mono device gets the
/// average of both channels, a device with more than two gets left and right in its
/// first two and silence in the rest. Returns the number of bytes written.
pub fn encode_stereo(frames: &[[i16; 2]], format: StreamFormat, out: &mut [u8]) -> usize {
    let frame_bytes = format.frame_bytes();
    if frame_bytes == 0 {
        return 0;
    }
    let size = format.subframe_size;
    let mut written = 0;
    for (frame, chunk) in frames.iter().zip(out.chunks_exact_mut(frame_bytes)) {
        if format.channels == 1 {
            put_sample(((frame[0] as i32 + frame[1] as i32) / 2) as i16, size, chunk);
        } else {
            for (channel, slot) in chunk.chunks_exact_mut(size as usize).enumerate() {
                put_sample(frame.get(channel).copied().unwrap_or(0), size, slot);
            }
        }
        written += frame_bytes;
    }
    written
}

/// Q15 gain for a volume in percent. The curve is quadratic, closer to how loudness is
/// heard than a linear one: 50 % is about -12 dB.
pub fn gain_for_volume(percent: u8) -> u16 {
    let p = percent.min(100) as u32;
    (p * p * 32768 / 10_000) as u16
}

/// Scale frames in place by a Q15 gain; 32768 leaves them untouched.
pub fn apply_gain(frames: &mut [[i16; 2]], gain: u16) {
    if gain >= 32768 {
        return;
    }
    for frame in frames {
        for sample in frame {
            *sample = ((*sample as i32 * gain as i32) >> 15) as i16;
        }
    }
}

/// Converts a stream of stereo frames from one rate to another by linear interpolation:
/// good enough for tones, speech and notifications, which is what the device plays. Equal
/// rates pass through untouched.
#[derive(Debug, Clone)]
pub struct Resampler {
    from: u32,
    to: u32,
    /// Position between `previous` and `current`, in units of 1/`to` of an input frame.
    phase: u32,
    previous: [i16; 2],
    current: [i16; 2],
}

impl Resampler {
    /// A resampler from `from` Hz to `to` Hz.
    pub fn new(from: u32, to: u32) -> Self {
        Self { from, to: to.max(1), phase: to.max(1), previous: [0; 2], current: [0; 2] }
    }

    /// The next output frame, taking input frames from `pull` as they are needed.
    pub fn next_frame(&mut self, mut pull: impl FnMut() -> [i16; 2]) -> [i16; 2] {
        if self.from == self.to {
            return pull();
        }
        while self.phase >= self.to {
            self.previous = self.current;
            self.current = pull();
            self.phase -= self.to;
        }
        let t = self.phase as i64;
        let mut out = [0i16; 2];
        for (channel, sample) in out.iter_mut().enumerate() {
            let a = self.previous[channel] as i64;
            let b = self.current[channel] as i64;
            *sample = (a + (b - a) * t / self.to as i64) as i16;
        }
        self.phase += self.from;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_the_rate() {
        let mut r = Resampler::new(48_000, 44_100);
        let mut pulled = 0usize;
        for _ in 0..44_100 {
            r.next_frame(|| {
                pulled += 1;
                [0; 2]
            });
        }
        // One second out takes one second in, give or take the frame in hand.
        assert!((47_999..=48_001).contains(&pulled), "{pulled}");

        let mut r = Resampler::new(48_000, 48_000);
        let mut n = 0i16;
        let out: alloc::vec::Vec<_> = (0..4)
            .map(|_| {
                r.next_frame(|| {
                    n += 1;
                    [n, -n]
                })
            })
            .collect();
        assert_eq!(out, [[1, -1], [2, -2], [3, -3], [4, -4]]);
    }

    #[test]
    fn resampling_interpolates() {
        // A ramp stays a ramp, one input frame late: output starts from silence.
        let mut r = Resampler::new(2, 3);
        let mut n = 0i16;
        let out: alloc::vec::Vec<i16> = (0..7)
            .map(|_| {
                r.next_frame(|| {
                    n += 300;
                    [n, n]
                })[0]
            })
            .collect();
        assert_eq!(out, [0, 200, 400, 600, 800, 1000, 1200]);
    }

    const FRAMES: [[i16; 2]; 2] = [[0x1234, -2], [i16::MIN, i16::MAX]];

    #[test]
    fn sixteen_bit_stereo_is_a_copy() {
        let mut out = [0u8; 8];
        let n = encode_stereo(&FRAMES, StreamFormat { channels: 2, subframe_size: 2 }, &mut out);
        assert_eq!(n, 8);
        assert_eq!(out, [0x34, 0x12, 0xfe, 0xff, 0x00, 0x80, 0xff, 0x7f]);
    }

    #[test]
    fn wider_subslots_are_msb_aligned() {
        let mut out = [0u8; 6];
        encode_stereo(&FRAMES[..1], StreamFormat { channels: 2, subframe_size: 3 }, &mut out);
        assert_eq!(out, [0x00, 0x34, 0x12, 0x00, 0xfe, 0xff]);
        let mut out = [0u8; 8];
        encode_stereo(&FRAMES[..1], StreamFormat { channels: 2, subframe_size: 4 }, &mut out);
        assert_eq!(out, [0, 0, 0x34, 0x12, 0, 0, 0xfe, 0xff]);
    }

    #[test]
    fn mono_and_multichannel() {
        let mut out = [0u8; 2];
        encode_stereo(&[[100, 300]], StreamFormat { channels: 1, subframe_size: 2 }, &mut out);
        assert_eq!(i16::from_le_bytes(out), 200);
        let mut out = [0xaau8; 8];
        encode_stereo(&[[1, 2]], StreamFormat { channels: 4, subframe_size: 2 }, &mut out);
        assert_eq!(out, [1, 0, 2, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn short_output_takes_whole_frames_only() {
        let mut out = [0u8; 7];
        assert_eq!(encode_stereo(&FRAMES, StreamFormat { channels: 2, subframe_size: 2 }, &mut out), 4);
    }

    #[test]
    fn volume() {
        assert_eq!(gain_for_volume(0), 0);
        assert_eq!(gain_for_volume(100), 32768);
        assert_eq!(gain_for_volume(50), 8192);
        let mut frames = [[1000i16, -1000]];
        apply_gain(&mut frames, gain_for_volume(50));
        assert_eq!(frames, [[250, -250]]);
    }
}
