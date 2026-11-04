//! A guest written by hand on raw sockets, for tests of the real `Bridge` as a host. Used by several test
//! files, and each uses part of it.
#![allow(dead_code)]

use pumpkin_link::bridge::{Bridge, GuestSink, NoGuests, PlayerSample, Tick, Worlds};
use rewrite_bridge::link::{Control, Datagram, Header, decode_datagram, encode_datagram, now_us};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

pub fn localhost(port: u16) -> SocketAddr {
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

/// One server tick of `b`, with no players and no world, then a short pause.
pub fn tick(b: &mut Bridge) {
    tick_with(b, &[], &mut pumpkin_link::bridge::NoWorlds, &mut NoGuests);
}

/// One server tick of `b` with the given players and world.
pub fn tick_with(
    b: &mut Bridge,
    players: &[PlayerSample],
    worlds: &mut dyn Worlds,
    guests: &mut dyn GuestSink,
) {
    b.tick(Tick {
        now: Instant::now(),
        players,
        worlds,
        guests,
    });
    std::thread::sleep(Duration::from_millis(3));
}

pub struct RawGuest {
    pub tcp: TcpStream,
    pub udp: UdpSocket,
    pub session: u32,
    seq: u32,
}

impl RawGuest {
    /// Connects to the bridge (a host) and completes the handshake by hand, ticking the bridge meanwhile.
    pub fn connect(b: &mut Bridge) -> Self {
        let port = b.control_port().unwrap();
        let mut tcp = TcpStream::connect(localhost(port)).unwrap();
        tcp.set_nonblocking(true).unwrap();
        let udp = UdpSocket::bind(localhost(0)).unwrap();
        udp.set_nonblocking(true).unwrap();
        let hello = format!(
            "{{\"type\":\"hello\",\"role\":\"guest\",\"game\":\"raw\",\"protocol\":1,\"min_protocol\":1,\
             \"udp_port\":{},\"heartbeat_ms\":250,\"timeout_ms\":2000}}\n",
            udp.local_addr().unwrap().port()
        );
        tcp.write_all(hello.as_bytes()).unwrap();
        let mut text = String::new();
        let end = Instant::now() + Duration::from_secs(5);
        let host = loop {
            tick(b);
            let mut buf = [0u8; 1024];
            if let Ok(n) = tcp.read(&mut buf) {
                text.push_str(&String::from_utf8_lossy(&buf[..n]));
            }
            if text.contains('\n') {
                match Control::parse(text.lines().next().unwrap()).unwrap() {
                    Control::Hello(h) => break h,
                    other => panic!("{other:?}"),
                }
            }
            assert!(Instant::now() < end, "no hello from the host");
        };
        udp.connect(localhost(host.udp_port)).unwrap();
        Self {
            tcp,
            udp,
            session: host.session.unwrap(),
            seq: 0,
        }
    }

    pub fn write_line(&mut self, line: &str) {
        self.tcp.write_all(line.as_bytes()).unwrap();
    }

    pub fn datagram(&mut self, body: &Datagram) {
        let header = Header {
            version: 1,
            flags: 0,
            session: self.session,
            seq: self.seq,
            time_us: now_us(),
        };
        self.seq += 1;
        let mut out = Vec::new();
        encode_datagram(&header, body, &mut out).unwrap();
        self.udp.send(&out).unwrap();
    }

    /// The datagrams that arrived since the last call.
    pub fn received(&self) -> Vec<Datagram> {
        let mut out = Vec::new();
        let mut buf = [0u8; 8193];
        while let Ok(n) = self.udp.recv(&mut buf) {
            if let Ok((_, body)) = decode_datagram(&buf[..n]) {
                out.push(body);
            }
        }
        out
    }
}
