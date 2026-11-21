//! Coordinates between Minecraft and the link (CONTRACT section 4).
//!
//! Minecraft's world is right-handed with +Y up and one block is one metre: +X east, +Y up, +Z south
//! (east, up, south is a right-handed triple). So with the default settings a position maps to itself;
//! what differs is how Minecraft names directions, which is what this module converts. The
//! conventions are the ones of `minecraft-link` ("Coordinates"), repeated here so they can be tested
//! without a server:
//!
//! * `yaw` is in degrees, 0 faces +Z (south) and grows clockwise seen from above (90 = west = -X);
//! * `pitch` is in degrees, 0 is level and +90 looks down;
//! * the view direction is `(-sin(yaw)cos(pitch), -sin(pitch), cos(yaw)cos(pitch))`;
//! * the entity rotation on the wire is `Ry(-yaw) * Rx(pitch)`, so the entity's front (+Z, CONTRACT
//!   section 4) is the view direction;
//! * velocities are metres per second.
//!
//! `flip_x`, `flip_z`, `scale` and `offset` line two worlds up, as in the `minecraft-link` config:
//! `link = flip(position) * scale + offset`. A flip mirrors the world, so a direction is mirrored the
//! same way before it is turned into a rotation, and a triangle's winding is reversed when exactly one
//! axis is flipped (a mirror turns counter-clockwise into clockwise, CONTRACT section 4: triangles wind
//! counter-clockwise seen from the front).

use std::f64::consts::PI;

/// How a Minecraft world is placed in the link's space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Mapping {
    pub flip_x: bool,
    pub flip_z: bool,
    /// Link metres per block.
    pub scale: f64,
    /// Link position of the Minecraft origin, after the flip and the scale.
    pub offset: [f64; 3],
}

impl Default for Mapping {
    fn default() -> Self {
        Self {
            flip_x: false,
            flip_z: false,
            scale: 1.0,
            offset: [0.0; 3],
        }
    }
}

fn f32x3(v: [f64; 3]) -> [f32; 3] {
    [v[0] as f32, v[1] as f32, v[2] as f32]
}

impl Mapping {
    fn flip(&self, v: [f64; 3]) -> [f64; 3] {
        [
            if self.flip_x { -v[0] } else { v[0] },
            v[1],
            if self.flip_z { -v[2] } else { v[2] },
        ]
    }

    /// True when the mapping is a reflection (exactly one horizontal axis flipped).
    pub fn mirrors(&self) -> bool {
        self.flip_x != self.flip_z
    }

    /// A Minecraft position (blocks) in link space (metres).
    pub fn position(&self, p: [f64; 3]) -> [f32; 3] {
        let f = self.flip(p);
        f32x3([
            f[0] * self.scale + self.offset[0],
            f[1] * self.scale + self.offset[1],
            f[2] * self.scale + self.offset[2],
        ])
    }

    /// A link position back in Minecraft blocks.
    pub fn position_back(&self, p: [f32; 3]) -> [f64; 3] {
        let s = if self.scale == 0.0 { 1.0 } else { self.scale };
        self.flip([
            (f64::from(p[0]) - self.offset[0]) / s,
            (f64::from(p[1]) - self.offset[1]) / s,
            (f64::from(p[2]) - self.offset[2]) / s,
        ])
    }

    /// A displacement in blocks over `seconds`, as a velocity in link metres per second.
    pub fn velocity(&self, delta_blocks: [f64; 3], seconds: f64) -> [f32; 3] {
        let f = self.flip(delta_blocks);
        let k = self.scale / seconds.max(1e-3);
        f32x3([f[0] * k, f[1] * k, f[2] * k])
    }

    /// The rotation of an entity looking along `yaw`/`pitch` (degrees), as a unit quaternion
    /// `x, y, z, w` in link space.
    pub fn rotation(&self, yaw_deg: f32, pitch_deg: f32) -> [f32; 4] {
        // The yaw is mirrored on its own, from the horizontal direction it names: through the view direction a
        // player looking straight up or down would have no horizontal direction and lose the yaw. A mirror in X
        // or Z leaves the pitch as it is.
        let d = view_direction(f64::from(yaw_deg), 0.0);
        let (yaw, _) = direction_to_yaw_pitch(self.flip(d));
        yaw_pitch_to_quat(yaw, f64::from(pitch_deg).clamp(-90.0, 90.0))
    }

    /// The `yaw`, `pitch` (degrees) of a link rotation, in Minecraft's naming; the inverse of
    /// [`Mapping::rotation`] up to roll, which Minecraft cannot express.
    pub fn yaw_pitch_back(&self, q: [f32; 4]) -> (f32, f32) {
        let d = rotate_z(q);
        let (yaw, pitch) = direction_to_yaw_pitch(self.flip(d));
        (yaw as f32, pitch as f32)
    }

    /// Angular velocity (axis times rad/s, link space) of an entity whose yaw changed by
    /// `delta_yaw_deg` over `seconds`. Yaw grows clockwise seen from above while the rotation is
    /// `Ry(-yaw)`, so the angular velocity about +Y is `-dyaw`; a mirror turns it round again.
    pub fn angular_velocity(&self, delta_yaw_deg: f32, seconds: f64) -> [f32; 3] {
        let d = f64::from(wrap_degrees(delta_yaw_deg)).to_radians() / seconds.max(1e-3);
        let sign = if self.mirrors() { 1.0 } else { -1.0 };
        [0.0, (sign * d) as f32, 0.0]
    }

    /// A triangle of the Minecraft world in link space, reordered when the mapping mirrors so that it
    /// still winds counter-clockwise seen from its front.
    pub fn triangle(&self, t: [[f64; 3]; 3]) -> [[f32; 3]; 3] {
        let a = self.position(t[0]);
        let b = self.position(t[1]);
        let c = self.position(t[2]);
        if self.mirrors() { [a, c, b] } else { [a, b, c] }
    }
}

/// An angle in degrees wrapped to `-180 ..= 180`.
pub fn wrap_degrees(d: f32) -> f32 {
    let mut x = d % 360.0;
    if x > 180.0 {
        x -= 360.0;
    } else if x < -180.0 {
        x += 360.0;
    }
    x
}

/// The unit view direction of a Minecraft `yaw`/`pitch` in degrees.
pub fn view_direction(yaw_deg: f64, pitch_deg: f64) -> [f64; 3] {
    let (y, p) = (yaw_deg.to_radians(), pitch_deg.to_radians());
    [-y.sin() * p.cos(), -p.sin(), y.cos() * p.cos()]
}

/// Minecraft's `yaw`, `pitch` in degrees for a direction; yaw is 0 for a vertical direction.
pub fn direction_to_yaw_pitch(d: [f64; 3]) -> (f64, f64) {
    let horizontal = (d[0] * d[0] + d[2] * d[2]).sqrt();
    let pitch = (-d[1]).atan2(horizontal) * 180.0 / PI;
    let yaw = if horizontal < 1e-9 {
        0.0
    } else {
        (-d[0]).atan2(d[2]) * 180.0 / PI
    };
    (yaw, pitch)
}

/// `Ry(-yaw) * Rx(pitch)` as a quaternion `x, y, z, w`.
pub fn yaw_pitch_to_quat(yaw_deg: f64, pitch_deg: f64) -> [f32; 4] {
    let hy = (-yaw_deg).to_radians() / 2.0;
    let hp = pitch_deg.to_radians() / 2.0;
    let (qy, qx) = (
        [0.0, hy.sin(), 0.0, hy.cos()],
        [hp.sin(), 0.0, 0.0, hp.cos()],
    );
    let q = quat_mul(qy, qx);
    let n = q.iter().map(|v| v * v).sum::<f64>().sqrt();
    [
        (q[0] / n) as f32,
        (q[1] / n) as f32,
        (q[2] / n) as f32,
        (q[3] / n) as f32,
    ]
}

fn quat_mul(a: [f64; 4], b: [f64; 4]) -> [f64; 4] {
    [
        a[3] * b[0] + a[0] * b[3] + a[1] * b[2] - a[2] * b[1],
        a[3] * b[1] - a[0] * b[2] + a[1] * b[3] + a[2] * b[0],
        a[3] * b[2] + a[0] * b[1] - a[1] * b[0] + a[2] * b[3],
        a[3] * b[3] - a[0] * b[0] - a[1] * b[1] - a[2] * b[2],
    ]
}

/// The quaternion's local +Z axis in the parent space (the entity's front, CONTRACT section 4).
pub fn rotate_z(q: [f32; 4]) -> [f64; 3] {
    let n = q.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt();
    if n == 0.0 || !n.is_finite() {
        return [0.0, 0.0, 1.0];
    }
    let (x, y, z, w) = (
        f64::from(q[0]) / n,
        f64::from(q[1]) / n,
        f64::from(q[2]) / n,
        f64::from(q[3]) / n,
    );
    [
        2.0 * (x * z + w * y),
        2.0 * (y * z - w * x),
        1.0 - 2.0 * (x * x + y * y),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The quaternion's local +Y axis (the entity's up) in the parent space.
    fn rotate_y(q: [f32; 4]) -> [f64; 3] {
        let (x, y, z, w) = (
            f64::from(q[0]),
            f64::from(q[1]),
            f64::from(q[2]),
            f64::from(q[3]),
        );
        [
            2.0 * (x * y - w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z + w * x),
        ]
    }

    fn close(a: [f64; 3], b: [f64; 3]) {
        for i in 0..3 {
            assert!((a[i] - b[i]).abs() < 1e-5, "{a:?} != {b:?}");
        }
    }

    #[test]
    fn default_mapping_is_the_identity_on_positions() {
        let m = Mapping::default();
        assert_eq!(m.position([1.5, 64.0, -3.25]), [1.5, 64.0, -3.25]);
        close(m.position_back([1.5, 64.0, -3.25]), [1.5, 64.0, -3.25]);
    }

    #[test]
    fn flip_scale_and_offset_apply_in_that_order_and_invert() {
        let m = Mapping {
            flip_x: true,
            flip_z: false,
            scale: 2.0,
            offset: [10.0, -4.0, 1.0],
        };
        // flip: (-1, 3, 5); scale: (-2, 6, 10); offset: (8, 2, 11)
        assert_eq!(m.position([1.0, 3.0, 5.0]), [8.0, 2.0, 11.0]);
        close(m.position_back([8.0, 2.0, 11.0]), [1.0, 3.0, 5.0]);
    }

    #[test]
    fn yaw_zero_is_the_identity_and_faces_plus_z() {
        let m = Mapping::default();
        let q = m.rotation(0.0, 0.0);
        assert!(
            q[0].abs() < 1e-6
                && q[1].abs() < 1e-6
                && q[2].abs() < 1e-6
                && (q[3] - 1.0).abs() < 1e-6
        );
        close(rotate_z(q), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn yaw_90_faces_minus_x_and_yaw_minus_90_faces_plus_x() {
        let m = Mapping::default();
        close(rotate_z(m.rotation(90.0, 0.0)), [-1.0, 0.0, 0.0]);
        close(rotate_z(m.rotation(-90.0, 0.0)), [1.0, 0.0, 0.0]);
        close(rotate_z(m.rotation(180.0, 0.0)), [0.0, 0.0, -1.0]);
    }

    #[test]
    fn pitch_plus_90_faces_down() {
        let m = Mapping::default();
        close(rotate_z(m.rotation(0.0, 90.0)), [0.0, -1.0, 0.0]);
        close(rotate_z(m.rotation(0.0, -90.0)), [0.0, 1.0, 0.0]);
    }

    #[test]
    fn looking_straight_up_or_down_keeps_the_yaw() {
        for map in [
            Mapping::default(),
            Mapping {
                flip_x: true,
                ..Mapping::default()
            },
        ] {
            for pitch in [90.0f32, -90.0] {
                let level = map.rotation(90.0, 0.0);
                let vertical = map.rotation(90.0, pitch);
                // the entity's up (+Y) tilts towards where it faced: its horizontal part is the level front
                let up = rotate_y(vertical);
                let front = rotate_z(level);
                let sign = if pitch > 0.0 { 1.0 } else { -1.0 };
                close([up[0] * sign, 0.0, up[2] * sign], [front[0], 0.0, front[2]]);
            }
        }
    }

    #[test]
    fn rotation_front_is_the_view_direction_for_many_angles() {
        let m = Mapping::default();
        for yaw in (-180..=180).step_by(23) {
            for pitch in (-80..=80).step_by(17) {
                let want = view_direction(f64::from(yaw), f64::from(pitch));
                close(rotate_z(m.rotation(yaw as f32, pitch as f32)), want);
            }
        }
    }

    #[test]
    fn rotation_is_a_unit_quaternion() {
        let m = Mapping::default();
        let q = m.rotation(37.0, -22.0);
        let n: f32 = q.iter().map(|v| v * v).sum();
        assert!((n - 1.0).abs() < 1e-6);
    }

    #[test]
    fn yaw_pitch_round_trips() {
        for m in [
            Mapping::default(),
            Mapping {
                flip_x: true,
                ..Mapping::default()
            },
            Mapping {
                flip_x: true,
                flip_z: true,
                ..Mapping::default()
            },
        ] {
            for (yaw, pitch) in [(0.0, 0.0), (45.0, 10.0), (-120.0, -30.0), (170.0, 60.0)] {
                let (y, p) = m.yaw_pitch_back(m.rotation(yaw, pitch));
                assert!((wrap_degrees(y - yaw)).abs() < 1e-3, "yaw {yaw} -> {y}");
                assert!((p - pitch).abs() < 1e-3, "pitch {pitch} -> {p}");
            }
        }
    }

    #[test]
    fn flip_x_mirrors_the_facing() {
        let m = Mapping {
            flip_x: true,
            ..Mapping::default()
        };
        // facing -X (yaw 90) becomes facing +X once the world is mirrored along X
        close(rotate_z(m.rotation(90.0, 0.0)), [1.0, 0.0, 0.0]);
        // facing +Z is unchanged
        close(rotate_z(m.rotation(0.0, 0.0)), [0.0, 0.0, 1.0]);
    }

    #[test]
    fn velocity_is_metres_per_second() {
        let m = Mapping::default();
        // 0.2 blocks in one 50 ms tick is 4 m/s
        let v = m.velocity([0.2, 0.0, -0.1], 0.05);
        assert!((v[0] - 4.0).abs() < 1e-5 && (v[2] + 2.0).abs() < 1e-5);
        let s = Mapping {
            scale: 3.0,
            ..Mapping::default()
        };
        assert!((s.velocity([0.2, 0.0, 0.0], 0.05)[0] - 12.0).abs() < 1e-4);
    }

    #[test]
    fn angular_velocity_follows_the_rotation_sign() {
        let m = Mapping::default();
        // the yaw grows by 90 degrees in one second: Ry(-yaw) turns by -pi/2 rad about +Y
        let w = m.angular_velocity(90.0, 1.0);
        assert!((w[1] + std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        // across the wrap: 179 -> -179 is +2 degrees, not -358
        let w = m.angular_velocity(-179.0 - 179.0, 1.0);
        assert!((w[1] + 2.0f32.to_radians()).abs() < 1e-5);
    }

    #[test]
    fn a_mirror_reverses_triangle_winding_and_a_double_flip_does_not() {
        let t = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]];
        assert_eq!(Mapping::default().triangle(t)[1], [1.0, 0.0, 0.0]);
        let one = Mapping {
            flip_x: true,
            ..Mapping::default()
        };
        assert_eq!(one.triangle(t)[1], [0.0, 0.0, 1.0]);
        let two = Mapping {
            flip_x: true,
            flip_z: true,
            ..Mapping::default()
        };
        assert_eq!(two.triangle(t)[1], [-1.0, 0.0, 0.0]);
    }

    #[test]
    fn wrap_degrees_stays_in_range() {
        assert_eq!(wrap_degrees(190.0), -170.0);
        assert_eq!(wrap_degrees(-190.0), 170.0);
        assert_eq!(wrap_degrees(360.0), 0.0);
    }
}
