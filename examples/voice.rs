//! Voice chat: everyone in the room is a sphere, with a bar over it that rises as they speak.
//!
//! ```sh
//! cargo run --example voice --features ui                 # prints a ticket
//! cargo run --example voice --features ui -- <ticket>
//! ./scripts/web.sh voice                                  # the same, in a browser tab
//! ```
//!
//! Arrow keys move you; each peer is heard from where they are. Top right: a mute button, a
//! meter that moves when your microphone hears you, and "Devices" for picking the microphone
//! and the speaker. Headphones: there is no echo cancellation on a desktop.
#![allow(clippy::type_complexity)]

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
            IrohPlugin::default().with_display_name(display_name()),
            MediaUiPlugin,
        ))
        .replicate::<Avatar>()
        .add_systems(Startup, setup)
        .add_systems(Update, (drive, print_ticket, announce, page_title))
        .add_observer(dress)
        .run();
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    match ticket() {
        Some(ticket) => commands.spawn(Room::join(ticket)),
        None => commands.spawn(Room::host("voice")),
    };
    // Me: a voice on a shared entity. Peers get `Avatar` + `Voice` + `Transform` and hear me.
    commands.spawn((
        Avatar {
            hue: rand::random::<f32>() * 360.0,
        },
        Voice::default(),
        Shared::default(),
        Transform::from_xyz(
            rand::random::<f32>() * 4.0 - 2.0,
            0.5,
            rand::random::<f32>() * 4.0 - 2.0,
        ),
    ));
    // Mute, meter and the device pickers, top right.
    commands.spawn(MediaPanel::voice());

    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(12.0, 12.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.25, 0.3, 0.35))),
    ));
    commands.spawn((
        DirectionalLight::default(),
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    // The listener rides the camera: a voice on the left of the screen is heard on the left.
    commands.spawn((
        Camera3d::default(),
        AudioListener,
        Transform::from_xyz(0.0, 6.0, 8.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// Runs for my avatar and for every avatar a peer sends me.
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
        // The bar over the head, fed by this entity's `VoiceLevel`.
        VoiceIndicator::default(),
    ));
}

fn drive(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut mine: Query<(&mut Transform, &mut Voice), (With<Avatar>, Without<Remote>)>,
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
    for (mut t, mut voice) in &mut mine {
        if dir != Vec3::ZERO {
            t.translation += dir.normalize() * 3.0 * time.delta_secs();
        }
        if keys.just_pressed(KeyCode::KeyM) {
            voice.muted = !voice.muted;
        }
    }
}

fn print_ticket(tickets: Query<&Ticket, Added<Ticket>>) {
    for ticket in &tickets {
        info!(
            "join with:\n\n    cargo run --example voice --features ui -- {}\n\nor in a browser: http://localhost:8000/?join={}\n",
            ticket.0, ticket.0
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

// A ticket on the command line, or `?join=` in a page's address bar.
#[cfg(not(target_arch = "wasm32"))]
fn ticket() -> Option<RoomTicket> {
    std::env::args()
        .nth(1)
        .map(|t| t.parse().expect("that is not a ticket"))
}

#[cfg(target_arch = "wasm32")]
fn ticket() -> Option<RoomTicket> {
    bevy_iroh::web::ticket_from_url("join")
}

/// In a page there is no terminal: the tab title says what the room is doing.
#[cfg(target_arch = "wasm32")]
fn page_title(rooms: Query<(&Room, &RoomStatus)>, peers: Query<(&Peer, Option<&MediaPath>)>) {
    let Ok((room, status)) = rooms.single() else {
        return;
    };
    let direct = peers
        .iter()
        .filter(|(_, p)| matches!(p, Some(MediaPath::WebRtc)))
        .count();
    let title = format!(
        "{} {:?} peers={} webrtc={}",
        room.name,
        status,
        peers.iter().count(),
        direct
    );
    if let Some(document) = web_sys::window().and_then(|w| w.document())
        && document.title() != title
    {
        document.set_title(&title);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn page_title() {}

#[cfg(not(target_arch = "wasm32"))]
fn display_name() -> String {
    std::env::var("USER").unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
fn display_name() -> String {
    "browser".into()
}
