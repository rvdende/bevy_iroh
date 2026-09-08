//! Two apps in one process, over real iroh, no relay.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_iroh::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Component, Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Cube {
    hue: f32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Chat(String);

const PATIENCE: Duration = Duration::from_secs(30);

fn app(name: &str) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        IrohPlugin::default()
            .with_relays(Relays::Disabled)
            .with_display_name(name),
    ))
    .replicate::<Cube>()
    .add_net_message::<Chat>();
    app
}

/// Step both apps until `pred` holds on `b`, or give up.
fn wait(a: &mut App, b: &mut App, what: &str, mut pred: impl FnMut(&mut App) -> bool) {
    let start = Instant::now();
    loop {
        a.update();
        b.update();
        if pred(b) {
            eprintln!("{what}: {:?}", start.elapsed());
            return;
        }
        assert!(start.elapsed() < PATIENCE, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn ticket(app: &mut App) -> Option<RoomTicket> {
    app.world_mut()
        .query::<&Ticket>()
        .iter(app.world())
        .next()
        .map(|t| t.0.clone())
}

fn remote_cube(app: &mut App) -> Option<(Entity, Cube, Transform)> {
    app.world_mut()
        .query_filtered::<(Entity, &Cube, &Transform), With<Remote>>()
        .iter(app.world())
        .next()
        .map(|(e, c, t)| (e, c.clone(), *t))
}

#[test]
fn a_cube_crosses_between_two_apps() {
    let mut alice = app("alice");
    let mut bob = app("bob");

    alice.world_mut().spawn(Room::host("test"));
    let cube = alice
        .world_mut()
        .spawn((
            Cube { hue: 0.25 },
            Shared::default(),
            Transform::from_xyz(1.0, 2.0, 3.0),
        ))
        .id();

    // Alice gets a ticket once she has an address to put in it.
    let mut placeholder = App::new();
    wait(&mut placeholder, &mut alice, "a ticket", |a| {
        ticket(a).is_some()
    });
    let ticket = ticket(&mut alice).unwrap();
    assert!(!ticket.peers.is_empty());

    bob.world_mut().spawn(Room::join(ticket));

    // Each sees the other.
    wait(&mut alice, &mut bob, "bob to see alice", |b| {
        b.world_mut()
            .query::<&Peer>()
            .iter(b.world())
            .any(|p| p.name.as_deref() == Some("alice"))
    });
    wait(&mut bob, &mut alice, "alice to see bob", |a| {
        a.world_mut()
            .query::<&Peer>()
            .iter(a.world())
            .any(|p| p.name.as_deref() == Some("bob"))
    });

    // The cube arrives with its data, its transform, and its owner.
    wait(&mut alice, &mut bob, "the cube", |b| {
        remote_cube(b).is_some()
    });
    let (remote, data, transform) = remote_cube(&mut bob).unwrap();
    assert_eq!(data, Cube { hue: 0.25 });
    assert_eq!(transform.translation, Vec3::new(1.0, 2.0, 3.0));
    let owner = bob.world().get::<Owner>(remote).unwrap().0;
    assert_eq!(owner, alice.world().resource::<Iroh>().id());
    assert_eq!(
        bob.world().get::<NetId>(remote),
        alice.world().get::<NetId>(cube)
    );

    // A move follows, smoothed.
    alice
        .world_mut()
        .get_mut::<Transform>(cube)
        .unwrap()
        .translation = Vec3::new(5.0, 2.0, 3.0);
    wait(&mut alice, &mut bob, "the move", |b| {
        remote_cube(b).is_some_and(|(_, _, t)| (t.translation.x - 5.0).abs() < 1e-3)
    });

    // A change to data follows.
    alice.world_mut().get_mut::<Cube>(cube).unwrap().hue = 0.75;
    wait(&mut alice, &mut bob, "the data change", |b| {
        remote_cube(b).is_some_and(|(_, c, _)| c.hue == 0.75)
    });

    // Bob's edits to Alice's cube do not travel back: she owns it.
    bob.world_mut().get_mut::<Cube>(remote).unwrap().hue = 0.1;
    for _ in 0..20 {
        alice.update();
        bob.update();
        std::thread::sleep(Duration::from_millis(16));
    }
    assert_eq!(alice.world().get::<Cube>(cube).unwrap().hue, 0.75);

    // A chat message crosses, attributed and gossiped.
    let room_b = bob
        .world_mut()
        .query_filtered::<Entity, With<RoomTopic>>()
        .single(bob.world())
        .unwrap();
    bob.world_mut()
        .run_system_once(move |net: NetSender| net.broadcast(room_b, &Chat("hi".into())))
        .unwrap();
    wait(&mut bob, &mut alice, "the chat", |a| {
        let msgs = a.world().resource::<Messages<Received<Chat>>>();
        msgs.iter_current_update_messages()
            .any(|m| m.msg == Chat("hi".into()) && m.via == Via::Gossip)
    });

    // Despawn propagates.
    alice.world_mut().despawn(cube);
    wait(&mut alice, &mut bob, "the despawn", |b| {
        remote_cube(b).is_none()
    });
}

use bevy::ecs::system::RunSystemOnce;
