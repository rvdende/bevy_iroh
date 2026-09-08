//! The hello world: each peer owns one cube and sees everyone else's.
//!
//! ```sh
//! cargo run --example cube                 # prints a ticket
//! cargo run --example cube -- <ticket>     # on another machine, or another terminal
//! ```
//!
//! Arrow keys move your cube. Peers' cubes glide to where their owners put them.

use bevy::prelude::*;
use bevy_iroh::prelude::*;
use serde::{Deserialize, Serialize};

/// What this entity is. Replicated, so a peer can build the visuals from it.
#[derive(Component, Serialize, Deserialize, Clone)]
struct Cube {
    color: [f32; 3],
}

fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins.set(bevy::log::LogPlugin {
                filter: "wgpu=error,naga=warn,bevy_iroh=debug".into(),
                ..default()
            }),
            IrohPlugin::default().with_display_name(whoami()),
        ))
        .replicate::<Cube>()
        .add_systems(Startup, setup)
        .add_systems(Update, (drive, print_ticket, announce_peers, page_title))
        .add_observer(dress_cube)
        .run();
}

fn whoami() -> String {
    std::env::var("USER").unwrap_or_else(|_| "someone".into())
}

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // A ticket on the command line, or `?join=` in a page's address bar.
    #[cfg(not(target_arch = "wasm32"))]
    let ticket: Option<RoomTicket> = std::env::args()
        .nth(1)
        .map(|t| t.parse().expect("that is not a ticket"));
    #[cfg(target_arch = "wasm32")]
    let ticket: Option<RoomTicket> = bevy_iroh::web::ticket_from_url("join");
    match ticket {
        Some(ticket) => {
            info!("joining {}", ticket.name);
            commands.spawn(Room::join(ticket));
        }
        None => {
            commands.spawn(Room::host("cubes"));
        }
    }

    // Mine: shared into every room I am in.
    let hue = rand::random::<f32>() * 360.0;
    let color = Color::hsl(hue, 0.8, 0.5).to_linear();
    commands.spawn((
        Cube {
            color: [color.red, color.green, color.blue],
        },
        Shared::default(),
        Transform::from_xyz(
            rand::random::<f32>() * 4.0 - 2.0,
            0.5,
            rand::random::<f32>() * 4.0 - 2.0,
        ),
    ));

    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(12.0, 12.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.3, 0.5, 0.3))),
    ));
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 4.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
    commands.spawn((
        Camera3d::default(),
        Transform::from_xyz(0.0, 7.0, 9.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

/// Runs for my cube and for every cube a peer sends: one code path for visuals.
fn dress_cube(
    add: On<Add, Cube>,
    cubes: Query<&Cube>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let cube = cubes.get(add.entity).unwrap();
    commands.entity(add.entity).insert((
        Mesh3d(meshes.add(Cuboid::from_length(1.0))),
        MeshMaterial3d(materials.add(Color::linear_rgb(
            cube.color[0],
            cube.color[1],
            cube.color[2],
        ))),
    ));
}

/// Arrow keys move the cube that is mine. Peers' cubes are `Remote`, and not ours to drive.
fn drive(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    mut mine: Query<&mut Transform, (With<Cube>, Without<Remote>)>,
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

fn print_ticket(tickets: Query<&Ticket, Added<Ticket>>) {
    for ticket in &tickets {
        println!(
            "\njoin with:\n\n    cargo run --example cube -- {}\n",
            ticket.0
        );
    }
}

fn announce_peers(
    mut joined: MessageReader<PeerJoined>,
    mut left: MessageReader<PeerLeft>,
    peers: Query<&Peer>,
) {
    for j in joined.read() {
        let label = peers.get(j.peer).map(|p| p.label()).unwrap_or_default();
        info!("{label} joined");
    }
    for l in left.read() {
        info!("{} left", l.id.fmt_short());
    }
}

/// In a page there is no terminal: the tab title says what the room is doing.
#[cfg(target_arch = "wasm32")]
fn page_title(rooms: Query<(&Room, &RoomStatus)>, peers: Query<&Peer>) {
    let Ok((room, status)) = rooms.single() else {
        return;
    };
    let title = format!("{} {:?} peers={}", room.name, status, peers.iter().count());
    if let Some(document) = web_sys::window().and_then(|w| w.document()) {
        if document.title() != title {
            document.set_title(&title);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn page_title() {}
