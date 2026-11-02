// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! How many audio frames go into each isochronous packet.
//!
//! A rate that is not a multiple of the packet rate cannot send the same number of frames
//! every time: 44.1 kHz at full speed sends nine packets of 44 frames and one of 45 every
//! 10 ms. The schedule keeps the remainder exactly, in units of microframes, so the long
//! term rate is the nominal one without drift.

use crate::select::Speed;

const MICROFRAMES_PER_SECOND: u64 = 8000;
const MICROFRAMES_PER_FRAME: u32 = 8;
/// USB 2.0 9.6.6: isochronous bInterval runs from 1 to 16, a period of 2^(bInterval-1).
const MAX_INTERVAL_EXPONENT: u32 = 15;

/// Service period of an isochronous endpoint in 125 us microframes, or `None` for an
/// interval the endpoint descriptor may not hold. A bInterval of 0, which some UAC1
/// devices report, is read as 1, the only value full-speed UAC1 allows.
pub fn period_microframes(speed: Speed, interval: u8) -> Option<u32> {
    let exponent = (interval.max(1) - 1) as u32;
    if exponent > MAX_INTERVAL_EXPONENT {
        return None;
    }
    let unit = match speed {
        Speed::Full => MICROFRAMES_PER_FRAME,
        Speed::High => 1,
    };
    Some(unit << exponent)
}

/// Frames per packet for a stream at a fixed rate.
#[derive(Debug, Clone)]
pub struct PacketSchedule {
    rate: u32,
    period: u32,
    remainder: u64,
}

impl PacketSchedule {
    /// A schedule for `rate` Hz with one packet every `period` microframes (see
    /// [`period_microframes`]).
    pub fn new(rate: u32, period: u32) -> Self { Self { rate, period: period.max(1), remainder: 0 } }

    /// Frames in the next packet.
    pub fn next_frames(&mut self) -> usize {
        self.remainder += self.rate as u64 * self.period as u64;
        let frames = self.remainder / MICROFRAMES_PER_SECOND;
        self.remainder -= frames * MICROFRAMES_PER_SECOND;
        frames as usize
    }

    /// Frames in the largest packet the schedule ever produces.
    pub fn max_frames(&self) -> usize {
        (self.rate as u64 * self.period as u64).div_ceil(MICROFRAMES_PER_SECOND) as usize
    }

    /// Frames in the smallest packet the schedule ever produces.
    pub fn min_frames(&self) -> usize {
        (self.rate as u64 * self.period as u64 / MICROFRAMES_PER_SECOND) as usize
    }

    /// Packets sent each second.
    pub fn packets_per_second(&self) -> u32 { (MICROFRAMES_PER_SECOND / self.period as u64) as u32 }

    /// Sample rate the schedule was made for.
    pub fn rate(&self) -> u32 { self.rate }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use super::*;

    fn take(schedule: &mut PacketSchedule, n: usize) -> Vec<usize> {
        (0..n).map(|_| schedule.next_frames()).collect()
    }

    #[test]
    fn full_speed_44100_is_nine_44s_and_a_45() {
        let mut s = PacketSchedule::new(44_100, period_microframes(Speed::Full, 1).unwrap());
        let ten = take(&mut s, 10);
        assert_eq!(ten, [44, 44, 44, 44, 44, 44, 44, 44, 44, 45]);
        assert_eq!(take(&mut s, 1000).iter().sum::<usize>(), 44_100);
        assert_eq!((s.min_frames(), s.max_frames()), (44, 45));
    }

    #[test]
    fn full_speed_48000_is_flat() {
        let mut s = PacketSchedule::new(48_000, period_microframes(Speed::Full, 1).unwrap());
        assert!(take(&mut s, 100).iter().all(|&n| n == 48));
        assert_eq!(s.max_frames(), 48);
    }

    #[test]
    fn high_speed_every_microframe() {
        let mut s = PacketSchedule::new(48_000, period_microframes(Speed::High, 1).unwrap());
        assert!(take(&mut s, 80).iter().all(|&n| n == 6));
        let mut s = PacketSchedule::new(44_100, period_microframes(Speed::High, 1).unwrap());
        let one_second = take(&mut s, 8000);
        assert_eq!(one_second.iter().sum::<usize>(), 44_100);
        assert!(one_second.iter().all(|&n| n == 5 || n == 6));
        assert_eq!(s.packets_per_second(), 8000);
    }

    #[test]
    fn high_speed_interval_4_is_one_packet_per_millisecond() {
        let period = period_microframes(Speed::High, 4).unwrap();
        assert_eq!(period, 8);
        let mut s = PacketSchedule::new(44_100, period);
        assert_eq!(take(&mut s, 10), [44, 44, 44, 44, 44, 44, 44, 44, 44, 45]);
    }

    #[test]
    fn odd_rates_do_not_drift() {
        for rate in [8_000, 11_025, 22_050, 32_000, 88_200, 96_000, 176_400, 192_000] {
            for (speed, packets) in [(Speed::Full, 1000), (Speed::High, 8000)] {
                let mut s = PacketSchedule::new(rate, period_microframes(speed, 1).unwrap());
                assert_eq!(take(&mut s, packets).iter().sum::<usize>(), rate as usize, "{rate} {speed:?}");
            }
        }
    }

    #[test]
    fn interval_bounds() {
        assert_eq!(period_microframes(Speed::Full, 0), Some(8));
        assert_eq!(period_microframes(Speed::High, 16), Some(1 << 15));
        assert_eq!(period_microframes(Speed::High, 17), None);
    }
}
