//! A headless host: no window, no GPU. Hosts a room, prints the ticket, and drives one cube
//! around in a circle so that a joining peer, native or browser, has something to watch.
//!
//! ```sh
//! cargo run --example host
//! ```
//!
//! Also what a server would look like: a peer that keeps a room alive when everyone else has
//! closed their laptop.

use std::time::Duration;

use bevy::{app::ScheduleRunnerPlugin, prelude::*};
use bevy_iroh::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Component, Serialize, Deserialize, Clone)]
struct Cube {
    color: [f32; 3],
}

fn main() {
    App::new()
        .add_plugins((
            MinimalPlugins.set(ScheduleRunnerPlugin::run_loop(Duration::from_millis(16))),
            bevy::log::LogPlugin {
                // `RUST_LOG=bevy_iroh=debug,iroh=debug cargo run --example host` to see dials.
                filter: std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
                ..default()
            },
            IrohPlugin::default().with_display_name("host"),
        ))
        .replicate::<Cube>()
        .add_systems(Startup, setup)
        .add_systems(Update, (orbit, print_ticket, announce))
        .run();
}

fn setup(mut commands: Commands) {
    commands.spawn(Room::host("cubes"));
    commands.spawn((
        Cube {
            color: [0.9, 0.6, 0.2],
        },
        Shared::default(),
        Transform::from_xyz(2.0, 0.5, 0.0),
    ));
}

fn orbit(time: Res<Time>, mut cubes: Query<&mut Transform, (With<Cube>, Without<Remote>)>) {
    let t = time.elapsed_secs();
    for mut transform in &mut cubes {
        transform.translation = Vec3::new(2.0 * t.cos(), 0.5, 2.0 * t.sin());
        transform.rotation = Quat::from_rotation_y(t);
    }
}

fn print_ticket(tickets: Query<&Ticket, Added<Ticket>>) {
    for ticket in &tickets {
        println!(
            "\njoin with:\n\n    cargo run --example cube -- {}\n\nor in a browser: ?join={}\n",
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
