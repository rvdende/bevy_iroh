//! A node that starts only when asked, stops, and starts again: over real iroh, no relay.

use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy_iroh::prelude::*;

const PATIENCE: Duration = Duration::from_secs(30);

/// Step the app until `pred` holds, or give up.
fn wait(app: &mut App, what: &str, mut pred: impl FnMut(&mut App) -> bool) {
    let start = Instant::now();
    loop {
        app.update();
        if pred(app) {
            return;
        }
        assert!(start.elapsed() < PATIENCE, "gave up waiting for {what}");
        std::thread::sleep(Duration::from_millis(16));
    }
}

fn queue(app: &mut App, command: impl Command) {
    app.world_mut().commands().queue(command);
    app.update();
}

fn id(app: &App) -> Option<EndpointId> {
    app.world().get_resource::<Iroh>().map(Iroh::id)
}

fn joined(app: &mut App, room: Entity) -> bool {
    app.world().get::<RoomTopic>(room).is_some()
}

#[test]
fn a_node_starts_when_asked_and_again_after_stopping() {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        IrohPlugin::default()
            .with_relays(Relays::Disabled)
            .with_autostart(false),
    ));
    for _ in 0..3 {
        app.update();
    }
    assert!(id(&app).is_none(), "no node before StartIroh");

    queue(&mut app, StartIroh::default());
    let first = id(&app).expect("a node after StartIroh");
    let room = app.world_mut().spawn(Room::host("before")).id();
    wait(&mut app, "the first room", |a| joined(a, room));

    queue(&mut app, StopIroh);
    assert!(id(&app).is_none(), "no node after StopIroh");
    assert!(!joined(&mut app, room), "the room is left");
    assert!(matches!(
        app.world().get::<RoomStatus>(room),
        Some(RoomStatus::Failed(_))
    ));
    assert_eq!(app.world().resource::<Rooms>().joined().count(), 0);

    queue(&mut app, StartIroh::default());
    assert_eq!(id(&app), Some(first), "the same identity after a restart");
    let again = app.world_mut().spawn(Room::host("after")).id();
    wait(&mut app, "a room on the restarted node", |a| {
        joined(a, again)
    });

    // Another identity, given when starting: kept from then on.
    queue(&mut app, StopIroh);
    let other = bevy_iroh::iroh::SecretKey::generate();
    queue(
        &mut app,
        StartIroh {
            identity: Some(Identity::Key(other.clone())),
            display_name: Some("someone else".into()),
        },
    );
    assert_eq!(id(&app), Some(other.public()));
    assert_eq!(
        app.world().resource::<Iroh>().display_name(),
        "someone else"
    );
}
