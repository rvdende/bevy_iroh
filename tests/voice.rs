//! Voice between two apps: a tone in on one side, a buffer out on the other.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bevy::{asset::AssetPlugin, prelude::*};
use bevy_iroh::{
    media::{AudioOutput, AudioSource, MicrophoneChoice, Mixer, SpeakerChoice},
    prelude::*,
};

/// A 440 Hz tone, produced at real time so the encoder paces like a microphone.
struct Tone {
    started: Instant,
    produced: u64,
}

impl AudioSource for Tone {
    fn sample_rate(&self) -> u32 {
        48_000
    }

    fn read(&mut self, out: &mut [f32]) -> usize {
        let due = (self.started.elapsed().as_secs_f64() * 48_000.0) as u64;
        let n = (due.saturating_sub(self.produced) as usize).min(out.len());
        for (i, s) in out.iter_mut().take(n).enumerate() {
            let t = (self.produced + i as u64) as f32 / 48_000.0;
            *s = 0.5 * (t * 440.0 * std::f32::consts::TAU).sin();
        }
        self.produced += n as u64;
        n
    }
}

/// Pulls the mix every 10 ms into a shared buffer.
struct Capture(Arc<Mutex<Vec<f32>>>);

impl AudioOutput for Capture {
    fn start(
        self: Box<Self>,
        mixer: Arc<Mutex<Mixer>>,
        stop: Arc<AtomicBool>,
    ) -> Result<(), String> {
        let sink = self.0;
        std::thread::spawn(move || {
            let mut buf = vec![0f32; 480 * 2];
            while !stop.load(Ordering::Relaxed) {
                mixer.lock().unwrap().render(&mut buf, 2, 48_000);
                sink.lock().unwrap().extend_from_slice(&buf);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(())
    }
}

fn app(name: &str, settings: MediaSettings) -> App {
    let mut app = App::new();
    app.insert_resource(settings)
        .add_plugins((
            MinimalPlugins,
            AssetPlugin::default(),
            IrohPlugin::default()
                .with_relays(Relays::Disabled)
                .with_display_name(name),
        ))
        .init_asset::<Image>();
    app
}

fn wait(a: &mut App, b: &mut App, what: &str, mut pred: impl FnMut(&mut App) -> bool) {
    let start = Instant::now();
    loop {
        a.update();
        b.update();
        if pred(b) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "gave up waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(16));
    }
}

#[test]
fn a_tone_crosses_between_two_apps() {
    let heard = Arc::new(Mutex::new(Vec::<f32>::new()));
    let mut alice = app(
        "alice",
        MediaSettings {
            microphone: MicrophoneChoice::custom(Tone {
                started: Instant::now(),
                produced: 0,
            }),
            speaker: SpeakerChoice::None,
            ..default()
        },
    );
    let mut bob = app(
        "bob",
        MediaSettings {
            microphone: MicrophoneChoice::None,
            speaker: SpeakerChoice::custom(Capture(heard.clone())),
            ..default()
        },
    );

    alice.world_mut().spawn(Room::host("voice"));
    // Voice and a picture on one entity, as a conference participant would have.
    alice.world_mut().spawn((
        Voice::default(),
        VideoFeed::new(64, 48),
        VideoInput::new(TestPattern::new(64, 48, 10.0)),
        Shared::default(),
        Transform::from_xyz(1.0, 0.0, 0.0),
    ));
    let mut none = App::new();
    wait(&mut none, &mut alice, "a ticket", |a| {
        a.world_mut()
            .query::<&Ticket>()
            .iter(a.world())
            .next()
            .is_some()
    });
    let ticket = alice
        .world_mut()
        .query::<&Ticket>()
        .single(alice.world())
        .unwrap()
        .0
        .clone();
    bob.world_mut().spawn(Room::join(ticket));

    wait(&mut alice, &mut bob, "the voice entity", |b| {
        b.world_mut()
            .query_filtered::<&Voice, With<Remote>>()
            .iter(b.world())
            .next()
            .is_some()
    });

    // Sound arrives: the last quarter second of the mix is not silence.
    wait(&mut alice, &mut bob, "sound", |_| {
        let heard = heard.lock().unwrap();
        let tail = &heard[heard.len().saturating_sub(48_000 / 2)..];
        tail.len() >= 48_000 / 4 && rms(tail) > 0.05
    });
    let level = bob
        .world_mut()
        .query_filtered::<&VoiceLevel, With<Remote>>()
        .single(bob.world())
        .unwrap()
        .0;
    assert!(level > 0.05, "level {level}");

    // The picture arrives alongside the sound.
    wait(&mut alice, &mut bob, "the picture", |b| {
        b.world_mut()
            .query_filtered::<&VideoImage, With<Remote>>()
            .iter(b.world())
            .next()
            .is_some()
    });

    // Muting stops it.
    let mine = alice
        .world_mut()
        .query_filtered::<Entity, With<Voice>>()
        .single(alice.world())
        .unwrap();
    alice.world_mut().get_mut::<Voice>(mine).unwrap().muted = true;
    wait(&mut alice, &mut bob, "silence after mute", |b| {
        b.world_mut()
            .query_filtered::<&Voice, With<Remote>>()
            .single(b.world())
            .is_ok_and(|v| v.muted)
    });
    let before = heard.lock().unwrap().len();
    wait(&mut alice, &mut bob, "half a second of mix", |_| {
        heard.lock().unwrap().len() >= before + 48_000
    });
    let heard = heard.lock().unwrap();
    let tail = &heard[heard.len() - 48_000 / 2..];
    assert!(rms(tail) < 0.01, "still hearing {}", rms(tail));
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}
