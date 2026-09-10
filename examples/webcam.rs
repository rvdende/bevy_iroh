//! Voice, a camera and a screen: everyone in the room is a sphere with their picture floating
//! over it, and a big screen above that while they share one.
//!
//! ```sh
//! cargo run                     # prints a ticket (also `cargo run --example webcam`)
//! cargo run -- <ticket>
//! ./scripts/web.sh webcam       # the same, in a browser tab
//! ```
//!
//! Everything the `voice` example does, plus a `VideoFeed` on the same entity fed by the
//! camera picked under "Devices", and a second shared entity with `VideoInput::desktop()`
//! that follows the sphere: "Share screen" in the panel opens the desktop's own picker, and
//! whatever is chosen appears over everyone's copy of you until "Stop sharing". Your own
//! pictures show too, so what you send is what you see. Enter opens the chat box at the
//! bottom left; what anyone types floats over their sphere for fifteen seconds and lands in
//! the log.
#![allow(clippy::type_complexity)]

use bevy::{
    input::keyboard::{Key, KeyboardInput},
    prelude::*,
    ui::{UiTransform, Val2},
    window::WindowPlugin,
};
use bevy_iroh::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Component, Serialize, Deserialize, Clone)]
struct Avatar {
    hue: f32,
}

/// A shared screen, one per person, hanging over their avatar while they share.
#[derive(Component, Serialize, Deserialize, Clone)]
struct Desk;

/// One line of chat, broadcast to the room.
#[derive(Serialize, Deserialize, Clone, Debug)]
struct Chat(String);

/// Where a desk hangs, over its owner.
const DESK_ABOVE: f32 = 3.2;

pub fn main() {
    App::new()
        .add_plugins((
            DefaultPlugins.set(window_plugin()),
            IrohPlugin::default().with_display_name(display_name()),
            MediaUiPlugin,
        ))
        .replicate::<Avatar>()
        .replicate::<Desk>()
        .add_net_message::<Chat>()
        .init_resource::<Composer>()
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (drive, follow, print_ticket, announce, screens, page_title),
        )
        .add_systems(Update, (compose, receive_chat, bubbles, chat_ui).chain())
        .add_observer(on_add_avatar)
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
    let at = Vec3::new(
        rand::random::<f32>() * 4.0 - 2.0,
        0.5,
        rand::random::<f32>() * 4.0 - 2.0,
    );
    commands.spawn((
        Avatar {
            hue: rand::random::<f32>() * 360.0,
        },
        Voice::default(),
        VideoFeed::new(640, 480),
        VideoInput::camera(),
        Shared::default(),
        Transform::from_translation(at),
    ));
    // My screen: nothing until "Share screen", then whatever the picker chose, fitted into
    // 1080p. Its own entity so it can be a different size and place from the camera's.
    commands.spawn((
        Desk,
        VideoFeed::new(1920, 1080),
        VideoInput::desktop(),
        Shared::default(),
        Transform::from_translation(at + Vec3::Y * DESK_ABOVE),
    ));
    commands.spawn(MediaPanel::video());
    chat_panel(&mut commands);

    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(12.0, 12.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.25, 0.3, 0.35))),
    ));
    // A sun with shadows, so everyone stands on the floor rather than floating over it.
    commands.spawn((
        DirectionalLight {
            illuminance: 12_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(5.0, 9.0, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));

    commands.spawn((
        Camera3d::default(),
        AmbientLight {
            brightness: 250.0,
            ..default()
        },
        AudioListener,
        Transform::from_xyz(0.0, 5.0, 7.0).looking_at(Vec3::new(0.0, 0.8, 0.0), Vec3::Y),
    ));
}

fn on_add_avatar(
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
/// and my own, since `VideoInput::camera()` keeps a preview. Desks get a bigger one. When the
/// picture goes (camera off, share stopped) the screen goes with it.
#[derive(Component)]
struct Screen;

/// My desk keeps over my avatar as I drive; peers see it move like anything else shared.
fn follow(
    avatars: Query<&Transform, (With<Avatar>, Without<Remote>, Without<Desk>)>,
    mut desks: Query<&mut Transform, (With<Desk>, Without<Remote>)>,
) {
    let Ok(avatar) = avatars.single() else { return };
    for mut desk in &mut desks {
        let want = avatar.translation + Vec3::Y * DESK_ABOVE;
        if desk.translation.distance_squared(want) > 1e-6 {
            desk.translation = want;
        }
    }
}

fn screens(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    arrived: Query<
        (
            Entity,
            &VideoImage,
            &VideoFeed,
            Has<Desk>,
            Option<&Children>,
        ),
        Changed<VideoImage>,
    >,
    mut gone: RemovedComponents<VideoImage>,
    screens: Query<Entity, With<Screen>>,
    children: Query<&Children>,
) {
    let take_down = |commands: &mut Commands, kids: Option<&Children>| {
        for child in kids.into_iter().flatten() {
            if screens.contains(*child) {
                commands.entity(*child).despawn();
            }
        }
    };
    for (entity, image, feed, desk, kids) in &arrived {
        // A new handle means a new size: replace the screen rather than stack one.
        take_down(&mut commands, kids);
        let aspect = feed.width as f32 / feed.height.max(1) as f32;
        let (width, above) = if desk {
            (3.2, 0.0)
        } else {
            (0.6 * aspect, 1.5)
        };
        commands.entity(entity).with_child((
            Screen,
            Mesh3d(meshes.add(Plane3d::new(Vec3::Z, Vec2::new(width, width / aspect)))),
            MeshMaterial3d(materials.add(StandardMaterial {
                base_color_texture: Some(image.0.clone()),
                unlit: true,
                ..default()
            })),
            Transform::from_xyz(0.0, above, 0.0),
        ));
    }
    for entity in gone.read() {
        take_down(&mut commands, children.get(entity).ok());
    }
}

fn drive(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time>,
    composer: Res<Composer>,
    mut mine: Query<(&mut Transform, &mut Voice), (With<Avatar>, Without<Remote>)>,
) {
    // The keys belong to the chat box while it is open.
    if composer.typing {
        return;
    }
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
        // In a page the address bar becomes the invitation: copy the URL and send it.
        #[cfg(target_arch = "wasm32")]
        bevy_iroh::web::share_ticket_in_url("join", &ticket.0);
        info!(
            "join with:\n\n    cargo run --example webcam -- {}\n\nor in a browser: http://localhost:8000/?join={}\n",
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

/// On a page, draw into the `#bevy` canvas and fill whatever element holds it; a desktop
/// gets a window as usual.
fn window_plugin() -> WindowPlugin {
    #[cfg(target_arch = "wasm32")]
    {
        WindowPlugin {
            primary_window: Some(Window {
                canvas: Some("#bevy".into()),
                fit_canvas_to_parent: true,
                ..default()
            }),
            ..default()
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        WindowPlugin::default()
    }
}

// -- chat ------------------------------------------------------------------------------------

/// The line being typed, if the chat box is open.
#[derive(Resource, Default)]
struct Composer {
    typing: bool,
    text: String,
}

/// The last thing an avatar said, and when.
#[derive(Component)]
struct Said {
    text: String,
    at: f64,
}

/// The floating line over an avatar.
#[derive(Component)]
struct Bubble(Entity);

#[derive(Component)]
struct ChatLog;

#[derive(Component)]
struct ChatInput;

/// How long a bubble stays before it fades, and how long the fade takes.
const BUBBLE_HOLD: f64 = 15.0;
const BUBBLE_FADE: f64 = 1.0;
/// Lines the log keeps.
const LOG_LINES: usize = 14;

/// Enter opens the box; Enter sends, Escape closes; while it is open the keys are text.
fn compose(
    mut keys: MessageReader<KeyboardInput>,
    mut composer: ResMut<Composer>,
    net: NetSender,
    time: Res<Time<bevy::time::Real>>,
    mine: Query<Entity, (With<Avatar>, Without<Remote>)>,
    mut commands: Commands,
    log: Query<Entity, With<ChatLog>>,
) {
    for key in keys.read() {
        if !key.state.is_pressed() {
            continue;
        }
        if !composer.typing {
            if key.logical_key == Key::Enter {
                composer.typing = true;
            }
            continue;
        }
        match &key.logical_key {
            Key::Enter => {
                let text = std::mem::take(&mut composer.text);
                composer.typing = false;
                let text = text.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                net.broadcast_all(&Chat(text.clone()));
                for avatar in &mine {
                    commands.entity(avatar).insert(Said {
                        text: text.clone(),
                        at: time.elapsed_secs_f64(),
                    });
                }
                if let Ok(log) = log.single() {
                    log_line(&mut commands, log, &format!("{}: {text}", display_name()));
                }
            }
            Key::Escape => {
                composer.typing = false;
                composer.text.clear();
            }
            Key::Backspace => {
                composer.text.pop();
            }
            Key::Space => composer.text.push(' '),
            Key::Character(c) => composer.text.push_str(c),
            _ => {}
        }
    }
}

/// A peer's line goes over their avatar and into the log.
fn receive_chat(
    mut chats: MessageReader<Received<Chat>>,
    peers: Query<&Peer>,
    avatars: Query<(Entity, &Owner), With<Avatar>>,
    time: Res<Time<bevy::time::Real>>,
    mut commands: Commands,
    log: Query<Entity, With<ChatLog>>,
) {
    for received in chats.read() {
        let who = received
            .peer
            .and_then(|p| peers.get(p).ok())
            .map(|p| p.label())
            .unwrap_or_else(|| received.from.fmt_short().to_string());
        for (avatar, owner) in &avatars {
            if owner.0 == received.from {
                commands.entity(avatar).insert(Said {
                    text: received.msg.0.clone(),
                    at: time.elapsed_secs_f64(),
                });
            }
        }
        info!("{who}: {}", received.msg.0);
        if let Ok(log) = log.single() {
            log_line(&mut commands, log, &format!("{who}: {}", received.msg.0));
        }
    }
}

/// Bubbles follow their avatar on screen, hold, fade, and go. A new line replaces the old
/// one at once, since `Said` is one component.
fn bubbles(
    mut commands: Commands,
    time: Res<Time<bevy::time::Real>>,
    camera: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    said: Query<(Entity, &Said, &GlobalTransform)>,
    mut existing: Query<(
        Entity,
        &Bubble,
        &mut Node,
        &mut Text,
        &mut TextColor,
        &mut BackgroundColor,
    )>,
) {
    let Ok((camera, camera_transform)) = camera.single() else {
        return;
    };
    let now = time.elapsed_secs_f64();
    // One bubble per talking avatar.
    for (avatar, said, _) in &said {
        if !existing.iter().any(|(_, b, ..)| b.0 == avatar) {
            commands.spawn((
                Bubble(avatar),
                Node {
                    position_type: PositionType::Absolute,
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(4.0)),
                    max_width: Val::Px(260.0),
                    border_radius: BorderRadius::all(Val::Px(6.0)),
                    ..default()
                },
                // Centred on the point, hanging down from it.
                UiTransform::from_translation(Val2::new(Val::Percent(-50.0), Val::ZERO)),
                BackgroundColor(Color::srgba(0.08, 0.09, 0.11, 0.85)),
                Text::new(said.text.clone()),
                TextFont::from_font_size(14.0),
                TextColor(Color::WHITE),
                Pickable::IGNORE,
            ));
        }
    }
    for (entity, bubble, mut node, mut text, mut colour, mut background) in &mut existing {
        let Ok((_, said, transform)) = said.get(bubble.0) else {
            commands.entity(entity).despawn();
            continue;
        };
        let age = now - said.at;
        if age > BUBBLE_HOLD + BUBBLE_FADE {
            commands.entity(entity).despawn();
            commands.entity(bubble.0).remove::<Said>();
            continue;
        }
        if text.0 != said.text {
            text.0 = said.text.clone();
        }
        let alpha = (1.0 - (age - BUBBLE_HOLD) / BUBBLE_FADE).clamp(0.0, 1.0) as f32;
        colour.0 = Color::WHITE.with_alpha(alpha);
        background.0 = Color::srgba(0.08, 0.09, 0.11, 0.85 * alpha);
        // Just above the picture, and kept on screen for an avatar near the edge.
        let over = transform.translation() + Vec3::Y * 2.0;
        match camera.world_to_viewport(camera_transform, over) {
            Ok(at) => {
                let size = camera
                    .logical_viewport_size()
                    .unwrap_or(Vec2::splat(1000.0));
                node.left = Val::Px(at.x.clamp(140.0, size.x - 140.0));
                node.top = Val::Px(at.y.max(40.0));
                node.display = Display::Flex;
            }
            Err(_) => node.display = Display::None,
        }
    }
}

/// The log at the bottom left, newest at the bottom, with the box under it.
fn chat_panel(commands: &mut Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(10.0),
                bottom: Val::Px(10.0),
                width: Val::Px(340.0),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(4.0),
                padding: UiRect::all(Val::Px(8.0)),
                border_radius: BorderRadius::all(Val::Px(6.0)),
                ..default()
            },
            BackgroundColor(Color::srgba(0.08, 0.09, 0.11, 0.85)),
            Pickable::IGNORE,
        ))
        .with_children(|panel| {
            panel.spawn((
                ChatLog,
                Node {
                    flex_direction: FlexDirection::Column,
                    row_gap: Val::Px(2.0),
                    ..default()
                },
            ));
            panel.spawn((
                ChatInput,
                Text::new("Enter to chat"),
                TextFont::from_font_size(13.0),
                TextColor(Color::srgb(0.6, 0.6, 0.65)),
            ));
        });
}

fn log_line(commands: &mut Commands, log: Entity, line: &str) {
    let text = commands
        .spawn((
            Text::new(line.to_string()),
            TextFont::from_font_size(13.0),
            TextColor(Color::srgb(0.92, 0.92, 0.94)),
        ))
        .id();
    commands.entity(log).add_child(text);
}

/// The box shows what is being typed, and the log keeps its last lines.
fn chat_ui(
    composer: Res<Composer>,
    mut input: Query<(&mut Text, &mut TextColor), With<ChatInput>>,
    log: Query<&Children, With<ChatLog>>,
    mut commands: Commands,
) {
    for (mut text, mut colour) in &mut input {
        let (wanted, tint) = if composer.typing {
            (format!("> {}_", composer.text), Color::WHITE)
        } else {
            ("Enter to chat".to_string(), Color::srgb(0.6, 0.6, 0.65))
        };
        if text.0 != wanted {
            text.0 = wanted;
            colour.0 = tint;
        }
    }
    for children in &log {
        for old in children
            .iter()
            .take(children.len().saturating_sub(LOG_LINES))
        {
            commands.entity(old).despawn();
        }
    }
}
