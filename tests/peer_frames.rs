//! What a peer's `frame_channel`, `Frame` and `Camera` do to the plugin: nothing but a count. A server has no
//! renderer, so it never opens the pixel file a peer names. The file is untrusted input (CONTRACT section
//! 9.6), and not touching it at all is the safe answer.
//!
//! The peer is a raw socket, the bridge is the real one. The test runs on every target; CI-style runs on
//! `wasm32-wasip2` (where a panic aborts the plugin) use a runner that gives it the network:
//!
//! ```text
//! CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -S inherit-network" \
//!   cargo test --target wasm32-wasip2 --no-default-features --test peer_frames
//! ```

use pumpkin_link::bridge::{Bridge, NoGuests, NoWorlds, Tick};
use pumpkin_link::config::{Config, Role};
use rewrite_bridge::link::{
    Camera, Control, Datagram, FrameReady, Header, encode_datagram, now_us,
};
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn tick(b: &mut Bridge) {
    b.tick(Tick {
        now: Instant::now(),
        players: &[],
        worlds: &mut NoWorlds,
        guests: &mut NoGuests,
    });
    std::thread::sleep(Duration::from_millis(3));
}

#[test]
fn a_pixel_file_a_peer_names_is_counted_and_never_opened() {
    // No token, so the bridge writes no endpoint file; the path is not used.
    let mut b = Bridge::new(
        Config {
            port: 0,
            ..Config::default()
        },
        PathBuf::from("endpoint.json"),
    );
    b.start(Role::Host).unwrap();
    let port = b.control_port().unwrap();

    let mut tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
    tcp.set_nonblocking(true).unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let hello = format!(
        "{{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"raw\",\"protocol\":1,\"min_protocol\":1,\
         \"udp_port\":{},\"heartbeat_ms\":250,\"timeout_ms\":2000}}\n",
        udp.local_addr().unwrap().port()
    );
    tcp.write_all(hello.as_bytes()).unwrap();

    // The host's hello carries its UDP port and the session.
    let mut text = String::new();
    let end = Instant::now() + Duration::from_secs(5);
    let reply = loop {
        tick(&mut b);
        let mut buf = [0u8; 1024];
        if let Ok(n) = tcp.read(&mut buf) {
            text.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        if let Some(line) = text.lines().next().filter(|_| text.contains('\n')) {
            match Control::parse(line).unwrap() {
                Control::Hello(h) => break h,
                other => panic!("{other:?}"),
            }
        }
        assert!(Instant::now() < end, "no hello from the host");
    };
    udp.connect(("127.0.0.1", reply.udp_port)).unwrap();
    let session = reply.session.unwrap();

    // Pixel files that do not exist, a directory, a relative path, sizes no file could have, and a path with
    // characters a file name cannot hold; then a Camera and Frames for them.
    let channels = [
        ("/does/not/exist.frames", 1280u64, 720u64, 3u64),
        ("/", 0, 0, 0),
        (
            "relative/path.frames",
            4_294_967_295,
            4_294_967_295,
            4_294_967_295,
        ),
        ("/tmp/\u{1}\u{7f}\u{e9}/x", 1, 1, 1),
    ];
    for (path, w, h, slots) in channels {
        let line = format!(
            "{{\"type\":\"frame_channel\",\"path\":{},\"width\":{w},\"height\":{h},\"slots\":{slots}}}\n",
            serde_json_string(path)
        );
        tcp.write_all(line.as_bytes()).unwrap();
    }
    let mut seq = 0;
    let mut send = |body: &Datagram| {
        let header = Header {
            version: 1,
            flags: 0,
            session,
            seq,
            time_us: now_us(),
        };
        seq += 1;
        let mut out = Vec::new();
        encode_datagram(&header, body, &mut out).unwrap();
        udp.send(&out).unwrap();
    };
    send(&Datagram::Camera(Camera::look_at(
        [0.0, 2.0, 10.0],
        [0.0; 3],
        1.0,
        0.1,
        500.0,
        [1280, 720],
    )));
    for frame in 1..=2 {
        send(&Datagram::Frame(FrameReady {
            frame,
            slot: 0,
            camera_seq: 0,
            width: 1280,
            height: 720,
        }));
    }
    // a frame that is stale, as the library counts them, is not counted by the bridge
    send(&Datagram::Frame(FrameReady {
        frame: 1,
        slot: 0,
        camera_seq: 0,
        width: 1280,
        height: 720,
    }));

    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && b.counters.frames_ignored < 6 {
        tick(&mut b);
    }
    // four channels and two frames; one camera; the session is intact; the datagram that decoded is not
    // an error either
    assert_eq!(b.counters.frames_ignored, 6, "{:?}", b.counters);
    assert_eq!(b.counters.cameras_ignored, 1);
    assert!(b.is_connected(), "{:?}", b.status());
    let status = b.status().join("\n");
    assert!(
        status.contains("1 cameras and 6 frame messages ignored"),
        "{status}"
    );
    let mut sink = NoGuests;
    b.stop(&mut sink);
    // what the peer could still read is not an error message about the files
    let mut rest = String::new();
    let _ = tcp.read_to_string(&mut rest);
    assert!(!rest.contains("frames"), "{rest}");
}

/// A JSON string literal for `s`, with the escapes a path can need.
fn serde_json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}
