//! Text from the peer (the `game` of its `hello`, the reason of its `bye`) reaches `/link status`, which a player
//! or the console reads, with no control character and cut to a short length, whatever the peer sent.

mod common;

use common::{RawGuest, tick};
use pumpkin_link::bridge::{Bridge, MAX_PEER_TEXT_CHARS, NoGuests};
use pumpkin_link::config::{Config, Role};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Escape sequences, a forged line, C1 controls (U+0085 next line, U+009B the one-character CSI) and a
/// right-to-left override, then 60 000 characters: a control line may be 65 536 bytes long.
fn evil() -> String {
    format!(
        "evil\u{1b}[2J\n[Server] FORGED\r\u{85}\u{9b}31m\u{202e}{}",
        "x".repeat(60_000)
    )
}

/// What a status line may hold of peer text: the fixed words around it, and one text of at most
/// `MAX_PEER_TEXT_CHARS` characters plus the `...` that says it was cut.
const MAX_LINE_CHARS: usize = 120 + MAX_PEER_TEXT_CHARS + 3;

fn assert_clean(status: &[String]) {
    for l in status {
        assert!(
            !l.chars().any(char::is_control) && !l.contains('\u{202e}'),
            "{l:?}"
        );
        assert!(l.chars().count() <= MAX_LINE_CHARS, "{} chars", l.len());
    }
}

#[test]
fn the_game_and_the_bye_reason_of_the_peer_are_cleaned_and_cut() {
    let mut b = Bridge::new(
        Config {
            port: 0,
            ..Config::default()
        },
        PathBuf::from("endpoint.json"),
    );
    b.start(Role::Host).unwrap();
    let mut peer = RawGuest::connect_as(&mut b, &evil());
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end && !b.is_connected() {
        tick(&mut b);
    }
    let status = b.status();
    assert!(
        status.iter().any(|l| l.starts_with("peer: evil ")),
        "{status:?}"
    );
    assert_clean(&status);

    let bye = format!(
        "{{\"type\":\"bye\",\"reason\":{}}}\n",
        serde_json::to_string(&evil()).unwrap()
    );
    peer.write_line(&bye);
    while Instant::now() < end && b.counters.lost == 0 {
        tick(&mut b);
    }
    assert_eq!(b.counters.lost, 1);
    let status = b.status();
    assert!(
        status.iter().any(|l| l.starts_with("last session ended: ")),
        "{status:?}"
    );
    assert_clean(&status);
    b.stop(&mut NoGuests);
}
