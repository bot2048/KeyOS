// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Choice of the alternate setting and sample rate to play at.

use alloc::vec::Vec;

use crate::descriptors::{AudioFunction, SampleRates, StreamingAlt};
use crate::requests::RateRange;
use crate::schedule::{period_microframes, PacketSchedule};

/// Rate every consumer device plays and the one the audio service mixes at.
pub const PREFERRED_RATE: u32 = 48_000;
/// CD rate, the next best thing when a device does not take 48 kHz.
pub const FALLBACK_RATE: u32 = 44_100;

/// Bus speed a device was enumerated at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speed {
    /// 12 Mbit/s: one isochronous service opportunity per 1 ms frame.
    Full,
    /// 480 Mbit/s: one per 125 us microframe.
    High,
}

/// Alternate setting and rate picked by [`choose_output`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputChoice {
    /// Index into [`AudioFunction::outputs`].
    pub output: usize,
    /// Sample rate in Hz.
    pub rate: u32,
}

impl OutputChoice {
    /// The chosen alternate setting.
    pub fn alt<'a>(&self, function: &'a AudioFunction) -> &'a StreamingAlt { &function.outputs[self.output] }
}

/// Rates worth trying on `alt`. `clock_rates` is what the clock source answered to a RANGE
/// request on a UAC2 device; without it 48 kHz and 44.1 kHz are assumed, which every
/// UAC2 headphone adapter seen so far supports.
fn candidate_rates(alt: &StreamingAlt, clock_rates: Option<&[RateRange]>) -> Vec<u32> {
    let mut rates: Vec<u32> = match &alt.rates {
        SampleRates::Discrete(rates) => rates.clone(),
        SampleRates::Continuous { min, max } => {
            let mut rates = alloc::vec![*min, *max];
            rates.extend([PREFERRED_RATE, FALLBACK_RATE].into_iter().filter(|r| (min..=max).contains(&r)));
            rates
        }
        SampleRates::Clock => match clock_rates {
            Some(ranges) => {
                let mut rates: Vec<u32> = [PREFERRED_RATE, FALLBACK_RATE]
                    .into_iter()
                    .filter(|&r| ranges.iter().any(|range| range.contains(r)))
                    .collect();
                rates.extend(ranges.iter().map(|range| range.min));
                rates
            }
            None => alloc::vec![PREFERRED_RATE, FALLBACK_RATE],
        },
    };
    rates.retain(|&r| r > 0);
    rates.sort_unstable();
    rates.dedup();
    rates
}

/// Whether the largest packet `rate` needs fits the endpoint.
pub fn fits(alt: &StreamingAlt, speed: Speed, rate: u32) -> bool {
    let Some(period) = period_microframes(speed, alt.interval) else { return false };
    let frames = PacketSchedule::new(rate, period).max_frames();
    frames * alt.frame_bytes() <= alt.max_payload()
}

/// Pick the playback setting: 16-bit stereo at 48 kHz if the device has it, else 16-bit
/// stereo at 44.1 kHz, else whatever it has, still preferring stereo, the narrowest
/// sample and the rate closest to 48 kHz. Settings whose packets would not fit their
/// endpoint, and sample sizes [`crate::pcm`] cannot write, are left out.
pub fn choose_output(
    function: &AudioFunction,
    speed: Speed,
    clock_rates: Option<&[RateRange]>,
) -> Option<OutputChoice> {
    let mut best: Option<((bool, u8, bool, i8, i64), OutputChoice)> = None;
    for (index, alt) in function.outputs.iter().enumerate() {
        if !(2..=4).contains(&alt.subframe_size) {
            continue;
        }
        let sixteen_bit_stereo = alt.channels == 2 && alt.subframe_size == 2 && alt.bit_resolution == 16;
        for rate in candidate_rates(alt, clock_rates) {
            if !fits(alt, speed, rate) {
                continue;
            }
            let rate_rank = match rate {
                PREFERRED_RATE => 2,
                FALLBACK_RATE => 1,
                _ => 0,
            };
            let key = (
                sixteen_bit_stereo,
                rate_rank,
                alt.channels == 2,
                -(alt.subframe_size as i8),
                -(rate as i64 - PREFERRED_RATE as i64).abs(),
            );
            if best.as_ref().map_or(true, |(best_key, _)| key > *best_key) {
                best = Some((key, OutputChoice { output: index, rate }));
            }
        }
    }
    best.map(|(_, choice)| choice)
}
