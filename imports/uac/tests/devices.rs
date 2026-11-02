// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Parsing and format selection on descriptor dumps of real devices.
//!
//! Sources of the dumps in `tests/dumps`, all `lsusb -v` output posted publicly:
//! - apple-usb-c-05ac-110a: github.com/Dunedan/mbp-2016-linux/issues/94 (comment 466390999)
//! - cmedia-audio-adapter-0d8c-000c, cmedia-cm108-mic-0d8c-013c: gists by matoken
//! - cmedia-cm6631-0d8c-0309: gist by hegge
//! - cmedia-schiit-0d8c-0304: gist by andrewdunndev
//! - audioquest-dragonfly-21b4-0083: github.com/raspberrypi/linux/issues/3962
//! - asus-xonar-u3-0b05-17a0: github.com/raspberrypi/linux/issues/3962 (comments)

mod lsusb;

use uac::descriptors::SyncType;
use uac::pcm::StreamFormat;
use uac::*;

fn load(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/dumps/{name}.txt", env!("CARGO_MANIFEST_DIR"));
    lsusb::parse(&std::fs::read_to_string(path).unwrap(), 0).bytes
}

fn function(name: &str) -> AudioFunction { parse_audio_function(&load(name)).unwrap() }

/// Frames per packet over one second of the chosen stream.
fn second_of_packets(alt: &StreamingAlt, speed: Speed, rate: u32) -> Vec<usize> {
    let period = schedule::period_microframes(speed, alt.interval).unwrap();
    let mut s = PacketSchedule::new(rate, period);
    (0..s.packets_per_second()).map(|_| s.next_frames()).collect()
}

#[test]
fn apple_usb_c_adapter_is_full_speed_uac2() {
    let f = function("apple-usb-c-05ac-110a");
    assert_eq!(f.version, UacVersion::Uac2);
    assert_eq!(f.control_interface, 0);
    assert_eq!(f.outputs.len(), 2);

    let choice = choose_output(&f, Speed::Full, None).unwrap();
    assert_eq!(choice.rate, 48_000);
    let alt = choice.alt(&f);
    assert_eq!((alt.interface, alt.alt_setting), (1, 2));
    assert_eq!((alt.channels, alt.subframe_size, alt.bit_resolution), (2, 2, 16));
    assert_eq!((alt.endpoint_address, alt.max_packet_size, alt.interval), (0x02, 192, 1));
    assert_eq!(alt.sync, SyncType::Synchronous);
    assert_eq!(alt.clock_source, Some(9));
    let unit = alt.feature_unit.as_ref().unwrap();
    assert_eq!(unit.unit_id, 2);
    assert!(unit.master_mute && !unit.master_volume);
    assert_eq!(unit.channel_volume, [1, 2]);

    // 48 frames of 4 bytes fill the 192 byte packet exactly.
    assert!(second_of_packets(alt, Speed::Full, 48_000).iter().all(|&n| n * 4 == 192));
    // The clock may only offer 44.1 kHz; that still fits.
    let only_cd = [requests::RateRange { min: 44_100, max: 44_100, res: 0 }];
    assert_eq!(choose_output(&f, Speed::Full, Some(&only_cd)).unwrap(), OutputChoice { output: 1, rate: 44_100 });
}

#[test]
fn cmedia_adapter_is_full_speed_uac1() {
    let f = function("cmedia-audio-adapter-0d8c-000c");
    assert_eq!(f.version, UacVersion::Uac1);
    let choice = choose_output(&f, Speed::Full, None).unwrap();
    assert_eq!(choice, OutputChoice { output: 0, rate: 48_000 });
    let alt = choice.alt(&f);
    assert_eq!(alt.rates, SampleRates::Discrete(vec![48_000, 44_100]));
    assert_eq!((alt.interface, alt.alt_setting, alt.endpoint_address), (1, 1, 0x01));
    assert_eq!(alt.sync, SyncType::Adaptive);
    assert!(alt.sampling_frequency_control);
    assert_eq!(alt.clock_source, None);
    assert_eq!(alt.feature_unit.as_ref().unwrap().unit_id, 9);

    let packets = second_of_packets(alt, Speed::Full, 44_100);
    assert_eq!(packets.iter().sum::<usize>(), 44_100);
    assert_eq!(&packets[..10], &[44, 44, 44, 44, 44, 44, 44, 44, 44, 45]);
}

#[test]
fn cm108_capture_only_has_no_output() {
    assert_eq!(parse_audio_function(&load("cmedia-cm108-mic-0d8c-013c")), Err(ParseError::NoOutput));
}

#[test]
fn cm6631_is_high_speed_uac2_with_feedback() {
    let f = function("cmedia-cm6631-0d8c-0309");
    assert_eq!(f.version, UacVersion::Uac2);
    assert_eq!(f.outputs.len(), 3);
    let choice = choose_output(&f, Speed::High, None).unwrap();
    assert_eq!(choice, OutputChoice { output: 0, rate: 48_000 });
    let alt = choice.alt(&f);
    assert_eq!((alt.subframe_size, alt.bit_resolution, alt.endpoint_address), (2, 16, 0x05));
    assert_eq!(alt.sync, SyncType::Asynchronous);
    assert!(alt.feedback_endpoint.is_some());
    assert!(alt.clock_source.is_some());
    // One packet per microframe: six frames of four bytes.
    assert!(second_of_packets(alt, Speed::High, 48_000).iter().all(|&n| n == 6));
}

#[test]
fn schiit_has_two_outputs_and_picks_the_first_16_bit_one() {
    let f = function("cmedia-schiit-0d8c-0304");
    assert_eq!(f.outputs.len(), 4);
    assert_eq!(f.outputs.iter().map(|a| a.interface).collect::<Vec<_>>(), [1, 1, 2, 2]);
    let choice = choose_output(&f, Speed::High, None).unwrap();
    assert_eq!(choice, OutputChoice { output: 0, rate: 48_000 });
    // Feature units sit on different paths.
    assert_ne!(f.outputs[0].feature_unit, f.outputs[2].feature_unit);
}

#[test]
fn dragonfly_only_has_24_bit() {
    let f = function("audioquest-dragonfly-21b4-0083");
    let choice = choose_output(&f, Speed::Full, None).unwrap();
    assert_eq!(choice, OutputChoice { output: 0, rate: 48_000 });
    let alt = choice.alt(&f);
    assert_eq!((alt.channels, alt.subframe_size, alt.bit_resolution), (2, 3, 24));
    assert_eq!(alt.rates, SampleRates::Discrete(vec![44_100, 48_000, 88_200, 96_000]));
    assert_eq!(alt.sync, SyncType::Asynchronous);

    let mut packet = vec![0u8; alt.max_payload()];
    let frames = vec![[0x0102i16, -1]; 48];
    let n = pcm::encode_stereo(&frames, StreamFormat::of(alt), &mut packet);
    assert_eq!(n, 48 * 6);
    assert_eq!(&packet[..6], &[0x00, 0x02, 0x01, 0x00, 0xff, 0xff]);
}

#[test]
fn xonar_u3_prefers_48k_over_its_other_rates() {
    let f = function("asus-xonar-u3-0b05-17a0");
    let choice = choose_output(&f, Speed::Full, None).unwrap();
    assert_eq!(choice, OutputChoice { output: 0, rate: 48_000 });
    assert_eq!(choice.alt(&f).rates, SampleRates::Discrete(vec![8_000, 16_000, 44_100, 48_000]));
}

#[test]
fn a_full_speed_endpoint_too_small_for_48k_falls_back() {
    let mut f = function("cmedia-audio-adapter-0d8c-000c");
    // 45 frames of 4 bytes: room for 44.1 kHz, not for 48 kHz.
    f.outputs[0].max_packet_size = 180;
    assert_eq!(choose_output(&f, Speed::Full, None).unwrap().rate, 44_100);
    f.outputs[0].max_packet_size = 100;
    assert_eq!(choose_output(&f, Speed::Full, None), None);
}

#[test]
fn truncated_descriptors_do_not_panic() {
    let bytes = load("apple-usb-c-05ac-110a");
    for len in 0..bytes.len() {
        let _ = parse_audio_function(&bytes[..len]);
    }
    // The 64 bytes the enumeration code used to fetch are not enough to find the stream.
    assert!(parse_audio_function(&bytes[..18 + 64]).is_err());
}
