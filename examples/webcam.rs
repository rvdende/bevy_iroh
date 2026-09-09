//! Voice and a camera: everyone in the room is a sphere with their picture floating over it.
//!
//! ```sh
//! cargo run --example webcam --features ui,v4l2            # Linux: prints a ticket
//! cargo run --example webcam --features ui,v4l2 -- <ticket>
//! ./scripts/web.sh webcam                                  # the same, in a browser tab
//! ```
//!
//! Everything the `voice` example does, plus a `VideoFeed` on the same entity fed by the
//! camera picked under "Devices". Your own picture shows over your sphere too, so what you
//! send is what you see.
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
        .add_systems(Update, (drive, print_ticket, announce, screens, page_title))
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
        None => commands.spawn(Room::host("webcam")),
    };
    // Me: a voice and a picture on one shared entity. The camera is whichever one the
    // settings name; the panel below changes it.
    commands.spawn((
        Avatar {
            hue: rand::random::<f32>() * 360.0,
        },
        Voice::default(),
        VideoFeed::new(640, 480),
        VideoInput::camera(),
        Shared::default(),
        Transform::from_xyz(
            rand::random::<f32>() * 4.0 - 2.0,
            0.5,
            rand::random::<f32>() * 4.0 - 2.0,
        ),
    ));
    commands.spawn(MediaPanel::video());

    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(12.0, 12.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.25, 0.3, 0.35))),
    ));
    commands.spawn((
        DirectionalLight::default(),
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((
        Camera3d::default(),
        AudioListener,
        Transform::from_xyz(0.0, 5.0, 7.0).looking_at(Vec3::new(0.0, 0.8, 0.0), Vec3::Y),
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
        VoiceIndicator {
            offset: Vec3::new(0.7, 0.9, 0.0),
            ..default()
        },
    ));
}

/// A screen floats over each avatar once there is a picture for it: a peer's when it arrives,
/// and my own, since `VideoInput::camera()` keeps a preview.
#[derive(Component)]
struct Screen;

fn screens(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    arrived: Query<(Entity, &VideoImage, &VideoFeed), (With<Avatar>, Added<VideoImage>)>,
) {
    for (entity, image, feed) in &arrived {
        let aspect = feed.width as f32 / feed.height.max(1) as f32;
        commands.entity(entity).with_child((
            Screen,
            Mesh3d(meshes.add(Plane3d::new(Vec3::Z, Vec2::new(0.6 * aspect, 0.6)))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color_texture: Some(image.0.clone()),
                unlit: true,
                ..default()
            })),
            Transform::from_xyz(0.0, 1.5, 0.0),
        ));
    }
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
            "join with:\n\n    cargo run --example webcam --features ui,v4l2 -- {}\n\nor in a browser: http://localhost:8000/?join={}\n",
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
