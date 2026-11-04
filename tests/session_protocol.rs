//! The rules of CONTRACT sections 6 to 9 that the plugin relies on `rewrite_bridge::link::Link` for,
//! checked with hand-written peers on raw sockets (so a wrong answer cannot hide behind a library that
//! makes the same mistake). The link is driven the way the plugin drives it: `poll` and nothing else.

use rewrite_bridge::link::{
    CollisionChunk, Control, Datagram, Event, Header, Link, LinkConfig, MAX_ENTITIES,
    PROTOCOL_VERSION, Pose, Role, decode_datagram, encode_datagram, now_us,
};
use std::io::{Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::time::{Duration, Instant};

struct Rig {
    host: Link,
    tcp: TcpStream,
    udp: UdpSocket,
    buf: Vec<u8>,
    events: Vec<Event>,
    session: u32,
    seq: u32,
}

fn cfg() -> LinkConfig {
    let mut c = LinkConfig::new("test host");
    c.control_port = 0;
    c.timeout = Duration::from_millis(2000);
    c
}

fn hello_line(extra: &str) -> String {
    format!(
        "{{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"raw\",\"protocol\":1,\"min_protocol\":1,\
         \"udp_port\":{{UDP}},\"heartbeat_ms\":250,\"timeout_ms\":2000{extra}}}\n"
    )
}

impl Rig {
    fn connect(host: Link) -> Self {
        let port = host.control_addr().unwrap().port();
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        tcp.set_nonblocking(true).unwrap();
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_nonblocking(true).unwrap();
        Self {
            host,
            tcp,
            udp,
            buf: Vec::new(),
            events: Vec::new(),
            session: 0,
            seq: 0,
        }
    }

    fn send(&mut self, line: &str) {
        let port = self.udp.local_addr().unwrap().port();
        let line = line.replace("{UDP}", &port.to_string());
        self.tcp.write_all(line.as_bytes()).unwrap();
    }

    /// Polls the host for a while, collecting events and the peer's incoming lines.
    fn run(&mut self, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            self.events.extend(self.host.poll());
            let mut b = [0u8; 4096];
            while let Ok(n) = self.tcp.read(&mut b) {
                if n == 0 {
                    break;
                }
                self.buf.extend_from_slice(&b[..n]);
            }
            std::thread::sleep(Duration::from_millis(3));
        }
    }

    fn until(&mut self, mut f: impl FnMut(&Rig) -> bool) -> bool {
        let end = Instant::now() + Duration::from_secs(3);
        while Instant::now() < end {
            self.run(10);
            if f(self) {
                return true;
            }
        }
        false
    }

    fn lines(&self) -> Vec<String> {
        String::from_utf8_lossy(&self.buf)
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn handshake(host: Link) -> Self {
        let mut r = Self::connect(host);
        r.send(&hello_line(""));
        assert!(r.until(|r| r.lines().iter().any(|l| l.contains("\"role\":\"host\""))));
        let reply = r.lines()[0].clone();
        match Control::parse(&reply).unwrap() {
            Control::Hello(h) => {
                assert_eq!(h.protocol, 1);
                assert!(h.session.unwrap() != 0);
                assert!(h.token.is_none(), "a host never puts a token in its hello");
                r.session = h.session.unwrap();
                r.udp.connect(("127.0.0.1", h.udp_port)).unwrap();
            }
            other => panic!("{other:?}"),
        }
        assert!(r.until(|r| r.events.iter().any(|e| matches!(e, Event::Connected(_)))));
        r
    }

    fn datagram(&mut self, body: &Datagram) {
        self.seq += 1;
        let h = Header {
            version: 1,
            flags: 0,
            session: self.session,
            seq: self.seq,
            time_us: now_us(),
        };
        let mut out = Vec::new();
        encode_datagram(&h, body, &mut out).unwrap();
        self.udp.send(&out).unwrap();
    }

    fn bye_reason(&self) -> Option<String> {
        self.lines().iter().find_map(|l| match Control::parse(l) {
            Ok(Control::Bye { reason }) => Some(reason),
            _ => None,
        })
    }

    fn eof(&mut self) -> bool {
        // the host half-closes after its bye: a read of 0 bytes
        let mut b = [0u8; 16];
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            self.host.poll();
            match self.tcp.read(&mut b) {
                Ok(0) => return true,
                Ok(_) => {}
                Err(_) => std::thread::sleep(Duration::from_millis(3)),
            }
        }
        false
    }
}

fn pose(entity: u32, t: u64) -> Pose {
    let mut p = Pose::new(entity, t);
    p.translation = [1.0, 2.0, 3.0];
    p
}

#[test]
fn the_first_line_must_be_a_hello_or_a_bye() {
    let mut r = Rig::connect(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"spawn\",\"entity\":1,\"provider\":\"p\",\"asset\":\"a\"}\n");
    assert!(r.until(|r| r.bye_reason().is_some()));
    assert!(
        r.events
            .iter()
            .any(|e| matches!(e, Event::HandshakeFailed { .. }))
    );
    assert!(r.eof(), "the host closes after its bye (section 6.1)");
    // an unknown type is a protocol error as well
    let mut r = Rig::connect(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"zzz\"}\n");
    assert!(r.until(|r| r.bye_reason().is_some()));
}

#[test]
fn a_bye_first_ends_the_handshake_without_a_bye_back() {
    let mut r = Rig::connect(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"bye\",\"reason\":\"never mind\"}\n");
    assert!(r.until(|r| {
        r.events
            .iter()
            .any(|e| matches!(e, Event::HandshakeFailed { .. }))
    }));
    assert!(r.eof());
    assert_eq!(r.bye_reason(), None);
}

#[test]
fn handshake_errors_are_answered_with_a_bye_and_a_failed_handshake() {
    let cases = [
        ("udp_port 0", hello_line("").replace("{UDP}", "0"), true),
        (
            "heartbeat 5",
            hello_line("").replace("\"heartbeat_ms\":250", "\"heartbeat_ms\":5"),
            false,
        ),
        (
            "heartbeat 10001",
            hello_line("").replace("\"heartbeat_ms\":250", "\"heartbeat_ms\":10001"),
            false,
        ),
        (
            "role host",
            hello_line("").replace("\"role\":\"guest\"", "\"role\":\"host\""),
            false,
        ),
        (
            "versions",
            hello_line("")
                .replace("\"min_protocol\":1", "\"min_protocol\":2")
                .replace("\"protocol\":1", "\"protocol\":3"),
            false,
        ),
    ];
    for (name, line, _) in cases {
        let mut r = Rig::connect(Link::host(cfg()).unwrap());
        r.send(&line);
        assert!(r.until(|r| r.bye_reason().is_some()), "{name}");
        assert!(
            r.events
                .iter()
                .any(|e| matches!(e, Event::HandshakeFailed { .. })),
            "{name}"
        );
        assert!(r.eof(), "{name}");
    }
}

#[test]
fn a_token_is_checked_first_and_the_answer_is_only_the_word_unauthorized() {
    let mut c = cfg();
    c.token = Some("s3cret-token".into());
    // wrong token AND an invalid hello (udp_port 0): still only "unauthorized", nothing about the port
    for extra in [
        "",
        ",\"token\":\"nope\"",
        ",\"token\":null",
        ",\"token\":\"\"",
        ",\"token\":\"s3cret-token \"",
    ] {
        let mut r = Rig::connect(Link::host(c.clone()).unwrap());
        r.send(&hello_line(extra).replace("{UDP}", "0"));
        assert!(r.until(|r| r.bye_reason().is_some()), "{extra}");
        assert_eq!(r.bye_reason().as_deref(), Some("unauthorized"), "{extra}");
        let all = r.lines().join("\n");
        assert!(!all.contains("s3cret"), "the token is never echoed");
        assert!(
            r.events.iter().any(
                |e| matches!(e, Event::HandshakeFailed { reason } if reason == "unauthorized")
            )
        );
    }
    // the right token connects
    let mut r = Rig::connect(Link::host(c).unwrap());
    r.send(&hello_line(",\"token\":\"s3cret-token\""));
    assert!(r.until(|r| r.events.iter().any(|e| matches!(e, Event::Connected(_)))));
}

#[test]
fn a_host_without_a_token_ignores_the_field_but_not_its_type() {
    let mut r = Rig::connect(Link::host(cfg()).unwrap());
    r.send(&hello_line(",\"token\":\"anything at all\""));
    assert!(r.until(|r| r.events.iter().any(|e| matches!(e, Event::Connected(_)))));
    let mut r = Rig::connect(Link::host(cfg()).unwrap());
    r.send(&hello_line(",\"token\":5"));
    assert!(
        r.until(|r| r.bye_reason().is_some()),
        "a token that is not a string is a protocol error"
    );
}

#[test]
fn a_second_connection_gets_busy_and_the_first_is_untouched() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    let port = r.host.control_addr().unwrap().port();
    let mut second = TcpStream::connect(("127.0.0.1", port)).unwrap();
    second.set_nonblocking(true).unwrap();
    let mut got = String::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && !got.contains("busy") {
        r.run(10);
        let mut b = [0u8; 256];
        if let Ok(n) = second.read(&mut b) {
            got.push_str(&String::from_utf8_lossy(&b[..n]));
        }
    }
    assert!(got.contains("\"reason\":\"busy\""), "{got}");
    assert!(r.host.is_connected());
}

#[test]
fn a_second_hello_is_a_protocol_error_that_ends_the_session() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    r.send(&hello_line(""));
    assert!(r.until(|r| r.events.iter().any(|e| matches!(e, Event::Lost { .. }))));
    assert!(r.bye_reason().is_some());
    assert!(!r.host.is_connected());
}

#[test]
fn a_line_over_65536_bytes_ends_the_session() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    let long = format!("{{\"type\":\"x\",\"pad\":\"{}\"}}\n", "a".repeat(65536));
    r.send(&long);
    assert!(r.until(|r| r.events.iter().any(|e| matches!(e, Event::Lost { .. }))));
    assert!(r.bye_reason().is_some());
}

#[test]
fn a_garbled_line_and_a_missing_field_end_the_session() {
    for bad in [
        "not json\n",
        "{\"type\":\"spawn\",\"entity\":-1,\"provider\":\"p\",\"asset\":\"a\"}\n",
        "{\"type\":\"despawn\",\"entity\":7.0}\n",
    ] {
        let mut r = Rig::handshake(Link::host(cfg()).unwrap());
        r.send(bad);
        assert!(
            r.until(|r| r.events.iter().any(|e| matches!(e, Event::Lost { .. }))),
            "{bad}"
        );
    }
}

#[test]
fn a_received_bye_ends_the_session_and_gets_no_bye_back() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"bye\",\"reason\":\"bye now\"}\n");
    assert!(r.until(|r| {
        r.events
            .iter()
            .any(|e| matches!(e, Event::Lost { reason } if reason.contains("bye now")))
    }));
    assert_eq!(r.bye_reason(), None);
    // the host listens again
    let mut again = Rig::connect(Link::host(cfg()).unwrap());
    again.send(&hello_line(""));
    assert!(again.until(|r| r.events.iter().any(|e| matches!(e, Event::Connected(_)))));
}

#[test]
fn unknown_types_and_fields_are_ignored() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"future\",\"x\":[1,2,{\"y\":null}]}\n");
    r.send("{\"type\":\"spawn\",\"entity\":3,\"provider\":\"p\",\"asset\":\"a\",\"extra\":true}\n");
    assert!(r.until(|r| {
        r.events
            .iter()
            .any(|e| matches!(e, Event::Spawn(s) if s.entity == 3))
    }));
    assert!(r.host.is_connected());
}

#[test]
fn at_most_1024_entities_a_replace_always_works_and_a_despawn_of_nothing_is_ignored() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    for i in 0..=MAX_ENTITIES as u32 {
        r.send(&format!(
            "{{\"type\":\"spawn\",\"entity\":{i},\"provider\":\"p\",\"asset\":\"a\"}}\n"
        ));
    }
    r.send("{\"type\":\"spawn\",\"entity\":5,\"provider\":\"q\",\"asset\":\"replaced\"}\n");
    r.send("{\"type\":\"despawn\",\"entity\":999999}\n");
    assert!(r.until(|r| {
        r.events
            .iter()
            .any(|e| matches!(e, Event::Spawn(s) if s.asset == "replaced"))
    }));
    let spawns = r
        .events
        .iter()
        .filter(|e| matches!(e, Event::Spawn(_)))
        .count();
    assert_eq!(
        spawns,
        MAX_ENTITIES + 1,
        "1024 new ones plus the replacement; the 1025th is ignored"
    );
    assert_eq!(r.host.stats().spawn_ignored, 1);
    assert!(!r.events.iter().any(|e| matches!(e, Event::Despawn { .. })));
    assert!(r.host.is_connected());
    // a despawn frees the place for a new id
    r.send("{\"type\":\"despawn\",\"entity\":0}\n");
    r.send("{\"type\":\"spawn\",\"entity\":5000,\"provider\":\"p\",\"asset\":\"a\"}\n");
    assert!(r.until(|r| {
        r.events
            .iter()
            .any(|e| matches!(e, Event::Spawn(s) if s.entity == 5000))
    }));
}

#[test]
fn poses_only_for_spawned_entities_and_never_older_than_the_last_used() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"spawn\",\"entity\":7,\"provider\":\"p\",\"asset\":\"a\"}\n");
    r.run(100);
    r.datagram(&Datagram::Pose(vec![pose(7, 1000), pose(8, 1000)]));
    r.datagram(&Datagram::Pose(vec![pose(7, 500)])); // older: dropped
    r.datagram(&Datagram::Pose(vec![pose(7, 1000)])); // not older: used
    r.run(150);
    let delivered: Vec<u64> = r
        .events
        .iter()
        .filter_map(|e| {
            if let Event::Poses { poses, .. } = e {
                Some(poses.iter().map(|p| p.time_us).collect::<Vec<_>>())
            } else {
                None
            }
        })
        .flatten()
        .collect();
    assert_eq!(delivered, vec![1000, 1000]);
    let s = r.host.stats();
    assert_eq!(s.unknown_entity, 1);
    assert_eq!(s.stale, 1);
}

#[test]
fn datagrams_of_another_session_or_address_are_dropped_and_malformed_ones_counted() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"spawn\",\"entity\":7,\"provider\":\"p\",\"asset\":\"a\"}\n");
    r.run(100);
    // another session
    let good = r.session;
    r.session = good.wrapping_add(1);
    r.datagram(&Datagram::Pose(vec![pose(7, 1)]));
    r.session = good;
    // another source address (a second socket)
    let other = UdpSocket::bind("127.0.0.1:0").unwrap();
    let h = Header {
        version: 1,
        flags: 0,
        session: good,
        seq: 99,
        time_us: 1,
    };
    let mut out = Vec::new();
    encode_datagram(&h, &Datagram::Pose(vec![pose(7, 5)]), &mut out).unwrap();
    other.send_to(&out, r.host.udp_addr().unwrap()).unwrap();
    // garbage
    r.udp.send(&[1, 2, 3]).unwrap();
    r.run(150);
    assert!(!r.events.iter().any(|e| matches!(e, Event::Poses { .. })));
    let s = r.host.stats();
    assert!(s.foreign >= 2, "{s:?}");
    assert!(s.malformed >= 1, "{s:?}");
}

fn chunk(query: u32, index: u16, count: u16, n: usize, tag: f32) -> CollisionChunk {
    CollisionChunk {
        query,
        chunk_index: index,
        chunk_count: count,
        center: [tag, 0.0, 0.0],
        radius: 4.0,
        triangles: vec![[[tag, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]]; n],
    }
}

#[test]
fn collision_assembly_follows_section_9_5() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    // chunks of query 1 arrive out of order, one twice with different content (the first copy wins)
    r.datagram(&Datagram::Collision(chunk(1, 1, 2, 1, 2.0)));
    r.datagram(&Datagram::Collision(chunk(1, 1, 2, 1, 9.0)));
    r.datagram(&Datagram::Collision(chunk(1, 0, 2, 1, 1.0)));
    r.run(150);
    let snaps: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| {
            if let Event::Collision(c) = e {
                Some(c.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].triangles.len(), 2);
    // in chunk index order: index 0 (tag 1) first, then index 1 (tag 2)
    assert_eq!(snaps[0].triangles[0][0][0], 1.0);
    assert_eq!(snaps[0].triangles[1][0][0], 2.0);
    // the first chunk to arrive decides the centre: that was index 1 with tag 2
    assert_eq!(snaps[0].center[0], 2.0);
    // a stale query (not newer than the completed one) is dropped
    r.datagram(&Datagram::Collision(chunk(1, 0, 1, 1, 5.0)));
    // a newer, incomplete query is replaced by an even newer one
    r.datagram(&Datagram::Collision(chunk(2, 0, 2, 1, 1.0)));
    r.datagram(&Datagram::Collision(chunk(3, 0, 1, 0, 7.0))); // empty snapshot: one chunk, no triangles
    r.run(150);
    let snaps: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| {
            if let Event::Collision(c) = e {
                Some(c.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(snaps.len(), 2);
    assert_eq!(snaps[1].query, 3);
    assert!(snaps[1].triangles.is_empty());
    // a late chunk of the discarded query 2 is stale
    r.datagram(&Datagram::Collision(chunk(2, 1, 2, 1, 1.0)));
    r.run(100);
    assert_eq!(
        r.events
            .iter()
            .filter(|e| matches!(e, Event::Collision(_)))
            .count(),
        2
    );
}

#[test]
fn a_collision_chunk_with_another_chunk_count_is_malformed_and_the_assembly_goes_on() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    r.datagram(&Datagram::Collision(chunk(1, 0, 3, 1, 1.0)));
    r.datagram(&Datagram::Collision(chunk(1, 1, 2, 1, 1.0))); // count 2, not 3: dropped
    r.datagram(&Datagram::Collision(chunk(1, 1, 3, 1, 1.0)));
    r.datagram(&Datagram::Collision(chunk(1, 2, 3, 1, 1.0)));
    r.run(150);
    let snaps: Vec<_> = r
        .events
        .iter()
        .filter_map(|e| {
            if let Event::Collision(c) = e {
                Some(c.clone())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(snaps.len(), 1);
    assert_eq!(snaps[0].triangles.len(), 3);
    assert_eq!(r.host.stats().malformed, 1);
}

#[test]
fn the_host_sends_heartbeats_and_the_watchdog_ends_a_silent_session() {
    let mut c = cfg();
    c.heartbeat = Duration::from_millis(20);
    c.timeout = Duration::from_millis(300);
    let mut r = Rig::connect(Link::host(c).unwrap());
    // a short heartbeat and a short timeout for the guest as well: the timeout is max(own, 4 x theirs)
    r.send(&hello_line("").replace("\"heartbeat_ms\":250", "\"heartbeat_ms\":20"));
    assert!(r.until(|r| r.events.iter().any(|e| matches!(e, Event::Connected(_)))));
    let reply = r.lines()[0].clone();
    let Control::Hello(h) = Control::parse(&reply).unwrap() else {
        panic!()
    };
    r.udp.connect(("127.0.0.1", h.udp_port)).unwrap();
    r.session = h.session.unwrap();
    // the host's heartbeats arrive
    let mut beats = 0;
    let mut b = [0u8; 8193];
    let end = Instant::now() + Duration::from_millis(200);
    while Instant::now() < end {
        r.host.poll();
        while let Ok(n) = r.udp.recv(&mut b) {
            if let Ok((hd, Datagram::Heartbeat)) = decode_datagram(&b[..n]) {
                assert_eq!(hd.session, r.session);
                assert_eq!(hd.version, PROTOCOL_VERSION);
                beats += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(beats >= 3, "{beats} heartbeats");
    // now the peer stays silent
    assert!(r.until(|r| {
        r.events
            .iter()
            .any(|e| matches!(e, Event::Lost { reason } if reason.contains("watchdog")))
    }));
    assert!(
        r.bye_reason().is_some(),
        "the watchdog closes with a bye (section 6.1)"
    );
}

#[test]
fn a_dropped_connection_without_a_bye_is_lost_at_once() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    drop(r.tcp.try_clone().unwrap());
    r.tcp.shutdown(std::net::Shutdown::Both).unwrap();
    assert!(r.until(|r| r.events.iter().any(|e| matches!(e, Event::Lost { .. }))));
}

#[test]
fn the_guest_asks_for_the_focus_and_the_host_may_grant_it() {
    let mut r = Rig::handshake(Link::host(cfg()).unwrap());
    r.send("{\"type\":\"input\",\"focus\":\"guest\"}\n");
    assert!(r.until(|r| r.events.iter().any(|e| matches!(
        e,
        Event::Focus {
            requested: true,
            ..
        }
    ))));
    assert_eq!(
        r.host.focus(),
        Role::Host,
        "a request changes nothing until the host answers"
    );
    r.host.set_focus(Role::Guest).unwrap();
    r.run(50);
    assert!(
        r.lines()
            .iter()
            .any(|l| l.contains("\"type\":\"input\"") && l.contains("\"focus\":\"guest\""))
    );
    assert_eq!(r.host.focus(), Role::Guest);
}

#[test]
fn a_guest_session_does_the_same_from_the_other_side() {
    // a raw host that answers a guest `Link`
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut c = LinkConfig::new("test guest");
    c.control_port = port;
    c.reconnect = Duration::from_millis(20);
    c.token = Some("tok".into());
    let mut guest = Link::guest(c).unwrap();
    let host_udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    let (mut conn, _) = loop {
        guest.poll();
        listener.set_nonblocking(true).unwrap();
        if let Ok(x) = listener.accept() {
            break x;
        }
        std::thread::sleep(Duration::from_millis(3));
    };
    conn.set_nonblocking(true).unwrap();
    let mut buf = String::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && !buf.contains('\n') {
        guest.poll();
        let mut b = [0u8; 1024];
        if let Ok(n) = conn.read(&mut b) {
            buf.push_str(&String::from_utf8_lossy(&b[..n]));
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    let Control::Hello(h) = Control::parse(buf.lines().next().unwrap()).unwrap() else {
        panic!()
    };
    assert_eq!(h.role, Role::Guest);
    assert_eq!(
        h.token.as_deref(),
        Some("tok"),
        "a guest with a token sends it in its hello"
    );
    let reply = format!(
        "{{\"type\":\"hello\",\"role\":\"host\",\"game\":\"raw host\",\"protocol\":1,\"min_protocol\":1,\"udp_port\":{},\
         \"heartbeat_ms\":250,\"timeout_ms\":2000,\"session\":777}}\n",
        host_udp.local_addr().unwrap().port()
    );
    conn.write_all(reply.as_bytes()).unwrap();
    let mut events = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && !events.iter().any(|e| matches!(e, Event::Connected(_))) {
        events.extend(guest.poll());
        std::thread::sleep(Duration::from_millis(3));
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::Connected(p) if p.session == 777 && p.role == Role::Host))
    );
    // the guest's datagrams carry the host's session and arrive at the host's UDP socket
    guest
        .spawn(&rewrite_bridge::link::Spawn {
            entity: 1,
            provider: "p".into(),
            asset: "a".into(),
            name: None,
        })
        .unwrap();
    guest.send_poses(&[pose(1, 10)]).unwrap();
    let mut b = [0u8; 8193];
    host_udp.set_nonblocking(true).unwrap();
    let mut got = None;
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end && got.is_none() {
        guest.poll();
        if let Ok((n, from)) = host_udp.recv_from(&mut b) {
            assert_eq!(from, guest.udp_addr().unwrap());
            if let Ok((hd, Datagram::Pose(p))) = decode_datagram(&b[..n]) {
                got = Some((hd, p));
            }
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    let (hd, poses) = got.expect("a pose datagram");
    assert_eq!(hd.session, 777);
    assert_eq!(poses[0].entity, 1);
}
