//! The link session, host and guest, written from `docs/CONTRACT.md` (revision 6) for a plugin that
//! runs as a WebAssembly component.
//!
//! Why this exists instead of `rewrite_bridge::link::Link`: that type compiles for `wasm32-wasip2` but
//! cannot run there. The host's session number comes from `std::process::id()`, which panics on this
//! target ("no pids on this platform"), and the guest connects from a spawned thread, which this target
//! has none of. So the two roles are done here, on non-blocking standard sockets driven by the server's
//! tick, and only the crate's wire codec is used: [`encode_datagram`], [`decode_datagram`],
//! [`Control::parse`] and [`Control::to_line`], and its plain data types ([`Event`], [`PeerInfo`],
//! [`Stats`], ...). Section numbers below are the contract's.
//!
//! One thing the sandbox does not allow: `SO_RCVBUF` (section 2, "Receive buffer") cannot be set
//! through the standard library on this target, so the UDP receive buffer is the system's default.

use rewrite_bridge::link::{COLLISION_PER_POLL, Collision, MAX_COLLISION_CHUNKS, MAX_TRIANGLES};
use rewrite_bridge::link::{
    CollisionChunk, Control, Datagram, Event, HEARTBEAT_MS_RANGE, Header, Hello, LINGER,
    MAX_DATAGRAM, MAX_ENTITIES, MAX_LINE, MAX_PENDING_CONTROL, MAX_TOKEN_BYTES,
    MIN_PROTOCOL_VERSION, PROTOCOL_VERSION, PeerInfo, Pose, Role, Spawn, Stats, Triangle,
    UNAUTHORIZED, decode_datagram, encode_datagram, negotiate, now_us, seq_newer, token_equal,
};
use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn localhost(port: u16) -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
}

/// Settings for [`Session::host`] and [`Session::guest`].
#[derive(Clone)]
pub struct SessionConfig {
    /// Sent in `hello`.
    pub game: String,
    /// Host: the port to listen on (0 = any free port). Guest: the host's port.
    pub control_port: u16,
    /// This side's UDP port (0 = any free port).
    pub udp_port: u16,
    pub heartbeat: Duration,
    pub timeout: Duration,
    /// Guest: the pause between connection attempts.
    pub reconnect: Duration,
    /// Host: the token a guest must send. Guest: the token to send.
    pub token: Option<String>,
    /// Guest: read the port and the token from this endpoint file before every attempt.
    pub endpoint_file: Option<PathBuf>,
}

impl SessionConfig {
    pub fn new(game: impl Into<String>) -> Self {
        Self {
            game: game.into(),
            control_port: rewrite_bridge::link::DEFAULT_CONTROL_PORT,
            udp_port: 0,
            heartbeat: Duration::from_millis(250),
            timeout: Duration::from_millis(2000),
            reconnect: Duration::from_millis(500),
            token: None,
            endpoint_file: None,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The control connection

struct Ctl {
    stream: TcpStream,
    rbuf: Vec<u8>,
    wbuf: Vec<u8>,
}

enum Fill {
    Open,
    Eof,
    Failed,
}

impl Ctl {
    fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        let _ = stream.set_nodelay(true);
        Ok(Self {
            stream,
            rbuf: Vec::new(),
            wbuf: Vec::new(),
        })
    }

    fn queue(&mut self, c: &Control) {
        self.wbuf.extend_from_slice(c.to_line().as_bytes());
    }

    /// Writes what the socket takes. `false` when the connection broke or more than 1 MiB of control
    /// bytes wait to be sent (section 7, "Full buffers": the peer is lost).
    fn flush(&mut self) -> bool {
        while !self.wbuf.is_empty() {
            match self.stream.write(&self.wbuf) {
                Ok(0) => return false,
                Ok(n) => {
                    self.wbuf.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
        self.wbuf.len() <= MAX_PENDING_CONTROL
    }

    /// Reads what is there into the buffer.
    fn fill(&mut self) -> Fill {
        let mut buf = [0u8; 4096];
        loop {
            // never hold more than a line and a bit: a longer one is an error found by `next_line`
            if self.rbuf.len() > MAX_LINE + buf.len() {
                return Fill::Open;
            }
            match self.stream.read(&mut buf) {
                Ok(0) => return Fill::Eof,
                Ok(n) => self.rbuf.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Fill::Open,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Fill::Failed,
            }
        }
    }

    /// The next complete line without its `\n` and one `\r` before it (section 8, "Syntax of a line", 1).
    /// `Err` is a protocol error: too long (65536 bytes including the newline), or not UTF-8 (2).
    fn next_line(&mut self) -> Option<Result<String, String>> {
        let Some(nl) = self.rbuf.iter().position(|b| *b == b'\n') else {
            return (self.rbuf.len() >= MAX_LINE).then(|| Err("line too long".into()));
        };
        if nl + 1 > MAX_LINE {
            return Some(Err("line too long".into()));
        }
        let mut line: Vec<u8> = self.rbuf.drain(..=nl).collect();
        line.pop();
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(String::from_utf8(line).map_err(|_| "line is not valid UTF-8".into()))
    }
}

/// A closed connection that is still being drained (section 6.1).
struct Closing {
    stream: TcpStream,
    deadline: Instant,
}

// ---------------------------------------------------------------------------------------------
// State of a session

/// Collision snapshot being assembled (section 9.5, "Assembly").
struct Assembly {
    query: u32,
    chunk_count: u16,
    center: [f32; 3],
    radius: f32,
    chunks: Vec<Option<Vec<Triangle>>>,
}

struct Live {
    ctl: Ctl,
    peer: PeerInfo,
    version: u16,
    session: u32,
    last_heard: Instant,
    last_beat: Instant,
    /// The peer's live entities, with the `time_us` of the last pose used for each.
    entities: HashMap<u32, Option<u64>>,
    over_limit_logged: bool,
    newest_camera: Option<u32>,
    newest_query: Option<u32>,
    assembly: Option<Assembly>,
    newest_frame: Option<u64>,
}

enum Conn {
    Idle { next_try: Instant },
    Handshake { ctl: Ctl, since: Instant },
    Live(Box<Live>),
}

/// Why a session ended and what to tell the peer.
struct End {
    reason: String,
    /// Send a `bye` of our own (not after receiving one).
    bye: bool,
    /// Report it as a lost session; otherwise as a failed handshake.
    lost: bool,
}

pub struct Session {
    role: Role,
    cfg: SessionConfig,
    listener: Option<TcpListener>,
    udp: UdpSocket,
    udp_port: u16,
    conn: Conn,
    closing: Vec<Closing>,
    events: Vec<Event>,
    stats: Stats,
    seq: u32,
    next_query: u32,
    collision_queue: VecDeque<CollisionChunk>,
    focus: Role,
    last_error: Option<String>,
}

impl Session {
    /// Section 6, step 1: listens on `127.0.0.1:control_port` and binds the UDP socket. The host
    /// accepts one guest at a time in [`Session::poll`].
    pub fn host(cfg: SessionConfig) -> io::Result<Self> {
        check_token_config(cfg.token.as_deref())?;
        let listener = TcpListener::bind(localhost(cfg.control_port))?;
        listener.set_nonblocking(true)?;
        Self::new(Role::Host, cfg, Some(listener))
    }

    /// Connects, in [`Session::poll`], to a host on `127.0.0.1`, again and again until one answers, and
    /// again after a session ends.
    pub fn guest(cfg: SessionConfig) -> io::Result<Self> {
        check_token_config(cfg.token.as_deref())?;
        Self::new(Role::Guest, cfg, None)
    }

    fn new(role: Role, cfg: SessionConfig, listener: Option<TcpListener>) -> io::Result<Self> {
        let udp = UdpSocket::bind(localhost(cfg.udp_port))?;
        udp.set_nonblocking(true)?;
        let udp_port = udp.local_addr()?.port();
        Ok(Self {
            role,
            cfg,
            listener,
            udp,
            udp_port,
            conn: Conn::Idle {
                next_try: Instant::now(),
            },
            closing: Vec::new(),
            events: Vec::new(),
            stats: Stats::default(),
            seq: 0,
            next_query: 1,
            collision_queue: VecDeque::new(),
            focus: Role::Host,
            last_error: None,
        })
    }

    /// Why the last connection attempt of a guest failed (the kind of the I/O error; never a token).
    pub fn last_error(&self) -> Option<&str> {
        self.last_error.as_deref()
    }

    pub fn role(&self) -> Role {
        self.role
    }

    /// Host: the port actually bound. Guest: `None`.
    pub fn control_addr(&self) -> Option<SocketAddr> {
        self.listener.as_ref().and_then(|l| l.local_addr().ok())
    }

    pub fn udp_addr(&self) -> SocketAddr {
        localhost(self.udp_port)
    }

    pub fn peer(&self) -> Option<&PeerInfo> {
        match &self.conn {
            Conn::Live(l) => Some(&l.peer),
            _ => None,
        }
    }

    pub fn is_connected(&self) -> bool {
        matches!(self.conn, Conn::Live(_))
    }

    pub fn focus(&self) -> Role {
        self.focus
    }

    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn heartbeat_ms(&self) -> u32 {
        // section 8, `hello`: a side whose own value is outside the range sends the nearest one inside it
        (self.cfg.heartbeat.as_millis().min(u128::from(u32::MAX)) as u32)
            .clamp(*HEARTBEAT_MS_RANGE.start(), *HEARTBEAT_MS_RANGE.end())
    }

    fn own_timeout(&self) -> Duration {
        self.cfg.timeout
    }

    fn hello(&self, protocol: u16, session: Option<u32>, token: Option<String>) -> Control {
        Control::Hello(Hello {
            role: self.role,
            game: self.cfg.game.clone(),
            protocol,
            min_protocol: MIN_PROTOCOL_VERSION,
            udp_port: self.udp_port,
            heartbeat_ms: self.heartbeat_ms(),
            timeout_ms: self.cfg.timeout.as_millis().min(u128::from(u32::MAX)) as u32,
            session,
            // only a guest's hello carries a token (section 6.2, 1)
            token: if self.role == Role::Guest {
                token
            } else {
                None
            },
        })
    }

    // -----------------------------------------------------------------------------------------
    // poll

    /// Does the I/O of one tick and returns what happened. Never blocks.
    pub fn poll(&mut self) -> Vec<Event> {
        let now = Instant::now();
        self.drain_closing(now);
        self.poll_connect(now);
        self.poll_control(now);
        self.poll_udp(now);
        self.poll_timers(now);
        self.pump_collision();
        std::mem::take(&mut self.events)
    }

    fn poll_connect(&mut self, now: Instant) {
        if let Some(listener) = &self.listener {
            let mut busy = Vec::new();
            while let Ok((stream, _)) = listener.accept() {
                let Ok(ctl) = Ctl::new(stream) else { continue };
                if matches!(self.conn, Conn::Idle { .. }) {
                    self.conn = Conn::Handshake { ctl, since: now };
                } else {
                    busy.push(ctl);
                }
            }
            // section 2: while a guest is connected, any other connection gets `bye` "busy"
            for ctl in busy {
                self.retire(ctl, Some("busy".into()), now);
            }
            return;
        }
        let Conn::Idle { next_try } = &self.conn else {
            return;
        };
        if now < *next_try {
            return;
        }
        let retry = Conn::Idle {
            next_try: now + self.cfg.reconnect,
        };
        let (port, token) = match &self.cfg.endpoint_file {
            // section 6.2, "Reading": before every attempt, since the host may have restarted
            Some(path) => match rewrite_bridge::link::Endpoint::read(path) {
                Ok(e) => (e.control_port, Some(e.token)),
                Err(e) => {
                    self.last_error = Some(format!("endpoint file: {}", e.kind()));
                    self.conn = retry;
                    return;
                }
            },
            None => (self.cfg.control_port, self.cfg.token.clone()),
        };
        match TcpStream::connect(localhost(port)).and_then(Ctl::new) {
            Ok(mut ctl) => {
                self.last_error = None;
                let hello = self.hello(PROTOCOL_VERSION, None, token);
                ctl.queue(&hello);
                self.stats.lines_sent += 1;
                if ctl.flush() {
                    self.conn = Conn::Handshake { ctl, since: now };
                } else {
                    self.conn = retry;
                }
            }
            Err(e) => {
                self.last_error = Some(format!(
                    "cannot connect to 127.0.0.1:{port}: {:?}",
                    e.kind()
                ));
                self.conn = retry;
            }
        }
    }

    fn drain_closing(&mut self, now: Instant) {
        self.closing.retain_mut(|c| {
            let mut buf = [0u8; 1024];
            loop {
                match c.stream.read(&mut buf) {
                    Ok(0) => return false,
                    Ok(_) => {}
                    Err(e) if e.kind() == ErrorKind::WouldBlock => return now < c.deadline,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => return false,
                }
            }
        });
    }

    /// Section 6.1: `bye` (if any), flush, half-close, and drain without blocking the tick.
    fn retire(&mut self, mut ctl: Ctl, bye: Option<String>, now: Instant) {
        if let Some(reason) = bye {
            ctl.queue(&Control::Bye { reason });
            self.stats.lines_sent += 1;
            let _ = ctl.flush();
        }
        let _ = ctl.stream.shutdown(Shutdown::Write);
        self.closing.push(Closing {
            stream: ctl.stream,
            deadline: now + LINGER,
        });
    }

    fn end_session(&mut self, end: End, now: Instant) {
        let old = std::mem::replace(
            &mut self.conn,
            Conn::Idle {
                next_try: now + self.cfg.reconnect,
            },
        );
        let ctl = match old {
            Conn::Handshake { ctl, .. } => ctl,
            Conn::Live(l) => l.ctl,
            Conn::Idle { .. } => return,
        };
        self.retire(ctl, end.bye.then(|| end.reason.clone()), now);
        self.collision_queue.clear();
        self.focus = Role::Host;
        self.events.push(if end.lost {
            Event::Lost { reason: end.reason }
        } else {
            Event::HandshakeFailed { reason: end.reason }
        });
    }

    // -----------------------------------------------------------------------------------------
    // control channel

    fn poll_control(&mut self, now: Instant) {
        // Handshake: the first line is a `hello` or a `bye` (section 6).
        if let Conn::Handshake { ctl, since } = &mut self.conn {
            let since = *since;
            let fill = ctl.fill();
            let mut first = None;
            if let Some(l) = ctl.next_line() {
                first = Some(l);
            }
            if !ctl.flush() {
                self.end_session(
                    End {
                        reason: "connection lost".into(),
                        bye: false,
                        lost: false,
                    },
                    now,
                );
                return;
            }
            match first {
                Some(Ok(line)) => {
                    self.stats.lines_received += 1;
                    self.on_first_line(&line, now);
                }
                Some(Err(why)) => self.end_session(
                    End {
                        reason: format!("protocol error: {why}"),
                        bye: true,
                        lost: false,
                    },
                    now,
                ),
                None => {
                    if !matches!(fill, Fill::Open) {
                        self.end_session(
                            End {
                                reason: "connection closed before hello".into(),
                                bye: false,
                                lost: false,
                            },
                            now,
                        );
                    } else if now.duration_since(since) > self.own_timeout() {
                        // section 6: no `hello` within our own `timeout_ms` of the connection
                        self.end_session(
                            End {
                                reason: "no hello in time".into(),
                                bye: true,
                                lost: false,
                            },
                            now,
                        );
                    }
                }
            }
        }
        // Live
        let Conn::Live(live) = &mut self.conn else {
            return;
        };
        let fill = live.ctl.fill();
        let mut lines = Vec::new();
        let mut bad = None;
        while let Some(l) = live.ctl.next_line() {
            match l {
                Ok(l) => lines.push(l),
                Err(why) => {
                    bad = Some(why);
                    break;
                }
            }
        }
        let flushed = live.ctl.flush();
        for line in lines {
            self.stats.lines_received += 1;
            if let Some(end) = self.on_live_line(&line, now) {
                self.end_session(end, now);
                return;
            }
        }
        if let Some(why) = bad {
            self.end_session(
                End {
                    reason: format!("protocol error: {why}"),
                    bye: true,
                    lost: true,
                },
                now,
            );
        } else if !flushed {
            self.end_session(
                End {
                    reason: "control channel stalled or broken".into(),
                    bye: false,
                    lost: true,
                },
                now,
            );
        } else if !matches!(fill, Fill::Open) {
            // EOF or an error with no `bye` before it: the peer is lost at once (section 7)
            self.end_session(
                End {
                    reason: "connection closed by the peer".into(),
                    bye: false,
                    lost: true,
                },
                now,
            );
        }
    }

    fn on_first_line(&mut self, line: &str, now: Instant) {
        let parsed = Control::parse(line);
        let fail = |s: &mut Self, reason: String, bye: bool| {
            s.end_session(
                End {
                    reason,
                    bye,
                    lost: false,
                },
                now,
            );
        };
        match parsed {
            Err(e) => fail(self, format!("protocol error: {e}"), true),
            Ok(Control::Bye { reason }) => {
                // a `bye` first: the handshake failed, we close without a `bye` of our own (section 6)
                fail(self, format!("peer said bye: {reason}"), false);
            }
            Ok(Control::Hello(h)) => self.on_hello(h, now),
            Ok(_) => fail(self, "the first line must be a hello".into(), true),
        }
    }

    fn on_hello(&mut self, h: Hello, now: Instant) {
        let fail = |s: &mut Self, reason: String| {
            s.end_session(
                End {
                    reason,
                    bye: true,
                    lost: false,
                },
                now,
            );
        };
        // section 6.2, 2: the token first, before any check that would tell a stranger something
        if self.role == Role::Host
            && let Some(want) = &self.cfg.token
        {
            let ok = h.token.as_deref().is_some_and(|got| {
                (1..=MAX_TOKEN_BYTES).contains(&got.len())
                    && token_equal(got.as_bytes(), want.as_bytes())
            });
            if !ok {
                return fail(self, UNAUTHORIZED.into());
            }
        }
        let expected = match self.role {
            Role::Host => Role::Guest,
            Role::Guest => Role::Host,
        };
        if h.role != expected {
            return fail(self, format!("peer claims role {}", h.role.as_str()));
        }
        if h.udp_port == 0 {
            return fail(self, "peer announced udp_port 0".into());
        }
        if !HEARTBEAT_MS_RANGE.contains(&h.heartbeat_ms) {
            return fail(
                self,
                format!(
                    "peer announced heartbeat_ms {}, outside {}..={}",
                    h.heartbeat_ms,
                    HEARTBEAT_MS_RANGE.start(),
                    HEARTBEAT_MS_RANGE.end()
                ),
            );
        }
        let (version, session) = match self.role {
            Role::Host => {
                let Some(v) = negotiate(
                    (MIN_PROTOCOL_VERSION, PROTOCOL_VERSION),
                    (h.min_protocol, h.protocol),
                ) else {
                    return fail(
                        self,
                        format!(
                            "no common protocol version (this host speaks {MIN_PROTOCOL_VERSION} to {PROTOCOL_VERSION})"
                        ),
                    );
                };
                let Some(session) = random_session() else {
                    return fail(self, "no source of random bytes for the session".into());
                };
                (v, session)
            }
            Role::Guest => {
                // section 6, step 4: the host's version must be inside our range, and it names a session
                if !(MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&h.protocol) {
                    return fail(
                        self,
                        format!(
                            "host chose protocol {}, outside {MIN_PROTOCOL_VERSION}..={PROTOCOL_VERSION}",
                            h.protocol
                        ),
                    );
                }
                match h.session {
                    Some(s) if s != 0 => (h.protocol, s),
                    _ => return fail(self, "host's hello has no valid session".into()),
                }
            }
        };
        let peer = PeerInfo {
            game: h.game.clone(),
            role: h.role,
            protocol: version,
            session,
            udp_addr: localhost(h.udp_port),
            heartbeat: Duration::from_millis(u64::from(h.heartbeat_ms)),
            // section 7: max(own timeout_ms, 4 * the peer's heartbeat_ms)
            timeout: self
                .own_timeout()
                .max(Duration::from_millis(4 * u64::from(h.heartbeat_ms))),
        };
        let reply = (self.role == Role::Host).then(|| self.hello(version, Some(session), None));
        let old = std::mem::replace(
            &mut self.conn,
            Conn::Idle {
                next_try: now + self.cfg.reconnect,
            },
        );
        let Conn::Handshake { mut ctl, .. } = old else {
            self.conn = old;
            return;
        };
        if let Some(r) = reply {
            ctl.queue(&r);
            self.stats.lines_sent += 1;
        }
        self.focus = Role::Host;
        self.events.push(Event::Connected(peer.clone()));
        self.conn = Conn::Live(Box::new(Live {
            ctl,
            peer,
            version,
            session,
            last_heard: now,
            last_beat: now,
            entities: HashMap::new(),
            over_limit_logged: false,
            newest_camera: None,
            newest_query: None,
            assembly: None,
            newest_frame: None,
        }));
        // bytes that followed the hello in the same read are live lines (a peer may spawn at once)
    }

    fn on_live_line(&mut self, line: &str, now: Instant) -> Option<End> {
        let protocol_error = |why: String| End {
            reason: format!("protocol error: {why}"),
            bye: true,
            lost: true,
        };
        let msg = match Control::parse(line) {
            Ok(m) => m,
            Err(e) => return Some(protocol_error(e.to_string())),
        };
        let Conn::Live(live) = &mut self.conn else {
            return None;
        };
        live.last_heard = now;
        match msg {
            // section 6: a second `hello` is a protocol error
            Control::Hello(_) => return Some(protocol_error("a second hello".into())),
            Control::Bye { reason } => {
                return Some(End {
                    reason: format!("peer said bye: {reason}"),
                    bye: false,
                    lost: true,
                });
            }
            Control::Spawn(s) => {
                // section 8, `spawn`: at most 1024 live entities; a spawn of a live id replaces it
                if live.entities.contains_key(&s.entity) || live.entities.len() < MAX_ENTITIES {
                    live.entities.insert(s.entity, None);
                    self.events.push(Event::Spawn(s));
                } else {
                    self.stats.spawn_ignored += 1;
                    live.over_limit_logged = true;
                }
            }
            Control::Despawn { entity } => {
                // a despawn of an id that is not live is ignored
                if live.entities.remove(&entity).is_some() {
                    self.events.push(Event::Despawn { entity });
                }
            }
            Control::Input { focus, .. } => match self.role {
                Role::Host => self.events.push(Event::Focus {
                    focus,
                    requested: true,
                }),
                Role::Guest => {
                    self.focus = focus;
                    self.events.push(Event::Focus {
                        focus,
                        requested: false,
                    });
                }
            },
            Control::FrameChannel(c) => {
                // section 8, `frame_channel`: a new channel resets the frame staleness
                live.newest_frame = None;
                self.events.push(Event::FrameChannel(c));
            }
            Control::Unknown(_) => {}
        }
        None
    }

    // -----------------------------------------------------------------------------------------
    // datagrams

    fn poll_udp(&mut self, now: Instant) {
        let mut buf = vec![0u8; MAX_DATAGRAM + 1]; // section 2: at least 8193 bytes
        loop {
            let (n, from) = match self.udp.recv_from(&mut buf) {
                Ok(x) => x,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let Conn::Live(live) = &mut self.conn else {
                self.stats.foreign += 1;
                continue;
            };
            // section 2: only from the exact address the peer announced
            if from != live.peer.udp_addr {
                self.stats.foreign += 1;
                continue;
            }
            let data = &buf[..n];
            let decoded = if n > MAX_DATAGRAM {
                // section 2: too long. A known kind is malformed; an unknown one is ignored
                match data.get(6) {
                    Some(1..=5) => Err(()),
                    _ => decode_datagram(&data[..MAX_DATAGRAM.min(n)]).map_err(|_| ()),
                }
            } else {
                decode_datagram(data).map_err(|_| ())
            };
            let Ok((header, body)) = decoded else {
                self.stats.malformed += 1;
                continue;
            };
            // section 6, step 5: another session or version is dropped silently
            if header.session != live.session || header.version != live.version {
                self.stats.foreign += 1;
                continue;
            }
            live.last_heard = now;
            self.stats.datagrams_received += 1;
            Self::on_datagram(live, &mut self.stats, &mut self.events, header, body);
        }
    }

    fn on_datagram(
        live: &mut Live,
        stats: &mut Stats,
        events: &mut Vec<Event>,
        header: Header,
        body: Datagram,
    ) {
        match body {
            Datagram::Heartbeat | Datagram::Unknown { .. } => {}
            Datagram::Camera(camera) => {
                // section 7: drop a camera whose seq is not newer than the newest received
                if live
                    .newest_camera
                    .is_some_and(|n| !seq_newer(header.seq, n))
                {
                    stats.stale += 1;
                } else {
                    live.newest_camera = Some(header.seq);
                    events.push(Event::Camera { header, camera });
                }
            }
            Datagram::Pose(poses) => {
                let mut kept: Vec<Pose> = Vec::new();
                for p in poses {
                    match live.entities.get_mut(&p.entity) {
                        None => stats.unknown_entity += 1,
                        // section 7: per entity, drop a record whose time_us is older than the last used
                        Some(last) if last.is_some_and(|t| p.time_us < t) => stats.stale += 1,
                        Some(last) => {
                            *last = Some(p.time_us);
                            kept.push(p);
                        }
                    }
                }
                if !kept.is_empty() {
                    events.push(Event::Poses {
                        header,
                        poses: kept,
                    });
                }
            }
            Datagram::Collision(chunk) => {
                if let Some(c) = Self::assemble(live, stats, chunk) {
                    events.push(Event::Collision(c));
                }
            }
            Datagram::Frame(frame) => {
                // section 7: only a frame number newer than the newest one shown from this pixel file
                if live.newest_frame.is_some_and(|n| frame.frame <= n) {
                    stats.stale += 1;
                } else {
                    live.newest_frame = Some(frame.frame);
                    events.push(Event::Frame { header, frame });
                }
            }
        }
    }

    /// Section 9.5, "Assembly", rules 1 to 5.
    fn assemble(live: &mut Live, stats: &mut Stats, c: CollisionChunk) -> Option<Collision> {
        // (the decoder already refused a chunk_count of 0 or above 4096, and an index outside it)
        if c.chunk_count == 0
            || c.chunk_count > MAX_COLLISION_CHUNKS
            || c.chunk_index >= c.chunk_count
        {
            stats.malformed += 1;
            return None;
        }
        // 1: stale when a query was completed and this one is not newer
        if live.newest_query.is_some_and(|n| !seq_newer(c.query, n)) {
            stats.stale += 1;
            return None;
        }
        if let Some(a) = &live.assembly {
            if a.query == c.query {
                // 2: another chunk_count for the query being assembled is malformed; the assembly goes on
                if a.chunk_count != c.chunk_count {
                    stats.malformed += 1;
                    return None;
                }
            } else if seq_newer(a.query, c.query) {
                // 2: the assembly is newer than this chunk
                stats.stale += 1;
                return None;
            } else {
                live.assembly = None; // 3: a newer query discards the incomplete older one
            }
        }
        // 3: the first chunk to arrive decides query, chunk_count, center and radius
        let a = live.assembly.get_or_insert_with(|| Assembly {
            query: c.query,
            chunk_count: c.chunk_count,
            center: c.center,
            radius: c.radius,
            chunks: vec![None; usize::from(c.chunk_count)],
        });
        let slot = &mut a.chunks[usize::from(c.chunk_index)];
        // 4: the first copy of a chunk wins
        if slot.is_some() {
            stats.stale += 1;
            return None;
        }
        *slot = Some(c.triangles);
        // 5: complete when every index is filled
        if a.chunks.iter().any(Option::is_none) {
            return None;
        }
        let done = live.assembly.take()?;
        live.newest_query = Some(done.query);
        Some(Collision {
            query: done.query,
            center: done.center,
            radius: done.radius,
            triangles: done.chunks.into_iter().flatten().flatten().collect(),
        })
    }

    fn poll_timers(&mut self, now: Instant) {
        let beat = Duration::from_millis(u64::from(self.heartbeat_ms()));
        let Conn::Live(live) = &mut self.conn else {
            return;
        };
        // section 7: the watchdog uses the monotonic clock, never the datagram timestamps
        if now.duration_since(live.last_heard) > live.peer.timeout {
            let ms = now.duration_since(live.last_heard).as_millis();
            self.end_session(
                End {
                    reason: format!("peer silent for {ms} ms"),
                    bye: true,
                    lost: true,
                },
                now,
            );
            return;
        }
        if now.duration_since(live.last_beat) >= beat {
            live.last_beat = now;
            self.send(&Datagram::Heartbeat);
        }
    }

    /// Sends a datagram to the peer; a send that would block is dropped (section 7, "Full buffers").
    fn send(&mut self, body: &Datagram) -> Option<u32> {
        let Conn::Live(live) = &self.conn else {
            return None;
        };
        self.seq = self.seq.wrapping_add(1);
        let header = Header {
            version: live.version,
            flags: 0,
            session: live.session,
            seq: self.seq,
            time_us: now_us(),
        };
        let mut out = Vec::with_capacity(256);
        if encode_datagram(&header, body, &mut out).is_err() {
            return None;
        }
        match self.udp.send_to(&out, live.peer.udp_addr) {
            Ok(_) => {
                self.stats.datagrams_sent += 1;
                Some(self.seq)
            }
            Err(_) => {
                self.stats.send_dropped += 1;
                None
            }
        }
    }

    fn pump_collision(&mut self) {
        // section 9.5, "Pacing": at most 32 collision datagrams per tick
        for _ in 0..COLLISION_PER_POLL {
            let Some(chunk) = self.collision_queue.pop_front() else {
                break;
            };
            if self.send(&Datagram::Collision(chunk)).is_none() && !self.is_connected() {
                self.collision_queue.clear();
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // sending

    fn send_control(&mut self, c: &Control) -> Result<(), String> {
        let Conn::Live(live) = &mut self.conn else {
            return Err("not connected".into());
        };
        live.ctl.queue(c);
        self.stats.lines_sent += 1;
        if live.ctl.flush() {
            Ok(())
        } else {
            Err("control channel stalled or broken".into())
        }
    }

    /// Sends poses, at most 102 per datagram (section 9.4).
    pub fn send_poses(&mut self, poses: &[Pose]) -> Result<(), String> {
        if !self.is_connected() {
            return Err("not connected".into());
        }
        for chunk in poses.chunks(rewrite_bridge::link::MAX_POSES) {
            self.send(&Datagram::Pose(chunk.to_vec()));
        }
        Ok(())
    }

    /// Queues a collision snapshot (section 9.5). A snapshot queued while an older one still has chunks
    /// waiting replaces them. Returns its query number.
    pub fn send_collision(
        &mut self,
        center: [f32; 3],
        radius: f32,
        triangles: &[Triangle],
    ) -> Result<u32, String> {
        if !self.is_connected() {
            return Err("not connected".into());
        }
        if triangles.len().div_ceil(MAX_TRIANGLES) > usize::from(MAX_COLLISION_CHUNKS) {
            return Err("too many triangles: a snapshot has at most 4096 chunks".into());
        }
        let c = Collision {
            query: self.next_query,
            center,
            radius,
            triangles: triangles.to_vec(),
        };
        self.collision_queue = c.chunks().map_err(|e| e.to_string())?.into();
        self.next_query = self.next_query.wrapping_add(1);
        Ok(c.query)
    }

    pub fn spawn(&mut self, s: &Spawn) -> Result<(), String> {
        self.send_control(&Control::Spawn(s.clone()))
    }

    pub fn despawn(&mut self, entity: u32) -> Result<(), String> {
        self.send_control(&Control::Despawn { entity })
    }

    /// Host: gives the focus (section 8, `input`). Guest: asks for it.
    pub fn set_focus(&mut self, focus: Role) -> Result<(), String> {
        self.send_control(&Control::Input {
            focus,
            reason: None,
        })?;
        if self.role == Role::Host {
            self.focus = focus;
        }
        Ok(())
    }

    /// Ends the session with a `bye` and closes as section 6.1 says, without blocking.
    pub fn disconnect(&mut self, reason: &str) {
        let now = Instant::now();
        if !matches!(self.conn, Conn::Idle { .. }) {
            self.end_session(
                End {
                    reason: reason.into(),
                    bye: true,
                    lost: true,
                },
                now,
            );
            self.events.pop(); // the caller knows; no event for its own disconnect
        }
    }

    /// Shuts down: `bye`, half-close, then drain for at most 200 ms (section 6.1, steps 1 to 4). Blocks
    /// for up to that long, so it is for stopping, not for the tick.
    pub fn shutdown(mut self) {
        self.disconnect("shutting down");
        let end = Instant::now() + LINGER;
        while !self.closing.is_empty() && Instant::now() < end {
            self.drain_closing(Instant::now());
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

fn check_token_config(t: Option<&str>) -> io::Result<()> {
    if let Some(t) = t
        && (!(1..=MAX_TOKEN_BYTES).contains(&t.len()) || t.contains('\0'))
    {
        // the error never quotes the token
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "token must be 1 to 256 bytes of UTF-8 without U+0000",
        ));
    }
    Ok(())
}

/// Section 6, step 3: a fresh nonzero random session number, from the sandbox's source of random bytes.
fn random_session() -> Option<u32> {
    for _ in 0..8 {
        let mut b = [0u8; 4];
        getrandom::fill(&mut b).ok()?;
        let s = u32::from_le_bytes(b);
        if s != 0 {
            return Some(s);
        }
    }
    None
}
