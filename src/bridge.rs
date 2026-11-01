//! The link session of the server: everything between the `rewrite-bridge` [`Link`] and the server's
//! players, blocks and guest entities, without any dependency on the server's API, so it runs and is
//! tested natively. The plugin glue (`plugin.rs`) hands it the server's state once per tick.
//!
//! Roles, and what a server without a renderer can do of the contract (CONTRACT section 1 table):
//!
//! | | Host (the server listens) | Guest (the server connects) |
//! |---|---|---|
//! | `hello`, `bye`, `Heartbeat`, token | yes | yes |
//! | `spawn`/`despawn`/`Pose` sent | the players, as host entities | the players, as guest entities |
//! | `spawn`/`despawn`/`Pose` received | the guest's entities, shown by the glue | counted, not shown |
//! | `Collision` | sent, around one player | received, counted |
//! | `Camera`, `Frame`, pixel file | not sent (no renderer); received ones are counted and ignored | same |
//! | `input` | the guest's request is ignored (section 8 lets a host ignore it) | a grant is noted |

use crate::collision::{BlockSource, Scan, Snapshot, fingerprint};
use crate::config::{Config, Role};
use crate::endpoint;
use crate::mapping::{Mapping, wrap_degrees};
use crate::session::{Session, SessionConfig};
use rewrite_bridge::link::{
    self, Event, MAX_ENTITIES, POSE_TELEPORT, Pose, Spawn, UNAUTHORIZED, now_us,
};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The game name sent in `hello` (informative, CONTRACT section 8).
pub const GAME: &str = "Minecraft (Pumpkin server)";

/// A player moving farther than this in one tick is teleported, not interpolated (the value
/// `minecraft-link` uses for its own player).
pub const TELEPORT_BLOCKS: f64 = 8.0;

/// Horizontal layers of the collision scan read per tick.
pub const SCAN_LAYERS_PER_TICK: usize = 1;

/// A snapshot that did not change is sent again after this long (as `minecraft-link` does).
pub const COLLISION_REFRESH: Duration = Duration::from_secs(5);

/// One online player, as the server reports it.
#[derive(Clone, Debug, PartialEq)]
pub struct PlayerSample {
    /// Stable for the player's session (the UUID as text).
    pub key: String,
    pub name: String,
    /// The world the player is in (its id).
    pub world: String,
    /// Feet position, blocks.
    pub pos: [f64; 3],
    /// Minecraft yaw and pitch, degrees.
    pub yaw: f32,
    pub pitch: f32,
}

/// Where the glue shows the guest's entities.
pub trait GuestSink {
    fn spawn(&mut self, entity: u32, spawn: &Spawn);
    /// Pose of a live guest entity, back in Minecraft's terms.
    fn pose(&mut self, entity: u32, pos: [f64; 3], yaw: f32, pitch: f32, teleport: bool);
    fn despawn(&mut self, entity: u32);
    /// The session ended: remove everything.
    fn clear(&mut self);
}

/// A sink that shows nothing.
pub struct NoGuests;

impl GuestSink for NoGuests {
    fn spawn(&mut self, _: u32, _: &Spawn) {}
    fn pose(&mut self, _: u32, _: [f64; 3], _: f32, _: f32, _: bool) {}
    fn despawn(&mut self, _: u32) {}
    fn clear(&mut self) {}
}

/// The blocks of the world a player is in.
pub trait Worlds {
    fn blocks(&mut self, world: &str) -> Option<&mut dyn BlockSource>;
}

/// Worlds that have no blocks.
pub struct NoWorlds;

impl Worlds for NoWorlds {
    fn blocks(&mut self, _: &str) -> Option<&mut dyn BlockSource> {
        None
    }
}

struct Tracked {
    entity: u32,
    name: String,
    spawned: bool,
    last: Option<([f64; 3], f32, Instant)>,
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct Counters {
    pub sessions: u64,
    pub lost: u64,
    pub refused_unauthorized: u64,
    pub handshake_failed: u64,
    pub poses_sent: u64,
    pub poses_received: u64,
    pub collision_sent: u64,
    pub collision_cut: u64,
    pub collision_received: u64,
    pub cameras_ignored: u64,
    pub frames_ignored: u64,
    pub players_over_limit: u64,
}

#[derive(Default)]
struct Collider {
    scan: Option<Scan>,
    scan_world: String,
    last_start: Option<Instant>,
    last_centre: Option<[f64; 3]>,
    last_hash: Option<u64>,
    last_sent: Option<Instant>,
    latest: Option<(usize, usize, f32)>, // (triangles sent, dropped, radius)
}

/// What the server hands the bridge each tick.
pub struct Tick<'a> {
    pub now: Instant,
    pub players: &'a [PlayerSample],
    pub worlds: &'a mut dyn Worlds,
    pub guests: &'a mut dyn GuestSink,
}

pub struct Bridge {
    cfg: Config,
    endpoint_path: PathBuf,
    link: Option<Session>,
    role: Role,
    token_active: bool,
    wrote_endpoint: bool,
    tracked: HashMap<String, Tracked>,
    next_entity: u32,
    peer_entities: HashMap<u32, Spawn>,
    focus_with_guest: bool,
    collider: Collider,
    pub counters: Counters,
    last_problem: Option<String>,
    last_lost: Option<String>,
}

impl Bridge {
    pub fn new(cfg: Config, endpoint_path: PathBuf) -> Self {
        let role = cfg.role;
        let next_entity = cfg.guest_entity;
        Self {
            cfg,
            endpoint_path,
            link: None,
            role,
            token_active: false,
            wrote_endpoint: false,
            tracked: HashMap::new(),
            next_entity,
            peer_entities: HashMap::new(),
            focus_with_guest: false,
            collider: Collider::default(),
            counters: Counters::default(),
            last_problem: None,
            last_lost: None,
        }
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn is_running(&self) -> bool {
        self.link.is_some()
    }

    pub fn is_connected(&self) -> bool {
        self.link.as_ref().is_some_and(Session::is_connected)
    }

    /// The control port actually bound (host) or being connected to (guest).
    pub fn control_port(&self) -> Option<u16> {
        self.link
            .as_ref()
            .and_then(|l| l.control_addr())
            .map(|a| a.port())
    }

    /// Starts `role`. Errors say what failed and never contain the token.
    pub fn start(&mut self, role: Role) -> Result<(), String> {
        if self.link.is_some() {
            return Err("the link is already running; stop it first".into());
        }
        self.role = role;
        let mut lc = SessionConfig::new(GAME);
        lc.control_port = self.cfg.port;
        lc.udp_port = self.cfg.udp_port;
        lc.heartbeat = Duration::from_millis(u64::from(self.cfg.heartbeat_ms));
        lc.timeout = Duration::from_millis(u64::from(self.cfg.timeout_ms));
        let mut token = None;
        match (role, self.cfg.token_auth) {
            (Role::Host, true) => {
                let t = endpoint::generate_token().map_err(|e| format!("token: {e}"))?;
                lc.token = Some(t.clone());
                token = Some(t);
            }
            (Role::Guest, true) => lc.endpoint_file = Some(self.endpoint_path.clone()),
            (_, false) => {}
        }
        let link = match role {
            Role::Host => Session::host(lc),
            Role::Guest => Session::guest(lc),
        }
        .map_err(|e| format!("cannot start the {}: {e}", role_name(role)))?;
        if let Some(t) = token {
            // Only now, with the control port bound (CONTRACT section 6.2, "Writing").
            let port = link
                .control_addr()
                .map(|a| a.port())
                .ok_or("the host has no control port")?;
            endpoint::write(&self.endpoint_path, port, &t)
                .map_err(|e| format!("cannot write the endpoint file: {e}"))?;
            self.wrote_endpoint = true;
        }
        self.token_active = self.cfg.token_auth;
        self.link = Some(link);
        self.last_problem = None;
        self.counters = Counters::default();
        Ok(())
    }

    /// Ends the session with a `bye` and releases the ports and the endpoint file.
    pub fn stop(&mut self, guests: &mut dyn GuestSink) {
        if let Some(link) = self.link.take() {
            // `bye` if there is a session, half-close, drain for at most 200 ms (CONTRACT section 6.1)
            link.shutdown();
        }
        if self.wrote_endpoint {
            endpoint::remove(&self.endpoint_path);
            self.wrote_endpoint = false;
        }
        guests.clear();
        self.peer_entities.clear();
        for t in self.tracked.values_mut() {
            t.spawned = false;
            t.last = None;
        }
        self.collider = Collider::default();
        self.focus_with_guest = false;
    }

    /// One server tick: polls the link, reacts to its events, publishes the players and, as a host,
    /// advances the collision scan.
    pub fn tick(&mut self, mut t: Tick<'_>) {
        let Some(mut link) = self.link.take() else {
            return;
        };
        let events = link.poll();
        for e in events {
            self.on_event(e, &mut *t.guests);
        }
        if link.is_connected() {
            self.publish_players(&mut link, &t);
            if self.role == Role::Host {
                self.drive_collision(&mut link, &mut t);
            }
        } else {
            self.collider = Collider::default();
        }
        self.link = Some(link);
    }

    fn on_event(&mut self, e: Event, guests: &mut dyn GuestSink) {
        match e {
            Event::Connected(_) => {
                self.counters.sessions += 1;
                self.last_problem = None;
                self.last_lost = None;
                for t in self.tracked.values_mut() {
                    t.spawned = false;
                    t.last = None;
                }
                self.peer_entities.clear();
                self.focus_with_guest = false;
                self.collider = Collider::default();
            }
            Event::Lost { reason } => {
                self.counters.lost += 1;
                self.last_lost = Some(reason);
                guests.clear();
                self.peer_entities.clear();
                self.focus_with_guest = false;
                for t in self.tracked.values_mut() {
                    t.spawned = false;
                }
            }
            Event::HandshakeFailed { reason } => {
                if reason == UNAUTHORIZED {
                    self.counters.refused_unauthorized += 1;
                } else {
                    self.counters.handshake_failed += 1;
                    self.last_problem = Some(reason);
                }
            }
            Event::Spawn(s) => {
                if self.role == Role::Host && self.cfg.show_guests {
                    guests.spawn(s.entity, &s);
                }
                self.peer_entities.insert(s.entity, s);
            }
            Event::Despawn { entity } => {
                self.peer_entities.remove(&entity);
                guests.despawn(entity);
            }
            Event::Poses { poses, .. } => {
                for p in poses {
                    self.counters.poses_received += 1;
                    if self.role == Role::Host && self.cfg.show_guests {
                        let finite = p
                            .translation
                            .iter()
                            .chain(p.rotation.iter())
                            .all(|v| v.is_finite());
                        // CONTRACT 9.4: a receiver skips a record whose numbers are not finite.
                        if finite && self.peer_entities.contains_key(&p.entity) {
                            let pos = self.cfg.mapping.position_back(p.translation);
                            let (yaw, pitch) = self.cfg.mapping.yaw_pitch_back(p.rotation);
                            guests.pose(p.entity, pos, yaw, pitch, p.flags & POSE_TELEPORT != 0);
                        }
                    }
                }
            }
            Event::Collision(c) => self.counters.collision_received += c.triangles.len() as u64,
            Event::Camera { .. } => self.counters.cameras_ignored += 1,
            Event::Frame { .. } | Event::FrameChannel(_) => self.counters.frames_ignored += 1,
            Event::Focus { focus, requested } => {
                // A host may ignore a guest's request (section 8, `input`); a grant from the host is noted.
                if !requested {
                    self.focus_with_guest = focus == link::Role::Guest;
                }
            }
        }
    }

    fn publish_players(&mut self, link: &mut Session, t: &Tick<'_>) {
        let map = self.cfg.mapping;
        // players that left
        let gone: Vec<String> = self
            .tracked
            .keys()
            .filter(|k| !t.players.iter().any(|p| &p.key == *k))
            .cloned()
            .collect();
        for k in gone {
            if let Some(tr) = self.tracked.remove(&k)
                && tr.spawned
            {
                let _ = link.despawn(tr.entity);
            }
        }
        let mut poses = Vec::with_capacity(t.players.len());
        for p in t.players {
            if !self.tracked.contains_key(&p.key) {
                if self.tracked.len() >= MAX_ENTITIES {
                    self.counters.players_over_limit += 1;
                    continue;
                }
                let entity = self.next_entity;
                self.next_entity = self.next_entity.wrapping_add(1);
                self.tracked.insert(
                    p.key.clone(),
                    Tracked {
                        entity,
                        name: p.name.clone(),
                        spawned: false,
                        last: None,
                    },
                );
            }
            let Some(tr) = self.tracked.get_mut(&p.key) else {
                continue;
            };
            if !tr.spawned {
                let spawn = Spawn {
                    entity: tr.entity,
                    provider: self.cfg.guest_provider.clone(),
                    asset: self.cfg.guest_asset.clone(),
                    name: Some(tr.name.clone()),
                };
                if link.spawn(&spawn).is_err() {
                    continue;
                }
                tr.spawned = true;
            }
            poses.push(make_pose(&map, tr, p, t.now));
        }
        if !poses.is_empty() {
            let n = poses.len() as u64;
            if link.send_poses(&poses).is_ok() {
                self.counters.poses_sent += n;
            }
        }
    }

    fn drive_collision(&mut self, link: &mut Session, t: &mut Tick<'_>) {
        let c = &mut self.collider;
        let centre = if self.cfg.collision_player.is_empty() {
            t.players.first()
        } else {
            t.players
                .iter()
                .find(|p| p.name == self.cfg.collision_player)
        };
        let Some(centre) = centre else {
            c.scan = None;
            return;
        };
        if c.scan.is_none() {
            let moved = self.cfg.collision_move_blocks > 0.0
                && c.last_centre.is_none_or(|l| {
                    let d = [
                        l[0] - centre.pos[0],
                        l[1] - centre.pos[1],
                        l[2] - centre.pos[2],
                    ];
                    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
                        >= self.cfg.collision_move_blocks
                });
            let due = c.last_start.is_none_or(|s| {
                t.now.duration_since(s) >= Duration::from_millis(self.cfg.collision_interval_ms)
            });
            if !(due || moved) {
                return;
            }
            c.scan = Some(Scan::new(centre.pos, self.cfg.collision_radius));
            c.scan_world = centre.world.clone();
            c.last_start = Some(t.now);
            c.last_centre = Some(centre.pos);
        }
        let Some(blocks) = t.worlds.blocks(&c.scan_world) else {
            c.scan = None;
            return;
        };
        let Some(scan) = c.scan.as_mut() else {
            return;
        };
        scan.advance(blocks, SCAN_LAYERS_PER_TICK);
        if !scan.is_done() {
            return;
        }
        let snap = scan.build(&self.cfg.mapping);
        c.scan = None;
        let hash = fingerprint(&snap);
        let stale = c
            .last_sent
            .is_none_or(|s| t.now.duration_since(s) >= COLLISION_REFRESH);
        if c.last_hash == Some(hash) && !stale {
            return;
        }
        self.send_snapshot(link, snap, hash, t.now);
    }

    fn send_snapshot(&mut self, link: &mut Session, snap: Snapshot, hash: u64, now: Instant) {
        match link.send_collision(snap.centre, snap.radius, &snap.triangles) {
            Ok(_) => {
                let c = &mut self.collider;
                c.last_hash = Some(hash);
                c.last_sent = Some(now);
                c.latest = Some((snap.triangles.len(), snap.dropped, snap.radius));
                self.counters.collision_sent += 1;
                if snap.dropped > 0 {
                    self.counters.collision_cut += 1;
                }
            }
            Err(e) => self.last_problem = Some(format!("collision snapshot not sent: {e}")),
        }
    }

    /// The lines of `/link status`. The token is never in them.
    pub fn status(&self) -> Vec<String> {
        let mut out = Vec::new();
        let Some(link) = &self.link else {
            out.push(format!(
                "link: stopped (configured role {}, control port {})",
                role_name(self.cfg.role),
                self.cfg.port
            ));
            return out;
        };
        let port = link.control_addr().map_or(0, |a| a.port());
        let mut head = format!("link: {} on 127.0.0.1:{port}", role_name(self.role));
        if self.token_active {
            head.push_str(", token auth on");
            if self.counters.refused_unauthorized > 0 {
                head.push_str(&format!(
                    ", {} refused as unauthorized",
                    self.counters.refused_unauthorized
                ));
            }
        }
        out.push(head);
        match link.peer() {
            Some(p) if link.is_connected() => {
                out.push(format!(
                    "peer: {} ({}), protocol {}, session {}, udp {}",
                    p.game,
                    if p.role == link::Role::Host {
                        "host"
                    } else {
                        "guest"
                    },
                    p.protocol,
                    p.session,
                    p.udp_addr.port()
                ));
                out.push(format!(
                    "input focus: {}",
                    if self.focus_with_guest {
                        "guest"
                    } else {
                        "host"
                    }
                ));
            }
            _ => out.push(match self.role {
                Role::Host => "peer: none (waiting for a guest)".into(),
                Role::Guest => "peer: none (connecting to a host)".into(),
            }),
        }
        let s = link.stats();
        out.push(format!(
            "datagrams: {} sent, {} received, {} malformed, {} stale, {} foreign, {} dropped on send",
            s.datagrams_sent, s.datagrams_received, s.malformed, s.stale, s.foreign, s.send_dropped
        ));
        let c = &self.counters;
        out.push(format!(
            "players published: {} ({} poses sent); peer entities: {} ({} poses received)",
            self.tracked.values().filter(|t| t.spawned).count(),
            c.poses_sent,
            self.peer_entities.len(),
            c.poses_received
        ));
        if self.role == Role::Host {
            match self.collider.latest {
                Some((n, dropped, radius)) => out.push(format!(
                    "collision: {} snapshots sent, latest {n} triangles within {radius} m, \
                     limit {}, {} snapshots cut to the nearest triangles ({dropped} dropped in the latest)",
                    c.collision_sent,
                    crate::collision::MAX_SNAPSHOT_TRIANGLES,
                    c.collision_cut
                )),
                None => out.push("collision: no snapshot sent yet".into()),
            }
        } else {
            out.push(format!(
                "collision received: {} triangles",
                c.collision_received
            ));
        }
        out.push(format!(
            "not used by a server: {} cameras and {} frame messages ignored",
            c.cameras_ignored, c.frames_ignored
        ));
        out.push(format!("sessions: {}, lost: {}", c.sessions, c.lost));
        if let Some(e) = link.last_error() {
            out.push(format!("connection: {e}"));
        }
        if let Some(r) = &self.last_lost {
            out.push(format!("last session ended: {r}"));
        }
        if let Some(p) = &self.last_problem {
            out.push(format!("problem: {p}"));
        }
        out
    }

    /// Host: gives the input focus to the guest or takes it back.
    pub fn set_focus(&mut self, focus: link::Role) -> Result<(), String> {
        let link = self.link.as_mut().ok_or("the link is not running")?;
        if self.role != Role::Host {
            return Err(
                "only a host gives the focus; a guest asks, and this server never needs it".into(),
            );
        }
        link.set_focus(focus).map_err(|e| e.to_string())?;
        self.focus_with_guest = focus == link::Role::Guest;
        Ok(())
    }
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Host => "host",
        Role::Guest => "guest",
    }
}

/// The pose of a player: feet position, view rotation, the real displacement since the last sample as
/// velocity, the yaw change as angular velocity about +Y, and TELEPORT on the first sample and after a
/// jump of more than [`TELEPORT_BLOCKS`] (as `minecraft-link` does for its player).
fn make_pose(map: &Mapping, tr: &mut Tracked, p: &PlayerSample, now: Instant) -> Pose {
    let mut pose = Pose::new(tr.entity, now_us());
    pose.translation = map.position(p.pos);
    pose.rotation = map.rotation(p.yaw, p.pitch);
    match tr.last {
        None => pose.flags |= POSE_TELEPORT,
        Some((pos, yaw, at)) => {
            let d = [p.pos[0] - pos[0], p.pos[1] - pos[1], p.pos[2] - pos[2]];
            let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
            let dt = now.duration_since(at).as_secs_f64();
            if dist > TELEPORT_BLOCKS {
                pose.flags |= POSE_TELEPORT;
            } else {
                pose.velocity = map.velocity(d, dt);
                pose.angular_velocity = map.angular_velocity(wrap_degrees(p.yaw - yaw), dt);
            }
        }
    }
    tr.last = Some((p.pos, p.yaw, now));
    pose
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracked() -> Tracked {
        Tracked {
            entity: 1,
            name: "a".into(),
            spawned: true,
            last: None,
        }
    }

    fn sample(x: f64, yaw: f32) -> PlayerSample {
        PlayerSample {
            key: "k".into(),
            name: "a".into(),
            world: "w".into(),
            pos: [x, 64.0, 0.0],
            yaw,
            pitch: 0.0,
        }
    }

    #[test]
    fn the_first_pose_teleports_and_later_ones_carry_velocity() {
        let map = Mapping::default();
        let mut tr = tracked();
        let t0 = Instant::now();
        let a = make_pose(&map, &mut tr, &sample(0.0, 0.0), t0);
        assert_eq!(a.flags & POSE_TELEPORT, POSE_TELEPORT);
        assert_eq!(a.velocity, [0.0; 3]);
        let b = make_pose(
            &map,
            &mut tr,
            &sample(0.2, 0.0),
            t0 + Duration::from_millis(50),
        );
        assert_eq!(b.flags, 0);
        assert!((b.velocity[0] - 4.0).abs() < 1e-4, "{:?}", b.velocity);
        assert_eq!(b.translation, [0.2, 64.0, 0.0]);
    }

    #[test]
    fn a_jump_of_more_than_eight_blocks_teleports_and_has_no_velocity() {
        let map = Mapping::default();
        let mut tr = tracked();
        let t0 = Instant::now();
        make_pose(&map, &mut tr, &sample(0.0, 0.0), t0);
        let b = make_pose(
            &map,
            &mut tr,
            &sample(8.5, 0.0),
            t0 + Duration::from_millis(50),
        );
        assert_eq!(b.flags & POSE_TELEPORT, POSE_TELEPORT);
        assert_eq!(b.velocity, [0.0; 3]);
        let c = make_pose(
            &map,
            &mut tr,
            &sample(8.5 + 8.0, 0.0),
            t0 + Duration::from_millis(100),
        );
        assert_eq!(c.flags, 0, "exactly 8 blocks is not more than 8");
    }

    #[test]
    fn yaw_change_gives_angular_velocity_across_the_wrap() {
        let map = Mapping::default();
        let mut tr = tracked();
        let t0 = Instant::now();
        make_pose(&map, &mut tr, &sample(0.0, 179.0), t0);
        let b = make_pose(
            &map,
            &mut tr,
            &sample(0.0, -179.0),
            t0 + Duration::from_secs(1),
        );
        assert!((b.angular_velocity[1] + 2.0f32.to_radians()).abs() < 1e-5);
    }

    #[test]
    fn pose_rotation_is_a_unit_quaternion_facing_the_view() {
        let map = Mapping::default();
        let mut tr = tracked();
        let p = make_pose(&map, &mut tr, &sample(0.0, 90.0), Instant::now());
        let f = crate::mapping::rotate_z(p.rotation);
        assert!((f[0] + 1.0).abs() < 1e-5, "{f:?}");
    }

    #[test]
    fn stopped_status_says_so_and_never_panics() {
        let b = Bridge::new(Config::default(), PathBuf::from("endpoint.json"));
        assert!(b.status()[0].starts_with("link: stopped"));
        assert!(!b.is_running());
    }
}
