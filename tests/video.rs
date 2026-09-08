//! Video between two apps: a test pattern in, decoded frames out, into an `Image`.

use std::time::{Duration, Instant};

use bevy::{asset::AssetPlugin, prelude::*};
use bevy_iroh::{
    media::{MicrophoneChoice, SpeakerChoice},
    prelude::*,
};

fn app(name: &str) -> App {
    let mut app = App::new();
    app.insert_resource(MediaSettings {
        microphone: MicrophoneChoice::None,
        speaker: SpeakerChoice::None,
        ..default()
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
            start.elapsed() < Duration::from_secs(30),
            "gave up waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn image_data(app: &mut App) -> Option<(u32, u32, Vec<u8>)> {
    let handle = app
        .world_mut()
        .query_filtered::<&VideoImage, With<Remote>>()
        .iter(app.world())
        .next()?
        .0
        .clone();
    let images = app.world().resource::<Assets<Image>>();
    let img = images.get(&handle)?;
    Some((img.width(), img.height(), img.data.clone()?))
}

#[test]
fn a_test_pattern_crosses_between_two_apps() {
    let mut alice = app("alice");
    let mut bob = app("bob");

    alice.world_mut().spawn(Room::host("video"));
    alice.world_mut().spawn((
        VideoFeed::new(160, 120),
        VideoInput::new(TestPattern::new(160, 120, 15.0)),
        Shared::default(),
        Transform::default(),
    ));
    // Bob publishes too, as every conference participant does.
    bob.world_mut().spawn((
        VideoFeed::new(320, 240),
        VideoInput::new(TestPattern::new(320, 240, 15.0)),
        Shared::default(),
        Transform::default(),
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

    wait(&mut alice, &mut bob, "the feed entity", |b| {
        b.world_mut()
            .query_filtered::<&VideoFeed, With<Remote>>()
            .iter(b.world())
            .next()
            .is_some_and(|f| f.width == 160 && f.height == 120)
    });

    // A picture arrives at the right size, and it is a picture: bars, not one flat colour.
    wait(&mut alice, &mut bob, "the first frame", |b| {
        image_data(b).is_some()
    });
    let (w, h, first) = image_data(&mut bob).unwrap();
    assert_eq!((w, h), (160, 120));
    assert_eq!(first.len(), 160 * 120 * 4);
    let distinct = first
        .chunks(4)
        .map(|p| p[0] / 32)
        .collect::<std::collections::HashSet<_>>();
    assert!(distinct.len() >= 3, "only {} shades", distinct.len());

    // And it moves.
    wait(&mut alice, &mut bob, "a later frame", |b| {
        image_data(b).is_some_and(|(_, _, data)| data != first)
    });

    // And Alice sees Bob's.
    wait(&mut bob, &mut alice, "bob's picture on alice", |a| {
        image_data(a).is_some_and(|(w, h, _)| (w, h) == (320, 240))
    });

    // Removing the feed on Alice's side removes it on Bob's.
    let feed = alice
        .world_mut()
        .query_filtered::<Entity, (With<VideoFeed>, Without<Remote>)>()
        .single(alice.world())
        .unwrap();
    alice.world_mut().entity_mut(feed).remove::<VideoFeed>();
    wait(&mut alice, &mut bob, "the feed to go", |b| {
        b.world_mut()
            .query_filtered::<&VideoFeed, With<Remote>>()
            .iter(b.world())
            .next()
            .is_none()
    });
}
