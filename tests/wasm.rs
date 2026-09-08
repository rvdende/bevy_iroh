//! Two apps in one page, over the real relay: a browser peer has no direct addresses, so this
//! is the whole browser path. Headless, through wasm-bindgen-test-runner.
//!
//! ```sh
//! GECKODRIVER=$(which geckodriver) WASM_BINDGEN_TEST_TIMEOUT=180 \
//!   cargo test --target wasm32-unknown-unknown --features wasm --test wasm
//! ```
#![cfg(target_arch = "wasm32")]

use std::time::Duration;

use bevy::prelude::*;
use bevy_iroh::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[derive(Component, Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Cube {
    hue: f32,
}

fn app(name: &str) -> App {
    let mut app = App::new();
    app.add_plugins((
        MinimalPlugins,
        IrohPlugin::default().with_display_name(name),
    ))
    .replicate::<Cube>();
    app
}

/// Step both apps until `pred` holds on `b`, or give up. Yields to the page between frames so
/// the endpoint's own tasks run: a browser has one thread and this is it.
async fn wait(a: &mut App, b: &mut App, what: &str, mut pred: impl FnMut(&mut App) -> bool) {
    let started = web_time::Instant::now();
    loop {
        a.update();
        b.update();
        if pred(b) {
            web_sys::console::log_1(&format!("{what}: {:?}", started.elapsed()).into());
            return;
        }
        assert!(
            started.elapsed() < Duration::from_secs(150),
            "gave up waiting for {what}"
        );
        n0_future::time::sleep(Duration::from_millis(16)).await;
    }
}

#[wasm_bindgen_test]
async fn a_cube_crosses_between_two_browser_apps() {
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

    // A ticket once Alice has reached a relay.
    let mut none = App::new();
    wait(&mut none, &mut alice, "a ticket", |a| {
        a.world_mut()
            .query::<&Ticket>()
            .iter(a.world())
            .next()
            .is_some()
    })
    .await;
    let ticket = alice
        .world_mut()
        .query::<&Ticket>()
        .single(alice.world())
        .unwrap()
        .0
        .clone();
    bob.world_mut().spawn(Room::join(ticket));

    wait(&mut alice, &mut bob, "bob to see alice", |b| {
        b.world_mut()
            .query::<&Peer>()
            .iter(b.world())
            .any(|p| p.name.as_deref() == Some("alice"))
    })
    .await;
    wait(&mut bob, &mut alice, "alice to see bob", |a| {
        a.world_mut()
            .query::<&Peer>()
            .iter(a.world())
            .any(|p| p.name.as_deref() == Some("bob"))
    })
    .await;

    wait(&mut alice, &mut bob, "the cube", |b| {
        b.world_mut()
            .query_filtered::<&Cube, With<Remote>>()
            .iter(b.world())
            .next()
            .is_some_and(|c| c.hue == 0.25)
    })
    .await;

    alice
        .world_mut()
        .get_mut::<Transform>(cube)
        .unwrap()
        .translation
        .x = 5.0;
    wait(&mut alice, &mut bob, "the move", |b| {
        b.world_mut()
            .query_filtered::<&Transform, (With<Cube>, With<Remote>)>()
            .iter(b.world())
            .next()
            .is_some_and(|t| (t.translation.x - 5.0).abs() < 1e-3)
    })
    .await;
}
