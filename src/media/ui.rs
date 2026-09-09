//! Ready-made Bevy UI for voice and video. Behind the `ui` feature.
//!
//! Spawn a [`MediaPanel`] and you have a corner of the screen with a mute button, a meter
//! that moves when the microphone hears you, and a "Devices" button that opens pickers for
//! the microphone, the speaker and, if asked, the camera. Put a [`VoiceIndicator`] on any
//! entity that carries a [`VoiceLevel`] and a small green bar floats over it, rising with the
//! voice. Each piece is its own component, so a different layout is a matter of spawning them
//! where you want them.
//!
//! Plain Bevy UI, no theme: a dark panel and a green that is the same green everywhere, so
//! the bar in your corner and the bar over somebody's head read as one instrument.

use bevy::{picking::Pickable, prelude::*};

use super::{
    AudioDevices, CameraChoice, CameraDevices, MediaSettings, MicLevel, MicrophoneChoice,
    Permission, SpeakerChoice, Voice, VoiceLevel,
};
use crate::replicate::Remote;

/// The green every meter here is drawn in.
pub const METER_GREEN: Color = Color::srgb(0.16, 0.95, 0.24);
const PANEL: Color = Color::srgba(0.08, 0.09, 0.11, 0.92);
const TRACK: Color = Color::srgb(0.16, 0.16, 0.18);
const ROW: Color = Color::srgba(1.0, 1.0, 1.0, 0.04);
const ROW_HOVER: Color = Color::srgba(1.0, 1.0, 1.0, 0.10);
const ROW_SELECTED: Color = Color::srgba(0.16, 0.95, 0.24, 0.18);
const TEXT: Color = Color::srgb(0.92, 0.92, 0.94);
const TEXT_DIM: Color = Color::srgb(0.6, 0.6, 0.65);
const MUTED: Color = Color::srgb(0.9, 0.35, 0.3);

/// Registers the systems behind every widget here. Needs `DefaultPlugins` (or the UI and
/// picking plugins) and `IrohPlugin` with the `media` feature.
pub struct MediaUiPlugin;

impl Plugin for MediaUiPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            (
                build_panels,
                build_meters,
                build_mute_buttons,
                build_pickers,
                toggle_panels,
                mute_clicks,
                picker_clicks,
                row_hover,
                sync_meters,
                sync_mute_buttons,
                build_indicators,
                sync_indicators,
            ),
        );
    }
}

// -- the corner panel ----------------------------------------------------------------------

/// A whole corner: mute button, microphone meter, and a button that opens the device pickers.
/// Spawn it and nothing else; it positions itself top right.
#[derive(Component, Debug, Clone)]
pub struct MediaPanel {
    /// Also offer a camera picker.
    pub video: bool,
}

impl MediaPanel {
    pub fn voice() -> Self {
        Self { video: false }
    }

    pub fn video() -> Self {
        Self { video: true }
    }
}

/// The button on a panel that shows and hides its pickers.
#[derive(Component)]
struct DevicesButton(Entity);

/// The pickers a panel opens.
#[derive(Component)]
struct PickerDrawer;

fn build_panels(mut commands: Commands, panels: Query<(Entity, &MediaPanel), Added<MediaPanel>>) {
    for (entity, panel) in &panels {
        let mut drawer = Entity::PLACEHOLDER;
        commands
            .entity(entity)
            .insert((
                Node {
                    position_type: PositionType::Absolute,
                    top: Val::Px(10.0),
                    right: Val::Px(10.0),
                    flex_direction: FlexDirection::Column,
                    align_items: AlignItems::FlexEnd,
                    row_gap: Val::Px(6.0),
                    ..default()
                },
                Pickable::IGNORE,
            ))
            .with_children(|parent| {
                // The strip: [mute] [meter] [devices]
                let strip = parent
                    .spawn((
                        Node {
                            flex_direction: FlexDirection::Row,
                            align_items: AlignItems::Center,
                            column_gap: Val::Px(8.0),
                            padding: UiRect::axes(Val::Px(10.0), Val::Px(6.0)),
                            border_radius: BorderRadius::all(Val::Px(6.0)),
                            ..default()
                        },
                        BackgroundColor(PANEL),
                    ))
                    .id();
                drawer = parent
                    .spawn((
                        PickerDrawer,
                        Node {
                            display: Display::None,
                            flex_direction: FlexDirection::Column,
                            row_gap: Val::Px(8.0),
                            padding: UiRect::all(Val::Px(10.0)),
                            width: Val::Px(320.0),
                            border_radius: BorderRadius::all(Val::Px(6.0)),
                            ..default()
                        },
                        BackgroundColor(PANEL),
                    ))
                    .with_children(|drawer| {
                        drawer.spawn(DevicePicker::microphone());
                        drawer.spawn(DevicePicker::speaker());
                        if panel.video {
                            drawer.spawn(DevicePicker::camera());
                        }
                    })
                    .id();
                parent.commands().entity(strip).with_children(|strip| {
                    strip.spawn(MuteButton);
                    strip.spawn(MicMeter::default());
                    strip
                        .spawn((
                            DevicesButton(drawer),
                            Button,
                            Node {
                                padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                                border_radius: BorderRadius::all(Val::Px(4.0)),
                                ..default()
                            },
                            BackgroundColor(ROW),
                        ))
                        .with_child((
                            Text::new("Devices"),
                            TextFont::from_font_size(13.0),
                            TextColor(TEXT),
                        ));
                });
            });
        let _ = drawer;
    }
}

fn toggle_panels(
    buttons: Query<(&Interaction, &DevicesButton), Changed<Interaction>>,
    mut drawers: Query<&mut Node, With<PickerDrawer>>,
) {
    for (interaction, button) in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        if let Ok(mut node) = drawers.get_mut(button.0) {
            node.display = if node.display == Display::None {
                Display::Flex
            } else {
                Display::None
            };
        }
    }
}

// -- the microphone meter ------------------------------------------------------------------

/// A vertical bar that fills with [`MicLevel`]: green from the bottom, on a dark track.
#[derive(Component, Debug, Clone)]
pub struct MicMeter {
    pub width: f32,
    pub height: f32,
}

impl Default for MicMeter {
    fn default() -> Self {
        Self {
            width: 6.0,
            height: 22.0,
        }
    }
}

#[derive(Component)]
struct MeterFill;

fn build_meters(mut commands: Commands, meters: Query<(Entity, &MicMeter), Added<MicMeter>>) {
    for (entity, meter) in &meters {
        commands
            .entity(entity)
            .insert((
                Node {
                    width: Val::Px(meter.width),
                    height: Val::Px(meter.height),
                    flex_direction: FlexDirection::Column,
                    justify_content: JustifyContent::FlexEnd,
                    flex_shrink: 0.0,
                    border_radius: BorderRadius::all(Val::Px(2.0)),
                    ..default()
                },
                BackgroundColor(TRACK),
            ))
            .with_child((
                MeterFill,
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Percent(0.0),
                    border_radius: BorderRadius::all(Val::Px(2.0)),
                    ..default()
                },
                BackgroundColor(METER_GREEN),
            ));
    }
}

/// Whole percent steps, so a hovering level does not relayout the bar over sub-pixel
/// differences every frame.
fn sync_meters(level: Res<MicLevel>, mut fills: Query<&mut Node, With<MeterFill>>) {
    let wanted = Val::Percent((level.0 * 100.0).round().clamp(0.0, 100.0));
    for mut fill in &mut fills {
        if fill.height != wanted {
            fill.height = wanted;
        }
    }
}

// -- mute ----------------------------------------------------------------------------------

/// A button that mutes and unmutes every local [`Voice`]. Reads "Mic on" or "Muted".
#[derive(Component, Debug, Clone, Default)]
pub struct MuteButton;

#[derive(Component)]
struct MuteLabel;

fn build_mute_buttons(mut commands: Commands, buttons: Query<Entity, Added<MuteButton>>) {
    for entity in &buttons {
        commands
            .entity(entity)
            .insert((
                Button,
                Node {
                    padding: UiRect::axes(Val::Px(8.0), Val::Px(3.0)),
                    min_width: Val::Px(64.0),
                    justify_content: JustifyContent::Center,
                    border_radius: BorderRadius::all(Val::Px(4.0)),
                    ..default()
                },
                BackgroundColor(ROW),
            ))
            .with_child((
                MuteLabel,
                Text::new("Mic on"),
                TextFont::from_font_size(13.0),
                TextColor(TEXT),
            ));
    }
}

fn mute_clicks(
    buttons: Query<&Interaction, (With<MuteButton>, Changed<Interaction>)>,
    mut voices: Query<&mut Voice, Without<Remote>>,
) {
    for interaction in &buttons {
        if *interaction != Interaction::Pressed {
            continue;
        }
        let muted = voices.iter().any(|v| !v.muted);
        for mut voice in &mut voices {
            voice.muted = muted;
        }
    }
}

fn sync_mute_buttons(
    voices: Query<&Voice, Without<Remote>>,
    mut labels: Query<(&mut Text, &mut TextColor), With<MuteLabel>>,
) {
    let muted = !voices.is_empty() && voices.iter().all(|v| v.muted);
    let (text, colour) = if muted {
        ("Muted", MUTED)
    } else {
        ("Mic on", TEXT)
    };
    for (mut label, mut tint) in &mut labels {
        if label.0 != text {
            label.0 = text.into();
            tint.0 = colour;
        }
    }
}

// -- device pickers ------------------------------------------------------------------------

/// Which list a picker shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Microphone,
    Speaker,
    Camera,
}

/// A titled list of devices; the chosen one is highlighted and a click chooses another.
/// In a page the list starts with a button that asks for permission.
#[derive(Component, Debug, Clone)]
pub struct DevicePicker {
    pub kind: DeviceKind,
    pub title: String,
}

impl DevicePicker {
    pub fn microphone() -> Self {
        Self {
            kind: DeviceKind::Microphone,
            title: "Microphone".into(),
        }
    }

    pub fn speaker() -> Self {
        Self {
            kind: DeviceKind::Speaker,
            title: "Speaker".into(),
        }
    }

    pub fn camera() -> Self {
        Self {
            kind: DeviceKind::Camera,
            title: "Camera".into(),
        }
    }
}

/// What a row does when clicked.
#[derive(Component, Clone)]
enum RowAction {
    Choose(DeviceKind, Option<String>),
    Request(DeviceKind),
}

#[derive(Component)]
struct RowSelected(bool);

/// Rows are rebuilt when the lists or the choice change.
fn build_pickers(
    mut commands: Commands,
    audio: Res<AudioDevices>,
    cameras: Res<CameraDevices>,
    settings: Res<MediaSettings>,
    pickers: Query<(Entity, &DevicePicker, Option<&Children>), With<DevicePicker>>,
    added: Query<Entity, Added<DevicePicker>>,
) {
    let changed = audio.is_changed() || cameras.is_changed() || settings.is_changed();
    for (entity, picker, children) in &pickers {
        if !changed && added.get(entity).is_err() {
            continue;
        }
        if let Some(children) = children {
            for child in children.iter() {
                commands.entity(child).despawn();
            }
        }
        let (permission, devices): (Permission, Vec<(String, String, String, bool)>) =
            match picker.kind {
                DeviceKind::Microphone => (
                    audio.permission.clone(),
                    audio
                        .microphones
                        .iter()
                        .map(|d| (d.id.clone(), d.name.clone(), d.detail(), d.is_default))
                        .collect(),
                ),
                DeviceKind::Speaker => (
                    audio.permission.clone(),
                    audio
                        .speakers
                        .iter()
                        .map(|d| (d.id.clone(), d.name.clone(), d.detail(), d.is_default))
                        .collect(),
                ),
                DeviceKind::Camera => (
                    cameras.permission.clone(),
                    cameras
                        .cameras
                        .iter()
                        .map(|d| (d.id.clone(), d.name.clone(), d.detail.clone(), d.is_default))
                        .collect(),
                ),
            };
        let chosen: Option<Option<String>> = match picker.kind {
            DeviceKind::Microphone => match &settings.microphone {
                MicrophoneChoice::Default => Some(None),
                MicrophoneChoice::Device(id) => Some(Some(id.clone())),
                _ => None,
            },
            DeviceKind::Speaker => match &settings.speaker {
                SpeakerChoice::Default => Some(None),
                SpeakerChoice::Device(id) => Some(Some(id.clone())),
                _ => None,
            },
            DeviceKind::Camera => match &settings.camera {
                CameraChoice::Default => Some(None),
                CameraChoice::Device(id) => Some(Some(id.clone())),
                CameraChoice::None => None,
            },
        };
        commands
            .entity(entity)
            .insert(Node {
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(3.0),
                ..default()
            })
            .with_children(|list| {
                list.spawn((
                    Text::new(picker.title.clone()),
                    TextFont::from_font_size(12.0),
                    TextColor(TEXT_DIM),
                ));
                match permission {
                    Permission::Idle if devices.is_empty() => {
                        row(
                            list,
                            RowAction::Request(picker.kind),
                            false,
                            &format!("Allow {} access", picker.title.to_lowercase()),
                            "the browser will ask",
                        );
                    }
                    Permission::Asking => {
                        list.spawn((
                            Text::new("Asking the browser…"),
                            TextFont::from_font_size(12.0),
                            TextColor(TEXT_DIM),
                        ));
                    }
                    Permission::Denied(ref why) => {
                        list.spawn((
                            Text::new(format!("No access: {why}")),
                            TextFont::from_font_size(12.0),
                            TextColor(MUTED),
                        ));
                    }
                    _ => {}
                }
                if permission == Permission::Ready || !devices.is_empty() {
                    let default_name = devices
                        .iter()
                        .find(|d| d.3)
                        .map(|d| d.1.as_str())
                        .unwrap_or("whatever the system picks");
                    row(
                        list,
                        RowAction::Choose(picker.kind, None),
                        chosen == Some(None),
                        "System default",
                        default_name,
                    );
                    for (id, name, detail, _) in &devices {
                        row(
                            list,
                            RowAction::Choose(picker.kind, Some(id.clone())),
                            chosen.as_ref().is_some_and(|c| c.as_deref() == Some(id)),
                            name,
                            detail,
                        );
                    }
                    if devices.is_empty() && permission == Permission::Ready {
                        list.spawn((
                            Text::new("none found"),
                            TextFont::from_font_size(12.0),
                            TextColor(TEXT_DIM),
                        ));
                    }
                }
            });
    }
}

fn row(
    list: &mut ChildSpawnerCommands,
    action: RowAction,
    selected: bool,
    name: &str,
    detail: &str,
) {
    list.spawn((
        action,
        RowSelected(selected),
        Button,
        Node {
            flex_direction: FlexDirection::Column,
            padding: UiRect::axes(Val::Px(8.0), Val::Px(4.0)),
            border_radius: BorderRadius::all(Val::Px(4.0)),
            ..default()
        },
        BackgroundColor(if selected { ROW_SELECTED } else { ROW }),
    ))
    .with_children(|r| {
        r.spawn((
            Text::new(if selected {
                format!("> {name}")
            } else {
                name.to_string()
            }),
            TextFont::from_font_size(13.0),
            TextColor(TEXT),
        ));
        r.spawn((
            Text::new(detail.to_string()),
            TextFont::from_font_size(10.0),
            TextColor(TEXT_DIM),
        ));
    });
}

fn picker_clicks(
    rows: Query<(&Interaction, &RowAction), Changed<Interaction>>,
    mut settings: ResMut<MediaSettings>,
    mut audio: ResMut<AudioDevices>,
    mut cameras: ResMut<CameraDevices>,
) {
    for (interaction, action) in &rows {
        if *interaction != Interaction::Pressed {
            continue;
        }
        match action {
            RowAction::Request(DeviceKind::Camera) => cameras.request(),
            RowAction::Request(_) => audio.request(),
            RowAction::Choose(DeviceKind::Microphone, id) => {
                settings.microphone = match id {
                    Some(id) => MicrophoneChoice::Device(id.clone()),
                    None => MicrophoneChoice::Default,
                };
            }
            RowAction::Choose(DeviceKind::Speaker, id) => {
                settings.speaker = match id {
                    Some(id) => SpeakerChoice::Device(id.clone()),
                    None => SpeakerChoice::Default,
                };
            }
            RowAction::Choose(DeviceKind::Camera, id) => {
                settings.camera = match id {
                    Some(id) => CameraChoice::Device(id.clone()),
                    None => CameraChoice::Default,
                };
            }
        }
    }
}

fn row_hover(
    mut rows: Query<
        (&Interaction, &mut BackgroundColor, Option<&RowSelected>),
        (With<Button>, Changed<Interaction>),
    >,
) {
    for (interaction, mut colour, selected) in &mut rows {
        let selected = selected.is_some_and(|s| s.0);
        colour.0 = match interaction {
            Interaction::Hovered | Interaction::Pressed => ROW_HOVER,
            Interaction::None if selected => ROW_SELECTED,
            Interaction::None => ROW,
        };
    }
}

// -- the bar over a head -------------------------------------------------------------------

/// A small green bar floating over an entity, rising with its [`VoiceLevel`]. Put it on the
/// entity that carries the voice, or on any entity whose level you copy there. Two unlit
/// cuboids: a dark track, always there so a quiet person still reads as a person with a
/// voice, and the bar, which fades to black at rest.
#[derive(Component, Debug, Clone)]
pub struct VoiceIndicator {
    /// Where the bar's bottom sits, in the entity's space.
    pub offset: Vec3,
    pub height: f32,
    pub width: f32,
}

impl Default for VoiceIndicator {
    fn default() -> Self {
        Self {
            offset: Vec3::new(0.0, 0.85, 0.0),
            height: 0.25,
            width: 0.05,
        }
    }
}

#[derive(Component)]
struct IndicatorBar {
    of: Entity,
    height: f32,
    base: Vec3,
}

fn build_indicators(
    mut commands: Commands,
    added: Query<(Entity, &VoiceIndicator), Added<VoiceIndicator>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (entity, indicator) in &added {
        let cell = meshes.add(Cuboid::from_length(1.0));
        let centre = indicator.offset + Vec3::Y * indicator.height / 2.0;
        commands.entity(entity).with_children(|parent| {
            parent.spawn((
                Mesh3d(cell.clone()),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::srgb(0.05, 0.09, 0.06),
                    unlit: true,
                    ..default()
                })),
                Transform::from_translation(centre).with_scale(Vec3::new(
                    indicator.width,
                    indicator.height,
                    indicator.width * 0.5,
                )),
                Pickable::IGNORE,
            ));
            parent.spawn((
                IndicatorBar {
                    of: entity,
                    height: indicator.height,
                    base: indicator.offset,
                },
                Mesh3d(cell),
                MeshMaterial3d(materials.add(StandardMaterial {
                    base_color: Color::BLACK,
                    unlit: true,
                    ..default()
                })),
                bar_transform(indicator.offset, indicator.height, indicator.width, 0.0),
                Pickable::IGNORE,
            ));
        });
    }
}

/// Grows from the bottom: a cuboid is centred on its origin, so filling upwards scales the
/// height and moves the middle up by half of it. Never exactly zero: a sliver at rest.
fn bar_transform(base: Vec3, height: f32, width: f32, level: f32) -> Transform {
    let h = (height * level.clamp(0.0, 1.0)).max(0.002);
    Transform::from_translation(base + Vec3::new(0.0, h / 2.0, width * 0.55)).with_scale(Vec3::new(
        width,
        h,
        width * 0.5,
    ))
}

fn sync_indicators(
    levels: Query<(&VoiceLevel, Option<&Voice>, &VoiceIndicator)>,
    mut bars: Query<(
        &IndicatorBar,
        &mut Transform,
        &MeshMaterial3d<StandardMaterial>,
    )>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    for (bar, mut transform, material) in &mut bars {
        let (level, indicator) = match levels.get(bar.of) {
            Ok((level, voice, indicator)) => (
                if voice.is_some_and(|v| v.muted) {
                    0.0
                } else {
                    level.0
                },
                indicator,
            ),
            Err(_) => continue,
        };
        let wanted = bar_transform(bar.base, bar.height, indicator.width, level);
        if *transform != wanted {
            *transform = wanted;
        }
        // Over 1.0 so a bloom pass reads it as glow, and fading out with the level so at
        // rest the track is all there is.
        let lit = METER_GREEN.to_linear() * (1.6 * level.clamp(0.0, 1.0));
        let colour = Color::LinearRgba(lit);
        if let Some(mut asset) = materials.get_mut(&material.0)
            && asset.base_color != colour
        {
            asset.base_color = colour;
        }
    }
}
