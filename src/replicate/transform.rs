//! `Transform` over the wire, with smoothing on arrival.
//!
//! Replicas do not write `Transform` directly: they carry a [`Glide`], and a system eases the
//! transform toward it over an interval learned from how updates actually arrive. Measured on a
//! gossip mesh, 20 updates a second arrived with a median gap of 56 ms and a p90 of 102 ms;
//! easing over exactly one nominal interval left half of every second frozen. Interpolating and
//! never extrapolating: being a fiftieth of a second behind is invisible, rubber-banding is not.

use bevy::{prelude::*, time::Real};
use serde::{Deserialize, Serialize};

use super::codec::{Codec, DecodeCx, EncodeCx, Rate, Rejected};

const GLIDE_MIN: f32 = 0.03;
const GLIDE_MAX: f32 = 0.25;
/// How much longer than the learned gap to take, so an ordinary late packet does not stall.
const GLIDE_SPREAD: f32 = 1.75;
/// EMA weight of the newest gap.
const GLIDE_LEARN: f32 = 0.2;

/// The built-in codec for `Transform`: 20 Hz and smoothed by default.
#[derive(Debug, Clone)]
pub struct TransformCodec {
    pub rate: Rate,
    /// `false` snaps to each arrival instead of gliding.
    pub smooth: bool,
}

impl Default for TransformCodec {
    fn default() -> Self {
        Self {
            rate: Rate::Hz(20.0),
            smooth: true,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TransformWire {
    pub translation: [f32; 3],
    /// xyzw
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl From<&Transform> for TransformWire {
    fn from(t: &Transform) -> Self {
        Self {
            translation: t.translation.to_array(),
            rotation: t.rotation.to_array(),
            scale: t.scale.to_array(),
        }
    }
}

impl From<TransformWire> for Transform {
    fn from(w: TransformWire) -> Self {
        Transform {
            translation: Vec3::from_array(w.translation),
            rotation: Quat::from_array(w.rotation).normalize(),
            scale: Vec3::from_array(w.scale),
        }
    }
}

impl Codec for TransformCodec {
    type Source = Transform;
    type Wire = TransformWire;
    type Target = Glide;

    fn name(&self) -> &'static str {
        "bevy_iroh/transform/1"
    }

    fn rate(&self) -> Rate {
        self.rate
    }

    fn encode(&self, src: &Transform, _cx: &EncodeCx) -> Option<TransformWire> {
        Some(src.into())
    }

    fn verify(&self, wire: &TransformWire) -> Result<(), Rejected> {
        let finite = wire
            .translation
            .iter()
            .chain(&wire.rotation)
            .chain(&wire.scale)
            .all(|f| f.is_finite());
        if !finite {
            return Err("transform is not finite".into());
        }
        if wire.rotation.iter().map(|f| f * f).sum::<f32>() < 1e-6 {
            return Err("rotation is a zero quaternion".into());
        }
        Ok(())
    }

    fn decode(&self, wire: TransformWire, cx: &mut DecodeCx) -> Result<Glide, Rejected> {
        let to: Transform = wire.into();
        let now = cx.world.resource::<Time<Real>>().elapsed_secs_f64();
        let nominal = self.rate.interval().max(0.01) as f32;
        let previous = cx.world.get::<Glide>(cx.entity);
        // From where it is drawn, so a burst of arrivals stays continuous.
        let from = cx.world.get::<Transform>(cx.entity).copied().unwrap_or(to);
        let (expect, last_arrival) = match previous {
            Some(g) => {
                let gap = (now - g.last_arrival) as f32;
                let expect = if gap.is_finite() && gap > 0.0 && gap < 2.0 {
                    g.expect * (1.0 - GLIDE_LEARN) + gap * GLIDE_LEARN
                } else {
                    g.expect
                };
                (expect, now)
            }
            None => (nominal, now),
        };
        let over = if self.smooth {
            (expect * GLIDE_SPREAD).clamp(GLIDE_MIN, GLIDE_MAX)
        } else {
            0.0
        };
        Ok(Glide {
            from,
            to,
            progress: if previous.is_some() { 0.0 } else { 1.0 },
            over,
            expect,
            last_arrival,
        })
    }
}

/// Where a replica's transform is heading, and how fast to get there.
#[derive(Component, Debug, Clone)]
pub struct Glide {
    pub from: Transform,
    pub to: Transform,
    progress: f32,
    over: f32,
    expect: f32,
    last_arrival: f64,
}

impl Glide {
    /// The transform the owner last sent.
    pub fn target(&self) -> Transform {
        self.to
    }
}

pub(crate) fn glide(
    mut commands: Commands,
    time: Res<Time>,
    mut glides: Query<(Entity, Option<&mut Transform>, &mut Glide)>,
) {
    let dt = time.delta_secs();
    for (entity, transform, mut glide) in &mut glides {
        let Some(mut transform) = transform else {
            commands.entity(entity).insert(glide.to);
            glide.progress = 1.0;
            continue;
        };
        if glide.progress >= 1.0 {
            continue;
        }
        glide.progress = if glide.over <= 0.0 {
            1.0
        } else {
            (glide.progress + dt / glide.over).min(1.0)
        };
        let t = glide.progress;
        transform.translation = glide.from.translation.lerp(glide.to.translation, t);
        transform.rotation = glide.from.rotation.slerp(glide.to.rotation, t);
        transform.scale = glide.from.scale.lerp(glide.to.scale, t);
    }
}
