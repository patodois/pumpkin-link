//! The token and the endpoint file of CONTRACT section 6.2, for a host that runs inside the plugin
//! sandbox.
//!
//! `rewrite-bridge` has `generate_token` and `Endpoint::write`, but both ask the operating system for
//! random bytes through `libc` or `/dev/urandom`, which a WebAssembly component does not have, so they
//! fail on `wasm32-wasip2`. This module does the same two jobs with the sandbox's own source of random
//! bytes (WASI `random`, which the host fills from the operating system) and the standard library's
//! file API. Reading is left to `rewrite_bridge::link::Endpoint`, which works as it is.
//!
//! What the sandbox cannot do: WASI has no file mode bits, so inside the plugin the file cannot be
//! created with `0600` from the start as section 6.2 asks (a native build of this module does ask for
//! it). The plugin's file gets whatever mode the server process's umask gives it; see the README
//! ("Token") for what to do about that.

use rewrite_bridge::link::MAX_TOKEN_BYTES;
use std::fs::OpenOptions;
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};

fn random(buf: &mut [u8]) -> io::Result<()> {
    getrandom::fill(buf).map_err(|_| io::Error::other("no source of random bytes"))
}

/// Section 6.2, "Generating": 32 random bytes as 64 lowercase hex digits. Fails rather than fall back
/// to a weaker source.
pub fn generate_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    random(&mut bytes)?;
    Ok(to_hex(&bytes))
}

fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(b"0123456789abcdef"[usize::from(b >> 4)]));
        s.push(char::from(b"0123456789abcdef"[usize::from(b & 15)]));
    }
    s
}

/// The text of the file: one JSON object with `control_port` and `token`, ending in a newline
/// (section 6.2, "The endpoint file").
pub fn endpoint_text(control_port: u16, token: &str) -> io::Result<String> {
    if control_port == 0 {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "endpoint file: control_port 0",
        ));
    }
    if !(1..=MAX_TOKEN_BYTES).contains(&token.len()) || token.contains('\0') {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            "token must be 1 to 256 bytes of UTF-8 without U+0000",
        ));
    }
    let mut text = serde_json::json!({ "control_port": control_port, "token": token }).to_string();
    text.push('\n');
    Ok(text)
}

/// Writes the file atomically (section 6.2, "Writing"): a temporary file in the same directory, created
/// exclusively, written and closed, then renamed over `path`. Call it only after the control port is
/// bound, with the port that was bound. Errors never quote the token.
pub fn write(path: &Path, control_port: u16, token: &str) -> io::Result<()> {
    let text = endpoint_text(control_port, token)?;
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "endpoint file: no file name"))?;
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let mut nonce = [0u8; 4];
    random(&mut nonce)?;
    let tmp: PathBuf = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        to_hex(&nonce)
    ));
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    // Only where the target has mode bits (not the plugin's WebAssembly target): owner-only from the
    // moment the file exists, as section 6.2 asks.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    let done = f
        .write_all(text.as_bytes())
        .and_then(|()| f.sync_all())
        .and_then(|()| {
            drop(f);
            std::fs::rename(&tmp, path)
        });
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}

/// Removes the file when the host shuts down cleanly. A file that is already gone is fine.
pub fn remove(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rewrite_bridge::link::{Endpoint, token_equal};

    fn temp_dir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("pumpkin-link-test-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_token_is_64_lowercase_hex_digits_and_new_each_time() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
        );
        assert!(!token_equal(a.as_bytes(), b.as_bytes()));
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(to_hex(&[0x00, 0x9f, 0xff]), "009fff");
    }

    #[test]
    fn the_text_is_the_contracts_example_shape_and_the_crate_reads_it() {
        let t = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let text = endpoint_text(47800, t).unwrap();
        assert_eq!(
            text,
            format!("{{\"control_port\":47800,\"token\":\"{t}\"}}\n")
        );
        let e = Endpoint::parse(text.as_bytes()).unwrap();
        assert_eq!(e.control_port, 47800);
        assert_eq!(e.token, t);
    }

    #[test]
    fn port_zero_and_bad_tokens_are_refused_without_quoting_the_token() {
        assert!(endpoint_text(0, "abc").is_err());
        assert!(endpoint_text(1, "").is_err());
        assert!(endpoint_text(1, &"x".repeat(257)).is_err());
        assert!(endpoint_text(1, "a\0b").is_err());
        let e = endpoint_text(1, &"secret".repeat(100))
            .unwrap_err()
            .to_string();
        assert!(!e.contains("secret"));
        assert!(endpoint_text(1, &"x".repeat(256)).is_ok());
    }

    #[test]
    fn writing_replaces_atomically_and_leaves_no_temporary_file() {
        let dir = temp_dir("write");
        let path = dir.join("endpoint.json");
        write(&path, 40001, "first").unwrap();
        write(&path, 40002, "second").unwrap();
        let e = Endpoint::parse(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!((e.control_port, e.token.as_str()), (40002, "second"));
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["endpoint.json".to_string()]);
        remove(&path);
        assert!(!path.exists());
        remove(&path); // gone already: fine
        let _ = std::fs::remove_dir(&dir);
    }

    #[test]
    fn a_failed_write_cleans_up() {
        let dir = temp_dir("fail");
        // the target is a directory, so the rename fails
        let path = dir.join("target");
        std::fs::create_dir_all(&path).unwrap();
        assert!(write(&path, 40001, "t").is_err());
        let names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["target".to_string()]);
        let _ = std::fs::remove_dir(&path);
        let _ = std::fs::remove_dir(&dir);
    }
}
