//! Voice between two apps over a WebRTC data channel rather than QUIC: one side is told to
//! offer, as a browser would, and the other answers. Host candidates only, so this stays on
//! the machine.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use bevy::{asset::AssetPlugin, prelude::*};
use bevy_iroh::{
    media::{AudioOutput, AudioSource, MicrophoneChoice, Mixer, Running, SpeakerChoice},
    prelude::*,
};

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

struct Capture(Arc<Mutex<Vec<f32>>>);

impl AudioOutput for Capture {
    fn start(self: Box<Self>, mixer: Arc<Mutex<Mixer>>) -> Result<Running, String> {
        let sink = self.0;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        std::thread::spawn(move || {
            let mut buf = vec![0f32; 480 * 2];
            while !flag.load(Ordering::Relaxed) {
                mixer.lock().unwrap().render(&mut buf, 2, 48_000);
                sink.lock().unwrap().extend_from_slice(&buf);
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        Ok(Running::flag(stop))
    }
}

fn app(name: &str, settings: MediaSettings, initiate: bool) -> App {
    let mut app = App::new();
    app.insert_resource(settings)
        .insert_resource(RtcSettings {
            stun: Vec::new(),
            initiate,
            enabled: true,
        })
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
            eprintln!("{what}: {:?}", start.elapsed());
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(40),
            "gave up waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(16));
    }
}

#[test]
fn a_tone_crosses_a_data_channel() {
    let heard = Arc::new(Mutex::new(Vec::<f32>::new()));
    // Alice speaks and answers offers, like a desktop. Bob listens and offers, like a page.
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
        false,
    );
    let mut bob = app(
        "bob",
        MediaSettings {
            microphone: MicrophoneChoice::None,
            speaker: SpeakerChoice::custom(Capture(heard.clone())),
            ..default()
        },
        true,
    );

    alice.world_mut().spawn(Room::host("webrtc"));
    alice.world_mut().spawn((
        Voice::default(),
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

    // The link comes up on both sides.
    wait(&mut alice, &mut bob, "a webrtc link on bob", |b| {
        b.world_mut()
            .query_filtered::<&MediaPath, With<Peer>>()
            .iter(b.world())
            .any(|p| *p == MediaPath::WebRtc)
    });
    wait(&mut bob, &mut alice, "a webrtc link on alice", |a| {
        a.world_mut()
            .query_filtered::<&MediaPath, With<Peer>>()
            .iter(a.world())
            .any(|p| *p == MediaPath::WebRtc)
    });

    // And the voice entity says its media goes that way, and sound arrives.
    wait(&mut alice, &mut bob, "the voice over webrtc", |b| {
        b.world_mut()
            .query_filtered::<&MediaPath, (With<Voice>, With<Remote>)>()
            .iter(b.world())
            .any(|p| *p == MediaPath::WebRtc)
    });
    // A second of frames over the link, and the mix is not silence.
    let received = |b: &mut App| {
        b.world_mut()
            .query_filtered::<&VoiceStats, With<Remote>>()
            .iter(b.world())
            .next()
            .map(|s| s.received)
            .unwrap_or(0)
    };
    let before = received(&mut bob);
    let mark = heard.lock().unwrap().len();
    wait(
        &mut alice,
        &mut bob,
        "a second of frames over webrtc",
        |b| received(b) >= before + 50,
    );
    wait(&mut alice, &mut bob, "sound over webrtc", |_| {
        let heard = heard.lock().unwrap();
        let fresh = &heard[mark.max(heard.len().saturating_sub(48_000 / 2))..];
        fresh.len() >= 48_000 / 4 && rms(fresh) > 0.05
    });
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}
