//! What a peer's `frame_channel`, `Frame` and `Camera` do to the plugin: nothing but a count. A server has no
//! renderer, so it has no use for the pixel file a peer names, and the file is untrusted input (CONTRACT
//! section 9.6).
//!
//! The peer is a raw socket, the bridge is the real one. The first test runs on every target, and on
//! `wasm32-wasip2`, where a panic aborts the plugin, it needs a runner that gives it the network:
//!
//! ```text
//! CARGO_TARGET_WASM32_WASIP2_RUNNER="wasmtime run -S inherit-network" \
//!   cargo test --target wasm32-wasip2 --no-default-features --test peer_frames
//! ```
//!
//! The second, on Unix, shows that the path is not opened: it names a named pipe, which blocks whoever opens
//! it for reading until someone opens it for writing, and a thread of the test is waiting to be that writer.

mod common;

use common::{RawGuest, tick};
use pumpkin_link::bridge::{Bridge, NoGuests};
use pumpkin_link::config::{Config, Role};
use rewrite_bridge::link::{Camera, Datagram, FrameReady};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn host() -> Bridge {
    // No token, so the bridge writes no endpoint file; the path is not used.
    let mut b = Bridge::new(
        Config {
            port: 0,
            ..Config::default()
        },
        PathBuf::from("endpoint.json"),
    );
    b.start(Role::Host).unwrap();
    b
}

fn channel_line(path: &str, w: u64, h: u64, slots: u64) -> String {
    format!(
        "{{\"type\":\"frame_channel\",\"path\":{},\"width\":{w},\"height\":{h},\"slots\":{slots}}}\n",
        serde_json::to_string(path).unwrap()
    )
}

/// Ticks the bridge until `done`, for at most five seconds.
fn wait_for(b: &mut Bridge, done: impl Fn(&Bridge) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && !done(b) {
        tick(b);
    }
    assert!(done(b), "{:?}", b.counters);
}

fn frame(frame: u64) -> Datagram {
    Datagram::Frame(FrameReady {
        frame,
        slot: 0,
        camera_seq: 0,
        width: 1280,
        height: 720,
    })
}

#[test]
fn frame_channels_frames_and_cameras_are_counted_and_nothing_else_happens() {
    let mut b = host();
    let mut peer = RawGuest::connect(&mut b);

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
        peer.write_line(&channel_line(path, w, h, slots));
    }
    // A control line travels on TCP and a datagram on UDP, and nothing orders them. A `frame_channel` that
    // arrives after a `Frame` starts the frame numbers again, which makes a stale frame count: so the
    // datagrams wait until all four channels have been counted.
    wait_for(&mut b, |b| b.counters.frames_ignored >= 4);
    peer.datagram(&Datagram::Camera(Camera::look_at(
        [0.0, 2.0, 10.0],
        [0.0; 3],
        1.0,
        0.1,
        500.0,
        [1280, 720],
    )));
    peer.datagram(&frame(1));
    peer.datagram(&frame(2));
    // a frame that is stale, as the library counts them, is not counted by the bridge
    peer.datagram(&frame(1));

    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && b.counters.frames_ignored < 6 {
        tick(&mut b);
    }
    // four channels and two frames; one camera; the session is intact
    assert_eq!(b.counters.frames_ignored, 6, "{:?}", b.counters);
    assert_eq!(b.counters.cameras_ignored, 1);
    assert!(b.is_connected(), "{:?}", b.status());
    let status = b.status().join("\n");
    assert!(
        status.contains("1 cameras and 6 frame messages ignored"),
        "{status}"
    );
    b.stop(&mut NoGuests);
}

#[cfg(unix)]
#[test]
fn the_pixel_file_a_peer_names_is_never_opened() {
    use std::fs::OpenOptions;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let dir = std::env::temp_dir().join(format!("pumpkin-link-fifo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fifo = dir.join("pixels.frames");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    assert!(made.success());

    // `open` for writing returns only when somebody opens the pipe for reading
    let opened = Arc::new(AtomicBool::new(false));
    let writer = {
        let (opened, fifo) = (opened.clone(), fifo.clone());
        std::thread::spawn(move || {
            let _w = OpenOptions::new().write(true).open(&fifo);
            opened.store(true, Ordering::SeqCst);
        })
    };

    let mut b = host();
    let mut peer = RawGuest::connect(&mut b);
    peer.write_line(&channel_line(fifo.to_str().unwrap(), 4, 4, 2));
    wait_for(&mut b, |b| b.counters.frames_ignored >= 1);
    peer.datagram(&frame(1));
    let end = Instant::now() + Duration::from_millis(500);
    while Instant::now() < end {
        tick(&mut b);
    }
    assert_eq!(b.counters.frames_ignored, 2, "{:?}", b.counters);
    assert!(
        !opened.load(Ordering::SeqCst),
        "the plugin opened the pixel file the peer named"
    );
    // let the waiting writer go: opening the pipe for reading ourselves is what it waits for
    let _reader = std::fs::File::open(&fifo).unwrap();
    writer.join().unwrap();
    b.stop(&mut NoGuests);
    let _ = std::fs::remove_file(&fifo);
    let _ = std::fs::remove_dir(&dir);
}
