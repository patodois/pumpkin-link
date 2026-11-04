//! What the bridge does with the link once per server tick: it polls it once. The tick-rate tests of
//! `session_protocol.rs` drive a `Link` directly, so they cannot tell how often the bridge polls; this one
//! counts the collision datagrams the bridge puts on the wire in each tick (CONTRACT section 9.5, "Pacing":
//! at most 32 per poll).

mod common;

use common::{RawGuest, tick_with};
use pumpkin_link::bridge::{Bridge, GuestSink, NoGuests, PlayerSample, Worlds};
use pumpkin_link::collision::{BlockSource, Cell};
use pumpkin_link::config::{Config, Role};
use rewrite_bridge::link::{COLLISION_PER_POLL, Datagram};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Cubes on every second block in every direction: nearly every face is exposed, so a small radius makes a
/// snapshot of many chunks.
struct Checker;

impl BlockSource for Checker {
    fn cell(&mut self, x: i32, y: i32, z: i32) -> Option<Rc<Cell>> {
        Some(if (x + y + z).rem_euclid(2) == 0 {
            Cell::cube()
        } else {
            Cell::empty()
        })
    }
}

struct World(Checker);

impl Worlds for World {
    fn blocks(&mut self, world: &str) -> Option<&mut dyn BlockSource> {
        (world == "overworld").then_some(&mut self.0 as &mut dyn BlockSource)
    }
}

#[test]
fn the_bridge_polls_the_link_once_per_tick_so_a_snapshot_goes_out_32_chunks_a_tick() {
    let mut b = Bridge::new(
        Config {
            port: 0,
            collision_radius: 5,
            collision_interval_ms: 0,
            ..Config::default()
        },
        PathBuf::from("endpoint.json"),
    );
    b.start(Role::Host).unwrap();
    let peer = RawGuest::connect(&mut b);
    let players = [PlayerSample {
        key: "a".into(),
        name: "a".into(),
        world: "overworld".into(),
        pos: [0.5, 64.0, 0.5],
        yaw: 0.0,
        pitch: 0.0,
    }];
    let mut sink: Box<dyn GuestSink> = Box::new(NoGuests);
    let mut world = World(Checker);
    // The scan reads one layer a tick (11 for this radius), then the chunks go out: a snapshot of 36 or more
    // chunks must take at least two ticks of sending.
    let mut per_tick = Vec::new();
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end && per_tick.iter().filter(|n| **n > 0).count() < 2 {
        tick_with(&mut b, &players, &mut world, sink.as_mut());
        let n = peer
            .received()
            .iter()
            .filter(|d| matches!(d, Datagram::Collision(_)))
            .count();
        per_tick.push(n);
    }
    println!("collision datagrams per tick: {per_tick:?}");
    assert!(
        per_tick.iter().filter(|n| **n > 0).count() >= 2,
        "the snapshot fit in one tick: {per_tick:?}"
    );
    assert_eq!(
        per_tick.iter().max(),
        Some(&COLLISION_PER_POLL),
        "{per_tick:?}"
    );
    b.stop(sink.as_mut());
}

/// The settings of the config that the peer can see reach the link: the UDP port, the heartbeat and the
/// timeout are in the host's `hello`.
#[test]
fn the_bridge_hands_the_udp_port_the_heartbeat_and_the_timeout_to_the_link() {
    let udp_port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut b = Bridge::new(
        Config {
            port: 0,
            udp_port,
            heartbeat_ms: 123,
            timeout_ms: 4567,
            ..Config::default()
        },
        PathBuf::from("endpoint.json"),
    );
    b.start(Role::Host).unwrap();
    let peer = RawGuest::connect(&mut b);
    assert_eq!(peer.host.udp_port, udp_port);
    assert_eq!(peer.host.heartbeat_ms, 123);
    assert_eq!(peer.host.timeout_ms, 4567);
    assert_eq!(peer.host.game, pumpkin_link::bridge::GAME);
    b.stop(&mut NoGuests);
}
