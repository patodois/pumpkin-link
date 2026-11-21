//! The plugin's bridge, driven like the server would drive it, as a `wasm32-wasip2` command: fake
//! players walk in circles over a flat floor, the config is read from the `data` folder, the guest's
//! entities are printed. It exists so the parts of the plugin that depend on the WebAssembly sandbox
//! (sockets, random bytes, files, clocks) can be run without a Pumpkin server, under
//! `tools/sandbox-host`. It contains no server code and no game data.
//!
//! cargo build --release --target wasm32-wasip2 --no-default-features --example sandbox_server
//! sandbox-host --data DIR target/wasm32-wasip2/release/examples/sandbox_server.wasm [--seconds 30]
//!     [--players 2] [--floor 64]
//!
//! It ends after `--seconds`, or at once when the file `data/stop` exists.

use pumpkin_link::bridge::{Bridge, GuestSink, PlayerSample, Tick, Worlds};
use pumpkin_link::collision::{BlockSource, Cell};
use pumpkin_link::config::{Config, Loaded};
use rewrite_bridge::link::Spawn;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

struct Floor(i32);

impl BlockSource for Floor {
    fn cell(&mut self, _x: i32, y: i32, _z: i32) -> Option<Rc<Cell>> {
        Some(if y < self.0 {
            Cell::cube()
        } else {
            Cell::empty()
        })
    }
}

struct One(Floor);

impl Worlds for One {
    fn blocks(&mut self, world: &str) -> Option<&mut dyn BlockSource> {
        (world == "overworld").then_some(&mut self.0 as &mut dyn BlockSource)
    }
}

#[derive(Default)]
struct Printer {
    posed: HashSet<u32>,
}

impl GuestSink for Printer {
    fn spawn(&mut self, entity: u32, s: &Spawn) {
        println!(
            "GUEST spawn entity={entity} provider={} asset={}",
            s.provider, s.asset
        );
    }
    fn pose(&mut self, entity: u32, pos: [f64; 3], yaw: f32, pitch: f32, teleport: bool) {
        if self.posed.insert(entity) {
            println!(
                "GUEST pose entity={entity} pos=[{:.2},{:.2},{:.2}] yaw={yaw:.1} pitch={pitch:.1} teleport={teleport}",
                pos[0], pos[1], pos[2]
            );
        }
    }
    fn despawn(&mut self, entity: u32) {
        println!("GUEST despawn entity={entity}");
        self.posed.remove(&entity);
    }
    fn clear(&mut self) {
        println!("GUEST clear");
        self.posed.clear();
    }
}

fn arg<T: std::str::FromStr>(name: &str) -> Option<T> {
    let args: Vec<String> = std::env::args().collect();
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1)?.parse().ok()
}

fn main() {
    let seconds: u64 = arg("--seconds").unwrap_or(30);
    let count: usize = arg("--players").unwrap_or(2);
    let floor: i32 = arg("--floor").unwrap_or(64);
    let dir = "data";
    let cfg_path = format!("{dir}/pumpkin-link.json");
    // the plugin's rule: only a missing file is written, a broken one is left as it is
    let cfg = match Loaded::from_read(std::fs::read_to_string(&cfg_path)) {
        Loaded::Valid(c) => c,
        Loaded::Missing => {
            let _ = std::fs::write(&cfg_path, Config::default_text());
            Config::default()
        }
        Loaded::Broken(e) => {
            eprintln!("config: {e}");
            std::process::exit(2);
        }
    };
    let name = if cfg.endpoint_file.is_empty() {
        "endpoint.json"
    } else {
        cfg.endpoint_file.as_str()
    };
    let endpoint = PathBuf::from(format!("{dir}/{name}"));
    let role = cfg.role;
    let auto = cfg.auto_start;
    let mut bridge = Bridge::new(cfg, endpoint);
    if auto {
        match bridge.start(role) {
            Ok(()) => println!(
                "STARTED {role:?} port={}",
                bridge.control_port().unwrap_or(0)
            ),
            Err(e) => {
                eprintln!("start: {e}");
                std::process::exit(1);
            }
        }
    }
    let mut worlds = One(Floor(floor));
    let mut sink = Printer::default();
    let begin = Instant::now();
    let mut last_status = Instant::now();
    let mut n: u64 = 0;
    while begin.elapsed() < Duration::from_secs(seconds) {
        let t = n as f64 * 0.05;
        let players: Vec<PlayerSample> = (0..count)
            .map(|i| {
                let a = t * 0.5 + i as f64;
                PlayerSample {
                    key: format!("player-{i}"),
                    name: format!("Player{i}"),
                    world: "overworld".into(),
                    pos: [8.0 * a.cos(), f64::from(floor), 8.0 * a.sin()],
                    yaw: ((a.to_degrees() + 90.0) % 360.0 - 180.0) as f32,
                    pitch: 10.0,
                }
            })
            .collect();
        bridge.tick(Tick {
            now: Instant::now(),
            players: &players,
            worlds: &mut worlds,
            guests: &mut sink,
        });
        if last_status.elapsed() >= Duration::from_secs(5) {
            last_status = Instant::now();
            for l in bridge.status() {
                println!("STATUS {l}");
            }
        }
        if n.is_multiple_of(20) && std::path::Path::new("data/stop").exists() {
            break;
        }
        n += 1;
        std::thread::sleep(Duration::from_millis(50));
    }
    for l in bridge.status() {
        println!("FINAL {l}");
    }
    bridge.stop(&mut sink);
    println!("STOPPED");
}
