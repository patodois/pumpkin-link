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
use crate::mapping::{Mapping, wrap_degrees};
use rewrite_bridge::link::{
    self, Event, Link, LinkConfig, MAX_ENTITIES, POSE_TELEPORT, Pose, Spawn, UNAUTHORIZED, now_us,
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

/// The most characters of one text from the peer (its `game`, the reason it gave) that the bridge keeps.
pub const MAX_PEER_TEXT_CHARS: usize = 128;

/// Text that came from the peer, made fit for a player's chat, the console and a log: every control character
/// (C0, DEL and C1), the line and paragraph separators and the bidirectional controls become a space, and the
/// text is cut to [`MAX_PEER_TEXT_CHARS`] characters, with `...` at the end when it was cut. The library already
/// replaces C0 and DEL (contract revision 8); the rest, and the length, are the plugin's own: a peer line may be
/// 64 KiB long.
pub fn clean_peer_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(MAX_PEER_TEXT_CHARS * 4 + 3));
    for (i, c) in s.chars().enumerate() {
        if i == MAX_PEER_TEXT_CHARS {
            out.push_str("...");
            break;
        }
        let unsafe_char = c.is_control()
            || matches!(c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        out.push(if unsafe_char { ' ' } else { c });
    }
    out
}

/// Where the endpoint file cannot be created readable by its user alone. The library creates it with mode 0600 from the moment it
/// exists on Unix; a WebAssembly component on WASI cannot ask for a mode, so there the file gets what the server
/// process's umask leaves (CONTRACT section 6.2, implementation note under "Writing").
pub const ENDPOINT_MODE_FROM_UMASK: bool = cfg!(target_os = "wasi");

/// What `/link status` and the server log say while a host with a token runs where the endpoint file's mode is
/// the umask's.
pub const ENDPOINT_MODE_WARNING: &str = "endpoint file: a plugin cannot create it with mode 0600, so its mode is what \
     the server's umask leaves; start the server with umask 077, or other users can read the token and a native \
     guest refuses the file";

fn endpoint_mode_warning(
    token_active: bool,
    role: Role,
    mode_from_umask: bool,
) -> Option<&'static str> {
    (token_active && role == Role::Host && mode_from_umask).then_some(ENDPOINT_MODE_WARNING)
}

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
    link: Option<Link>,
    role: Role,
    token_active: bool,
    tracked: HashMap<String, Tracked>,
    next_entity: u32,
    peer_entities: HashMap<u32, Spawn>,
    focus_with_guest: bool,
    collider: Collider,
    pub counters: Counters,
    last_problem: Option<String>,
    last_lost: Option<String>,
    /// The config file could not be read or has an error: the link never starts (it fails closed).
    config_error: Option<String>,
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
            tracked: HashMap::new(),
            next_entity,
            peer_entities: HashMap::new(),
            focus_with_guest: false,
            collider: Collider::default(),
            counters: Counters::default(),
            last_problem: None,
            last_lost: None,
            config_error: None,
        }
    }

    /// A bridge for a config file that could not be read or has an error: it shows the error in the status
    /// and refuses to start, so a broken file never gives a link with the defaults (which have no token).
    pub fn broken_config(error: String, endpoint_path: PathBuf) -> Self {
        let mut b = Self::new(Config::default(), endpoint_path);
        b.config_error = Some(error);
        b
    }

    pub fn config(&self) -> &Config {
        &self.cfg
    }

    pub fn is_running(&self) -> bool {
        self.link.is_some()
    }

    pub fn is_connected(&self) -> bool {
        self.link.as_ref().is_some_and(Link::is_connected)
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
        if let Some(e) = &self.config_error {
            return Err(format!(
                "the link does not start while the config file has an error ({e}); fix the file and restart the server"
            ));
        }
        if self.link.is_some() {
            return Err("the link is already running; stop it first".into());
        }
        self.role = role;
        let mut lc = LinkConfig::new(GAME);
        lc.control_port = self.cfg.port;
        lc.udp_port = self.cfg.udp_port;
        lc.heartbeat = Duration::from_millis(u64::from(self.cfg.heartbeat_ms));
        lc.timeout = Duration::from_millis(u64::from(self.cfg.timeout_ms));
        if self.cfg.token_auth {
            // A host makes the token and writes this file once its control port is bound (CONTRACT section
            // 6.2, "Writing"), and removes it when the link is dropped; a guest reads port and token from it.
            lc.endpoint_file = Some(self.endpoint_path.clone());
        }
        let link = match role {
            Role::Host => Link::host(lc),
            Role::Guest => Link::guest(lc),
        }
        .map_err(|e| format!("cannot start the {}: {e}", role_name(role)))?;
        self.token_active = self.cfg.token_auth;
        self.link = Some(link);
        self.last_problem = None;
        self.counters = Counters::default();
        Ok(())
    }

    /// Ends the session with a `bye` and releases the ports and the endpoint file.
    pub fn stop(&mut self, guests: &mut dyn GuestSink) {
        // Dropping the link says `bye` if there is a session, half-closes, drains for at most 200 ms
        // (CONTRACT section 6.1) and removes the endpoint file it wrote.
        drop(self.link.take());
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
                // may hold the peer's own `bye` reason
                self.last_lost = Some(clean_peer_text(&reason));
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
                    self.last_problem = Some(clean_peer_text(&reason));
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

    fn publish_players(&mut self, link: &mut Link, t: &Tick<'_>) {
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
                let entity = self.allocate_entity();
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

    /// The next entity id for a player: the one after the last given, skipping ids that players still online
    /// hold, so the counter wrapping round (from a `guest_entity` near `u32::MAX`) never gives two players one id.
    /// At most `MAX_ENTITIES` ids are held, so the search ends.
    fn allocate_entity(&mut self) -> u32 {
        let mut entity = self.next_entity;
        while self.tracked.values().any(|t| t.entity == entity) {
            entity = entity.wrapping_add(1);
        }
        self.next_entity = entity.wrapping_add(1);
        entity
    }

    fn drive_collision(&mut self, link: &mut Link, t: &mut Tick<'_>) {
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
            c.scan = Some(Scan::new(
                centre.pos,
                self.cfg.collision_radius,
                self.cfg.mapping,
            ));
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
        let Some(scan) = c.scan.take() else {
            return;
        };
        let snap = scan.finish();
        let hash = fingerprint(&snap);
        let stale = c
            .last_sent
            .is_none_or(|s| t.now.duration_since(s) >= COLLISION_REFRESH);
        if c.last_hash == Some(hash) && !stale {
            return;
        }
        self.send_snapshot(link, snap, hash, t.now);
    }

    fn send_snapshot(&mut self, link: &mut Link, snap: Snapshot, hash: u64, now: Instant) {
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
        if let Some(e) = &self.config_error {
            out.push(format!(
                "config: {e}; the file was left as it is and the link does not start until it is fixed \
                 and the server restarted"
            ));
            return out;
        }
        let Some(link) = &self.link else {
            out.push(format!(
                "link: stopped (configured role {}, control port {})",
                role_name(self.cfg.role),
                self.cfg.port
            ));
            return out;
        };
        // A guest has no control listener (`control_addr` is port 0): it connects to the host's port, from
        // the endpoint file when the token is on, else from the config.
        let mut warning = None;
        let mut head = match self.role {
            Role::Host => {
                let port = link.control_addr().map_or(0, |a| a.port());
                format!("link: host on 127.0.0.1:{port}")
            }
            Role::Guest if self.token_active => {
                "link: guest, connecting to 127.0.0.1 (port from the endpoint file)".to_string()
            }
            Role::Guest => format!("link: guest, connecting to 127.0.0.1:{}", self.cfg.port),
        };
        if self.token_active {
            head.push_str(", token auth on");
            if let Some(w) = self.endpoint_mode_warning() {
                warning = Some(w);
            }
            if self.counters.refused_unauthorized > 0 {
                head.push_str(&format!(
                    ", {} refused as unauthorized",
                    self.counters.refused_unauthorized
                ));
            }
        }
        out.push(head);
        out.extend(warning.map(str::to_owned));
        match link.peer() {
            Some(p) if link.is_connected() => {
                out.push(format!(
                    "peer: {} ({}), protocol {}, session {}, udp {}",
                    clean_peer_text(&p.game),
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
            out.push(format!("connection: {}", clean_peer_text(e)));
        }
        if let Some(r) = &self.last_lost {
            out.push(format!("last session ended: {r}"));
        }
        if let Some(p) = &self.last_problem {
            out.push(format!("problem: {p}"));
        }
        out
    }

    /// A host with a token that runs where the endpoint file cannot be created with mode 0600: the warning to log and
    /// show (see [`ENDPOINT_MODE_FROM_UMASK`]).
    pub fn endpoint_mode_warning(&self) -> Option<&'static str> {
        endpoint_mode_warning(
            self.is_running() && self.token_active,
            self.role,
            ENDPOINT_MODE_FROM_UMASK,
        )
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
    fn peer_text_loses_every_control_character_and_is_cut() {
        let evil = "a\u{1b}[31mb\nFORGED\r\u{7f}\u{85}\u{9b}2J\u{2028}\u{202e}\u{2066}\0z";
        let c = clean_peer_text(evil);
        assert_eq!(c, "a [31mb FORGED    2J    z");
        assert!(!c.chars().any(char::is_control));
        let long = "\u{e9}".repeat(70_000);
        let c = clean_peer_text(&long);
        assert_eq!(c.chars().count(), MAX_PEER_TEXT_CHARS + 3);
        assert!(c.ends_with("..."));
        assert_eq!(
            clean_peer_text(&"x".repeat(MAX_PEER_TEXT_CHARS)).len(),
            MAX_PEER_TEXT_CHARS
        );
        assert_eq!(
            clean_peer_text("Minecraft (Pumpkin server)"),
            "Minecraft (Pumpkin server)"
        );
    }

    #[test]
    fn the_umask_warning_is_for_a_host_with_a_token_where_the_mode_cannot_be_set() {
        assert_eq!(
            endpoint_mode_warning(true, Role::Host, true),
            Some(ENDPOINT_MODE_WARNING)
        );
        assert_eq!(endpoint_mode_warning(false, Role::Host, true), None);
        assert_eq!(endpoint_mode_warning(true, Role::Guest, true), None);
        assert_eq!(endpoint_mode_warning(true, Role::Host, false), None);
    }

    #[test]
    fn entity_ids_wrap_round_but_skip_the_ones_still_held() {
        let mut b = Bridge::new(
            Config {
                guest_entity: u32::MAX,
                ..Config::default()
            },
            PathBuf::from("endpoint.json"),
        );
        let hold = |b: &mut Bridge, key: &str| {
            let entity = b.allocate_entity();
            b.tracked.insert(
                key.into(),
                Tracked {
                    entity,
                    name: key.into(),
                    spawned: true,
                    last: None,
                },
            );
            entity
        };
        assert_eq!(hold(&mut b, "a"), u32::MAX);
        assert_eq!(hold(&mut b, "b"), 0);
        assert_eq!(hold(&mut b, "c"), 1);
        // the counter comes round to ids that "a" and "b" still hold
        b.next_entity = u32::MAX;
        assert_eq!(hold(&mut b, "d"), 2);
        b.tracked.remove("b");
        b.next_entity = u32::MAX;
        assert_eq!(hold(&mut b, "e"), 0, "a freed id is given again");
    }

    #[test]
    fn a_broken_config_never_starts_a_link() {
        let mut b = Bridge::broken_config(
            "\"collision_radius\" must be a whole number from 1 to 64".into(),
            PathBuf::from("endpoint.json"),
        );
        for role in [Role::Host, Role::Guest] {
            let e = b.start(role).unwrap_err();
            assert!(e.contains("collision_radius"), "{e}");
            assert!(!b.is_running());
        }
        let status = b.status().join("\n");
        assert!(status.starts_with("config: "), "{status}");
        assert!(status.contains("does not start"), "{status}");
    }

    #[test]
    fn stopped_status_says_so_and_never_panics() {
        let b = Bridge::new(Config::default(), PathBuf::from("endpoint.json"));
        assert!(b.status()[0].starts_with("link: stopped"));
        assert!(!b.is_running());
    }
}
