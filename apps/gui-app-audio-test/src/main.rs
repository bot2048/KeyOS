// SPDX-FileCopyrightText: 2026 Arpan Jain
// SPDX-License-Identifier: GPL-3.0-or-later

//! Audio Test: plays a 440 Hz sine through `os/audio`, to check a USB audio device by ear.

use std::cell::OnceCell;
use std::f32::consts::TAU;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use audio::AudioError;
use slint_keyos_platform::{app, gui_server_api::InputMessage};

audio::use_api!();

thread_local! {
    static UI: OnceCell<AppWindow> = OnceCell::new();
}

app!("Audio Test");

const TONE_HZ: f32 = 440.0;
/// Frames generated and queued at a time: 20 ms.
const CHUNK_FRAMES: usize = audio::SAMPLE_RATE as usize / 50;
/// A little under full scale, so the tone does not clip after resampling.
const AMPLITUDE: f32 = 0.8 * i16::MAX as f32;
const VOLUMES: [u8; 4] = [25, 50, 75, 100];

fn app_main(cx: AppContext, ui: AppWindow) {
    log_server::init_wait(env!("CARGO_CRATE_NAME")).unwrap();
    log::set_max_level(log::LevelFilter::Info);

    UI.with(|slot| {
        slot.set(ui.clone_strong()).ok();
    });

    let playing = Arc::new(AtomicBool::new(false));

    cx.set_input_handler({
        let playing = playing.clone();
        move |input| {
            if input.msg == InputMessage::Hidden {
                playing.store(false, Ordering::SeqCst);
            }
        }
    });

    ui.global::<Callbacks>().on_play({
        let playing = playing.clone();
        move || {
            if playing.swap(true, Ordering::SeqCst) {
                return;
            }
            let volume =
                VOLUMES[UI.with(|ui| ui.get().unwrap().global::<State>().get_volume_index()) as usize % 4];
            let playing = playing.clone();
            std::thread::spawn(move || play_tone(playing, volume));
        }
    });

    ui.global::<Callbacks>().on_stop({
        let playing = playing.clone();
        move || playing.store(false, Ordering::SeqCst)
    });

    ui.global::<Callbacks>().on_volume_changed(|index| {
        let volume = VOLUMES[index.clamp(0, 3) as usize];
        std::thread::spawn(move || AudioApi::default().set_volume(volume));
    });

    ui.run().expect("UI running");
}

fn play_tone(playing: Arc<AtomicBool>, volume: u8) {
    let mut api = AudioApi::default();
    let status = api.status();
    if !status.connected {
        playing.store(false, Ordering::SeqCst);
        show(false, "No USB audio device. Connect one and try again.".into());
        return;
    }
    api.set_volume(volume);
    let bus = if status.full_speed { "full speed" } else { "high speed" };
    show(true, format!("Playing 440 Hz, device at {} Hz ({bus})", status.device_rate));

    let step = TAU * TONE_HZ / audio::SAMPLE_RATE as f32;
    let mut phase = 0f32;
    let mut chunk = vec![[0i16; 2]; CHUNK_FRAMES];
    let mut result = Ok(());
    while playing.load(Ordering::SeqCst) {
        for frame in &mut chunk {
            let sample = (phase.sin() * AMPLITUDE) as i16;
            *frame = [sample, sample];
            phase = (phase + step) % TAU;
        }
        result = api.play(&chunk);
        if result.is_err() {
            break;
        }
    }
    api.stop();
    playing.store(false, Ordering::SeqCst);
    match result {
        Ok(()) => show(false, "Stopped".into()),
        Err(AudioError::NotConnected) => show(false, "The audio device went away.".into()),
        Err(e) => show(false, format!("Playback failed: {e}")),
    }
}

fn show(playing: bool, status: String) {
    slint_keyos_platform::spawn(async move {
        UI.with(|ui| {
            let state = ui.get().expect("UI initialized").global::<State>();
            state.set_playing(playing);
            state.set_status_text(status.into());
        });
    })
    .detach();
}
