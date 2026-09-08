//! Voice chat: everyone in the room is a sphere that swells when they speak.
//!
//! ```sh
//! cargo run --example conference --features media                 # prints a ticket
//! cargo run --example conference --features media -- <ticket>
//! ```
//!
//! Arrow keys move you; the sound of each peer comes from where they are. `M` mutes.
//! Headphones: there is no echo cancellation.

use bevy::prelude::*;
use bevy_iroh::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Component, Serialize, Deserialize, Clone)]
struct Avatar {
    hue: f32,
}

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins,
            IrohPlugin::default().with_display_name(std::env::var("USER").unwrap_or_default()),
        ))
        .replicate::<Avatar>()
        .add_systems(Startup, setup)
        .add_systems(Update, (drive, mute, swell, print_ticket, announce))
        .add_observer(dress)
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    match std::env::args().nth(1) {
        Some(t) => {
            commands.spawn(Room::join(t.parse().expect("that is not a ticket")));
        }
        None => {
            commands.spawn(Room::host("conference"));
        }
    }
    commands.spawn((
        Avatar {
            hue: rand::random::<f32>() * 360.0,
        },
        Voice::default(),
        Shared::default(),
        Transform::from_xyz(
            rand::random::<f32>() * 4.0 - 2.0,
            0.6,
            rand::random::<f32>() * 4.0 - 2.0,
        ),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(12.0, 12.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.25, 0.3, 0.35))),
    ));
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    // The listener rides the camera: a voice on the left of the screen is heard on the left.
    commands.spawn((
        Camera3d::default(),
        AudioListener,
        Transform::from_xyz(0.0, 6.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn dress(
    add: On<Add, Avatar>,
    avatars: Query<&Avatar>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let avatar = avatars.get(add.entity).unwrap();
    commands.entity(add.entity).insert((
        Mesh3d(meshes.add(Sphere::new(0.5))),
        MeshMaterial3d(materials.add(Color::hsl(avatar.hue, 0.7, 0.5))),
    ));
}

fn drive(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut mine: Query<&mut Transform, (With<Avatar>, Without<Remote>)>,
) {
    let mut dir = Vec3::ZERO;
    if keys.pressed(KeyCode::ArrowLeft) {
        dir.x -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowRight) {
        dir.x += 1.0;
    }
    if keys.pressed(KeyCode::ArrowUp) {
        dir.z -= 1.0;
    }
    if keys.pressed(KeyCode::ArrowDown) {
        dir.z += 1.0;
    }
    if dir == Vec3::ZERO {
        return;
    }
    for mut t in &mut mine {
        t.translation += dir.normalize() * 3.0 * time.delta_secs();
    }
}

fn mute(keys: Res<ButtonInput<KeyCode>>, mut mine: Query<&mut Voice, Without<Remote>>) {
    if keys.just_pressed(KeyCode::KeyM) {
        for mut v in &mut mine {
            v.muted = !v.muted;
            info!("{}", if v.muted { "muted" } else { "live" });
        }
    }
}

/// A sphere grows with its voice: yours from the microphone, theirs from what you hear.
fn swell(mut spheres: Query<(&VoiceLevel, &Voice, &mut Transform), With<Avatar>>) {
    for (level, voice, mut t) in &mut spheres {
        let target = if voice.muted {
            0.7
        } else {
            1.0 + (level.0 * 6.0).min(1.0)
        };
        t.scale = t.scale.lerp(Vec3::splat(target), 0.2);
    }
}

fn print_ticket(tickets: Query<&Ticket, Added<Ticket>>) {
    for ticket in &tickets {
        println!(
            "\njoin with:\n\n    cargo run --example conference --features media -- {}\n",
            ticket.0
        );
    }
}

fn announce(
    mut joined: MessageReader<PeerJoined>,
    mut left: MessageReader<PeerLeft>,
    peers: Query<&Peer>,
) {
    for j in joined.read() {
        info!(
            "{} joined",
            peers.get(j.peer).map(|p| p.label()).unwrap_or_default()
        );
    }
    for l in left.read() {
        info!("{} left", l.id.fmt_short());
    }
}
