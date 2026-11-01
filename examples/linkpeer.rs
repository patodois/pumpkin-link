//! The other end for the live tests: a `rewrite-bridge` peer that checks what the plugin's link sends.
//! Prints one result line, `PEER OK ...` or `PEER FAIL: ...`, and exits 0 or 1.
//!
//! cargo run --release --example linkpeer -- guest --port 47800 --expect ok [--endpoint FILE]
//!     [--token T] [--players 2] [--seconds 20]
//! cargo run --release --example linkpeer -- guest --port 47800 --expect unauthorized [--token T]
//! cargo run --release --example linkpeer -- host --port 47800 [--players 2] [--endpoint FILE]
//!
//! `guest --expect ok`: connects to the plugin as host, and must see a spawn and poses for every
//! player (provider and asset as configured) and a collision snapshot; it also spawns an entity of its
//! own and poses it so the server's side can show it. `guest --expect unauthorized`: the handshake must
//! end in `unauthorized` and no session may start. `host`: listens for the plugin as guest and must
//! see the same spawns and poses.

use rewrite_bridge::link::{Event, Link, LinkConfig, Pose, Spawn, UNAUTHORIZED, now_us};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::exit;
use std::time::{Duration, Instant};

fn arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse().ok()
}

fn fail(why: &str) -> ! {
    println!("PEER FAIL: {why}");
    exit(1)
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let port: u16 = arg("--port").unwrap_or(47800);
    let players: usize = arg("--players").unwrap_or(2);
    let seconds: u64 = arg("--seconds").unwrap_or(20);
    let expect: String = arg("--expect").unwrap_or_else(|| "ok".into());
    let endpoint: Option<PathBuf> = arg::<String>("--endpoint").map(PathBuf::from);
    let token: Option<String> = arg("--token");

    let mut cfg = LinkConfig::new("linkpeer");
    cfg.control_port = port;
    cfg.reconnect = Duration::from_millis(100);
    cfg.token = token;
    cfg.endpoint_file = endpoint;
    let mut link = match mode.as_str() {
        "guest" => Link::guest(cfg),
        "host" => Link::host(cfg),
        _ => fail("first argument must be guest or host"),
    }
    .unwrap_or_else(|e| fail(&format!("cannot start: {e}")));

    let end = Instant::now() + Duration::from_secs(seconds);
    let mut spawned: HashSet<u32> = HashSet::new();
    let mut posed: HashSet<u32> = HashSet::new();
    let mut collision = 0usize;
    let mut connected = false;
    let mut sent_own = false;
    let mut own_poses = 0u32;
    let mut refused: Option<(Instant, String)> = None;
    while Instant::now() < end {
        for e in link.poll() {
            match e {
                Event::Connected(p) => {
                    if expect == "unauthorized" {
                        fail("a session started although unauthorized was expected");
                    }
                    println!("peer connected to {:?} session {}", p.game, p.session);
                    connected = true;
                }
                Event::HandshakeFailed { reason } => {
                    if expect == "unauthorized" && reason.contains(UNAUTHORIZED) {
                        refused.get_or_insert((Instant::now(), reason));
                    } else if expect == "unauthorized" {
                        fail(&format!("refused for another reason: {reason}"));
                    } else {
                        println!("handshake failed ({reason}), retrying");
                    }
                }
                Event::Spawn(s) => {
                    if s.provider != "Minecraft" || s.asset != "minecraft:player" {
                        fail(&format!("unexpected spawn {s:?}"));
                    }
                    spawned.insert(s.entity);
                }
                Event::Poses { poses, .. } => {
                    for p in poses {
                        if !spawned.contains(&p.entity) {
                            fail("pose for an entity that was not spawned was delivered");
                        }
                        let n: f32 = p.rotation.iter().map(|v| v * v).sum();
                        if !p.translation.iter().all(|v| v.is_finite()) || (n - 1.0).abs() > 1e-3 {
                            fail(&format!("bad pose {p:?}"));
                        }
                        posed.insert(p.entity);
                    }
                }
                Event::Collision(c) => {
                    if !c.triangles.is_empty() {
                        collision += 1;
                    }
                }
                Event::Lost { reason } => println!("session lost: {reason}"),
                _ => {}
            }
        }
        // unauthorized: no session may follow it (the guest retries on its own, and each try must fail too)
        if let Some((at, reason)) = &refused
            && at.elapsed() > Duration::from_millis(700)
        {
            println!("PEER OK unauthorized: {reason}");
            exit(0);
        }
        if connected && !sent_own && mode == "guest" {
            // an entity of our own, so the server has a guest to show
            let _ = link.spawn(&Spawn {
                entity: 90,
                provider: "linkpeer".into(),
                asset: "test/cube".into(),
                name: Some("peer".into()),
            });
            sent_own = true;
        }
        if sent_own && link.is_connected() && own_poses < 400 {
            let mut p = Pose::new(90, now_us());
            p.translation = [2.0, 65.0, 3.0];
            let _ = link.send_poses(&[p]);
            own_poses += 1;
        }
        let done = spawned.len() >= players
            && posed.len() >= players
            && (mode == "host" || collision > 0)
            && (mode == "host" || own_poses >= 60);
        if expect != "unauthorized" && done {
            println!(
                "PEER OK {} players spawned and posed, {collision} collision snapshots, {} own poses sent",
                spawned.len(),
                own_poses
            );
            // give the server a moment to print what it received before the bye
            std::thread::sleep(Duration::from_millis(300));
            link.disconnect("done");
            let _ = link.poll();
            exit(0);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    if expect == "unauthorized" {
        fail("no unauthorized answer within the time limit");
    }
    fail(&format!(
        "timed out: connected={connected} spawned={} posed={} collision={collision}",
        spawned.len(),
        posed.len()
    ));
}
