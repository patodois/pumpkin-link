//! The bridge against a real `rewrite-bridge` peer over loopback, in one thread: the bridge is ticked
//! with fake players and a fake world, the peer is polled in between. Covers the handshake, spawn and
//! pose of the players, the collision snapshot, guest entities reaching the sink, the token and a wrong
//! token (CONTRACT sections 6 and 6.2), and the guest role.

use pumpkin_link::bridge::{Bridge, GuestSink, PlayerSample, Tick, Worlds};
use pumpkin_link::collision::{BlockSource, Cell};
use pumpkin_link::config::{Config, Role};
use rewrite_bridge::link::{self, Event, Link, LinkConfig, Pose, Spawn};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

struct Floor;

impl BlockSource for Floor {
    fn cell(&mut self, _x: i32, y: i32, _z: i32) -> Option<Rc<Cell>> {
        Some(if y < 64 { Cell::cube() } else { Cell::empty() })
    }
}

struct OneWorld(Floor);

impl Worlds for OneWorld {
    fn blocks(&mut self, world: &str) -> Option<&mut dyn BlockSource> {
        (world == "overworld").then_some(&mut self.0 as &mut dyn BlockSource)
    }
}

#[derive(Default)]
struct Sink {
    log: Vec<String>,
    live: HashMap<u32, ([f64; 3], f32, f32)>,
}

impl GuestSink for Sink {
    fn spawn(&mut self, entity: u32, s: &Spawn) {
        self.log.push(format!("spawn {entity} {}", s.asset));
        self.live.insert(entity, ([0.0; 3], 0.0, 0.0));
    }
    fn pose(&mut self, entity: u32, pos: [f64; 3], yaw: f32, pitch: f32, _t: bool) {
        self.live.insert(entity, (pos, yaw, pitch));
    }
    fn despawn(&mut self, entity: u32) {
        self.log.push(format!("despawn {entity}"));
        self.live.remove(&entity);
    }
    fn clear(&mut self) {
        self.log.push("clear".into());
        self.live.clear();
    }
}

fn player(key: &str, x: f64) -> PlayerSample {
    PlayerSample {
        key: key.into(),
        name: key.into(),
        world: "overworld".into(),
        pos: [x, 64.0, 0.5],
        yaw: -90.0,
        pitch: 0.0,
    }
}

fn temp_path(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pumpkin-link-e2e-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d.join("endpoint.json")
}

fn cleanup(p: &PathBuf) {
    let _ = std::fs::remove_file(p);
    if let Some(d) = p.parent() {
        let _ = std::fs::remove_dir(d);
    }
}

fn cfg() -> Config {
    Config {
        port: 0,
        collision_radius: 2,
        collision_interval_ms: 0,
        ..Config::default()
    }
}

/// Ticks the bridge and polls the peer until `done` or two seconds pass.
fn pump(
    b: &mut Bridge,
    peer: &mut Link,
    players: &[PlayerSample],
    sink: &mut Sink,
    events: &mut Vec<Event>,
    mut done: impl FnMut(&Bridge, &[Event]) -> bool,
) -> bool {
    let end = Instant::now() + Duration::from_secs(3);
    let mut worlds = OneWorld(Floor);
    while Instant::now() < end {
        b.tick(Tick {
            now: Instant::now(),
            players,
            worlds: &mut worlds,
            guests: sink,
        });
        events.extend(peer.poll());
        if done(b, events) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

fn guest_cfg(port: u16) -> LinkConfig {
    let mut lc = LinkConfig::new("test guest");
    lc.control_port = port;
    lc.reconnect = Duration::from_millis(50);
    lc
}

#[test]
fn host_without_token_spawns_players_sends_poses_and_collision() {
    let ep = temp_path("plain");
    let mut b = Bridge::new(cfg(), ep.clone());
    b.start(Role::Host).unwrap();
    let port = b.control_port().unwrap();
    let mut peer = Link::guest(guest_cfg(port)).unwrap();
    let (mut sink, mut ev) = (Sink::default(), Vec::new());
    let players = [player("alice", 0.5), player("bob", 3.5)];
    let ok = pump(&mut b, &mut peer, &players, &mut sink, &mut ev, |_, e| {
        let has = |f: fn(&Event) -> bool| e.iter().any(f);
        has(|x| matches!(x, Event::Collision(_)))
            && e.iter().filter(|x| matches!(x, Event::Spawn(_))).count() == 2
            && has(|x| matches!(x, Event::Poses { .. }))
    });
    assert!(ok, "events: {ev:?}");
    assert!(b.is_connected());
    // no token file without token_auth
    assert!(!ep.exists());
    let spawns: Vec<&Spawn> = ev
        .iter()
        .filter_map(|e| {
            if let Event::Spawn(s) = e {
                Some(s)
            } else {
                None
            }
        })
        .collect();
    let mut names: Vec<_> = spawns.iter().map(|s| s.name.clone().unwrap()).collect();
    names.sort();
    assert_eq!(names, ["alice", "bob"]);
    assert!(
        spawns
            .iter()
            .all(|s| s.provider == "Minecraft" && s.asset == "minecraft:player")
    );
    let ids: Vec<u32> = spawns.iter().map(|s| s.entity).collect();
    assert!(ids.contains(&1) && ids.contains(&2));
    // poses: yaw -90 faces +X
    let pose: Pose = ev
        .iter()
        .find_map(|e| {
            if let Event::Poses { poses, .. } = e {
                poses.first().copied()
            } else {
                None
            }
        })
        .unwrap();
    let f = pumpkin_link::mapping::rotate_z(pose.rotation);
    assert!((f[0] - 1.0).abs() < 1e-5, "{f:?}");
    // collision: radius 2 around the player, a floor below y = 64
    let c = ev
        .iter()
        .find_map(|e| {
            if let Event::Collision(c) = e {
                Some(c.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(c.radius, 2.0);
    assert!(!c.triangles.is_empty());
    assert!(
        c.triangles
            .iter()
            .flatten()
            .all(|v| v[1] <= 64.0 + 1e-6 && v[1] >= 62.0 - 1e-6)
    );
    // a player leaving is despawned
    let players = [player("alice", 0.5)];
    ev.clear();
    assert!(pump(
        &mut b,
        &mut peer,
        &players,
        &mut sink,
        &mut ev,
        |_, e| e.iter().any(|x| matches!(x, Event::Despawn { .. }))
    ));
    let status = b.status().join("\n");
    assert!(status.contains("host on 127.0.0.1"), "{status}");
    assert!(
        status.contains("Minecraft (Pumpkin server)") || status.contains("test guest"),
        "{status}"
    );
    b.stop(&mut sink);
    assert!(!b.is_running());
    cleanup(&ep);
}

#[test]
fn guest_entities_reach_the_sink_and_go_away_with_the_session() {
    let ep = temp_path("sink");
    let mut b = Bridge::new(cfg(), ep.clone());
    b.start(Role::Host).unwrap();
    let mut peer = Link::guest(guest_cfg(b.control_port().unwrap())).unwrap();
    let (mut sink, mut ev) = (Sink::default(), Vec::new());
    // Both ends of the handshake: the guest can only send once it has seen the host's welcome.
    assert!(pump(&mut b, &mut peer, &[], &mut sink, &mut ev, |b, e| {
        b.is_connected() && e.iter().any(|x| matches!(x, Event::Connected(_)))
    }));
    peer.spawn(&Spawn {
        entity: 7,
        provider: "Other".into(),
        asset: "x/y".into(),
        name: None,
    })
    .unwrap();
    // a pose facing +X at (10, 65, -3)
    let mut p = Pose::new(7, link::now_us());
    p.translation = [10.0, 65.0, -3.0];
    p.rotation = pumpkin_link::mapping::Mapping::default().rotation(-90.0, 0.0);
    std::thread::sleep(Duration::from_millis(20));
    peer.send_poses(&[p]).unwrap();
    assert!(pump(&mut b, &mut peer, &[], &mut sink, &mut ev, |_, _| {
        true
    }));
    // let the pose arrive
    for _ in 0..40 {
        let mut w = OneWorld(Floor);
        b.tick(Tick {
            now: Instant::now(),
            players: &[],
            worlds: &mut w,
            guests: &mut sink,
        });
        peer.poll();
        std::thread::sleep(Duration::from_millis(5));
        if sink.live.get(&7).is_some_and(|l| l.0 != [0.0; 3]) {
            break;
        }
    }
    let (pos, yaw, pitch) = sink.live[&7];
    assert_eq!(pos, [10.0, 65.0, -3.0]);
    assert!(
        (yaw + 90.0).abs() < 1e-3 && pitch.abs() < 1e-3,
        "{yaw} {pitch}"
    );
    assert_eq!(sink.log[0], "spawn 7 x/y");
    // the peer says bye: everything is cleared
    peer.disconnect("done");
    for _ in 0..200 {
        let mut w = OneWorld(Floor);
        b.tick(Tick {
            now: Instant::now(),
            players: &[],
            worlds: &mut w,
            guests: &mut sink,
        });
        peer.poll();
        if sink.live.is_empty() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(sink.live.is_empty());
    assert!(sink.log.iter().any(|l| l == "clear"));
    b.stop(&mut sink);
    cleanup(&ep);
}

#[test]
fn token_auth_writes_the_endpoint_after_binding_and_a_guest_with_the_file_connects() {
    let ep = temp_path("token");
    let mut c = cfg();
    c.token_auth = true;
    let mut b = Bridge::new(c, ep.clone());
    b.start(Role::Host).unwrap();
    // the file names the port that was really bound
    let e = link::Endpoint::parse(&std::fs::read(&ep).unwrap()).unwrap();
    assert_eq!(e.control_port, b.control_port().unwrap());
    assert_eq!(e.token.len(), 64);
    let mut lc = guest_cfg(1);
    lc.endpoint_file = Some(ep.clone());
    let mut peer = Link::guest(lc).unwrap();
    let (mut sink, mut ev) = (Sink::default(), Vec::new());
    assert!(pump(
        &mut b,
        &mut peer,
        &[player("a", 0.0)],
        &mut sink,
        &mut ev,
        |b, e| { b.is_connected() && e.iter().any(|x| matches!(x, Event::Spawn(_))) }
    ));
    let status = b.status().join("\n");
    assert!(status.contains("token auth on"), "{status}");
    assert!(
        !status.contains(&e.token),
        "the token must never be in the status"
    );
    b.stop(&mut sink);
    assert!(!ep.exists(), "the host removes the file when it stops");
    cleanup(&ep);
}

#[test]
fn a_guest_without_the_token_or_with_a_wrong_one_is_refused_as_unauthorized() {
    let ep = temp_path("wrong");
    let mut c = cfg();
    c.token_auth = true;
    let mut b = Bridge::new(c, ep.clone());
    b.start(Role::Host).unwrap();
    let port = b.control_port().unwrap();
    let real = link::Endpoint::parse(&std::fs::read(&ep).unwrap()).unwrap();
    let mut wrong = real.token.clone();
    wrong.replace_range(63.., if wrong.ends_with('0') { "1" } else { "0" });
    for token in [None, Some(wrong), Some(String::new())] {
        let mut lc = guest_cfg(port);
        lc.token = token.filter(|t| !t.is_empty());
        let mut peer = Link::guest(lc).unwrap();
        let (mut sink, mut ev) = (Sink::default(), Vec::new());
        let before = b.counters.refused_unauthorized;
        let ok = pump(&mut b, &mut peer, &[], &mut sink, &mut ev, |b, e| {
            b.counters.refused_unauthorized > before
                && e.iter().any(|x| matches!(x, Event::HandshakeFailed { reason } if reason.contains(link::UNAUTHORIZED)))
        });
        assert!(ok, "events: {ev:?}");
        assert!(!b.is_connected());
        drop(peer);
    }
    let status = b.status().join("\n");
    assert!(status.contains("refused as unauthorized"), "{status}");
    assert!(!status.contains(&real.token));
    b.stop(&mut Sink::default());
    cleanup(&ep);
}

#[test]
fn guest_role_publishes_players_to_a_real_host_with_and_without_token() {
    for with_token in [false, true] {
        let ep = temp_path(if with_token { "gtok" } else { "gplain" });
        let mut lc = LinkConfig::new("test host");
        lc.control_port = 0;
        let mut host = if with_token {
            lc.endpoint_file = Some(ep.clone());
            Link::host(lc).unwrap()
        } else {
            Link::host(lc).unwrap()
        };
        let port = host.control_addr().unwrap().port();
        let mut c = cfg();
        c.port = port;
        c.token_auth = with_token;
        let mut b = Bridge::new(c, ep.clone());
        b.start(Role::Guest).unwrap();
        let (mut sink, mut ev) = (Sink::default(), Vec::new());
        let players = [player("alice", 1.0)];
        let mut worlds = OneWorld(Floor);
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            b.tick(Tick {
                now: Instant::now(),
                players: &players,
                worlds: &mut worlds,
                guests: &mut sink,
            });
            ev.extend(host.poll());
            if ev.iter().any(|e| matches!(e, Event::Poses { .. })) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            ev.iter()
                .any(|e| matches!(e, Event::Spawn(s) if s.name.as_deref() == Some("alice"))),
            "{ev:?}"
        );
        assert!(ev.iter().any(|e| matches!(e, Event::Poses { .. })));
        // a guest never sends collision
        assert!(!ev.iter().any(|e| matches!(e, Event::Collision(_))));
        host.send_collision([0.0; 3], 1.0, &[]).unwrap();
        for _ in 0..100 {
            b.tick(Tick {
                now: Instant::now(),
                players: &players,
                worlds: &mut worlds,
                guests: &mut sink,
            });
            host.poll();
            std::thread::sleep(Duration::from_millis(5));
        }
        let status = b.status().join("\n");
        assert!(status.contains("guest on"), "{status}");
        assert!(status.contains("collision received"), "{status}");
        b.stop(&mut sink);
        drop(host);
        cleanup(&ep);
    }
}

#[test]
fn a_second_guest_gets_busy_and_the_first_session_survives() {
    let ep = temp_path("busy");
    let mut b = Bridge::new(cfg(), ep.clone());
    b.start(Role::Host).unwrap();
    let port = b.control_port().unwrap();
    let mut first = Link::guest(guest_cfg(port)).unwrap();
    let (mut sink, mut ev) = (Sink::default(), Vec::new());
    assert!(pump(&mut b, &mut first, &[], &mut sink, &mut ev, |b, _| b
        .is_connected()));
    let mut second = Link::guest(guest_cfg(port)).unwrap();
    let mut seen = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end
        && !seen
            .iter()
            .any(|e| matches!(e, Event::HandshakeFailed { .. }))
    {
        let mut w = OneWorld(Floor);
        b.tick(Tick {
            now: Instant::now(),
            players: &[],
            worlds: &mut w,
            guests: &mut sink,
        });
        first.poll();
        seen.extend(second.poll());
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        seen.iter()
            .any(|e| matches!(e, Event::HandshakeFailed { reason } if reason.contains("busy"))),
        "{seen:?}"
    );
    assert!(b.is_connected());
    b.stop(&mut sink);
    cleanup(&ep);
}

#[test]
fn start_twice_and_a_taken_port_are_errors_that_leave_no_endpoint_file() {
    let ep = temp_path("taken");
    let mut c = cfg();
    c.token_auth = true;
    let mut a = Bridge::new(c.clone(), ep.clone());
    a.start(Role::Host).unwrap();
    assert!(a.start(Role::Host).is_err());
    let taken = a.control_port().unwrap();
    // a second host on the same port cannot bind, so it writes no file (CONTRACT 6.2: squatting)
    let ep2 = temp_path("taken2");
    c.port = taken;
    let mut squatted = Bridge::new(c, ep2.clone());
    let err = squatted.start(Role::Host).unwrap_err();
    assert!(err.contains("cannot start the host"), "{err}");
    assert!(!ep2.exists());
    a.stop(&mut Sink::default());
    cleanup(&ep);
    cleanup(&ep2);
}
