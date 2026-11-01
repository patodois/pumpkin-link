//! The JSON configuration in the plugin's data folder, modelled on `config/minecraft-link.json` of
//! minecraft-link. Only the keys that apply to a server are here; unknown keys are ignored. A key
//! with the wrong type is an error that names the key, so a typo does not silently fall back.

use crate::mapping::Mapping;
use rewrite_bridge::link::{DEFAULT_CONTROL_PORT, HEARTBEAT_MS_RANGE};
use serde_json::{Map, Value};

/// Which side of the link the server is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// Listens for one guest (the other game).
    Host,
    /// Connects to the other game's host.
    Guest,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    /// Start this role when the plugin loads. The commands start one otherwise.
    pub auto_start: bool,
    pub role: Role,
    pub port: u16,
    pub udp_port: u16,
    pub mapping: Mapping,
    /// Host: the player the collision snapshot is centred on; "" means the first player online.
    pub collision_player: String,
    pub collision_radius: u32,
    pub collision_interval_ms: u64,
    pub collision_move_blocks: f64,
    /// Host: show the guest's entities in the world as block displays.
    pub show_guests: bool,
    /// Host: the block a guest entity is shown as.
    pub guest_block: String,
    /// Guest: the entity id of the first player; the next players take the next ids.
    pub guest_entity: u32,
    pub guest_provider: String,
    pub guest_asset: String,
    pub heartbeat_ms: u32,
    pub timeout_ms: u32,
    /// The link token of contract revision 6.
    pub token_auth: bool,
    /// token_auth: the name of the endpoint file in the plugin's data folder; "" means `endpoint.json`.
    /// A plugin sees no other folder (CONTRACT 6.2 leaves the place to the application).
    pub endpoint_file: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            auto_start: false,
            role: Role::Host,
            port: DEFAULT_CONTROL_PORT,
            udp_port: 0,
            mapping: Mapping::default(),
            collision_player: String::new(),
            collision_radius: 16,
            collision_interval_ms: 500,
            collision_move_blocks: 1.0,
            show_guests: true,
            guest_block: "minecraft:magenta_glazed_terracotta".into(),
            guest_entity: 1,
            guest_provider: "Minecraft".into(),
            guest_asset: "minecraft:player".into(),
            heartbeat_ms: 250,
            timeout_ms: 2000,
            token_auth: false,
            endpoint_file: String::new(),
        }
    }
}

fn get<'a>(o: &'a Map<String, Value>, k: &str) -> Option<&'a Value> {
    o.get(k).filter(|v| !v.is_null())
}

fn bool_of(o: &Map<String, Value>, k: &str, d: bool) -> Result<bool, String> {
    match get(o, k) {
        None => Ok(d),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("\"{k}\" must be true or false")),
    }
}

fn str_of(o: &Map<String, Value>, k: &str, d: &str) -> Result<String, String> {
    match get(o, k) {
        None => Ok(d.to_owned()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("\"{k}\" must be a string")),
    }
}

/// A file name inside the data folder: the sandbox shows the plugin that folder and nothing else.
fn file_name_of(o: &Map<String, Value>, k: &str) -> Result<String, String> {
    let s = str_of(o, k, "")?;
    if s.contains(['/', '\\', '\0']) || s == "." || s == ".." {
        return Err(format!(
            "\"{k}\" must be a file name inside the plugin's data folder, not a path"
        ));
    }
    Ok(s)
}

fn num_of(o: &Map<String, Value>, k: &str, d: f64) -> Result<f64, String> {
    match get(o, k) {
        None => Ok(d),
        Some(Value::Number(n)) => n
            .as_f64()
            .filter(|x| x.is_finite())
            .ok_or_else(|| format!("\"{k}\" is not a finite number")),
        Some(_) => Err(format!("\"{k}\" must be a number")),
    }
}

fn int_of(o: &Map<String, Value>, k: &str, d: u64, lo: u64, hi: u64) -> Result<u64, String> {
    match get(o, k) {
        None => Ok(d),
        Some(Value::Number(n)) => n
            .as_u64()
            .filter(|x| (lo..=hi).contains(x))
            .ok_or_else(|| format!("\"{k}\" must be a whole number from {lo} to {hi}")),
        Some(_) => Err(format!("\"{k}\" must be a whole number")),
    }
}

impl Config {
    /// Parses the file's text. Errors name the key and never quote the file.
    pub fn parse(text: &str) -> Result<Self, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
        let o = v.as_object().ok_or("the file must hold one JSON object")?;
        let d = Self::default();
        let role = match str_of(o, "role", "host")?.as_str() {
            "host" => Role::Host,
            "guest" => Role::Guest,
            _ => return Err("\"role\" must be \"host\" or \"guest\"".into()),
        };
        let offset = match get(o, "offset") {
            None => [0.0; 3],
            Some(Value::Array(a)) if a.len() == 3 => {
                let mut out = [0.0; 3];
                for (i, x) in a.iter().enumerate() {
                    out[i] = x
                        .as_f64()
                        .filter(|x| x.is_finite())
                        .ok_or("\"offset\" must be three finite numbers")?;
                }
                out
            }
            Some(_) => return Err("\"offset\" must be three finite numbers".into()),
        };
        let scale = num_of(o, "scale", 1.0)?;
        if scale <= 0.0 {
            return Err("\"scale\" must be above 0".into());
        }
        let lo = u64::from(*HEARTBEAT_MS_RANGE.start());
        let hi = u64::from(*HEARTBEAT_MS_RANGE.end());
        Ok(Self {
            auto_start: bool_of(o, "auto_start", d.auto_start)?,
            role,
            port: int_of(o, "port", u64::from(d.port), 0, 65535)? as u16,
            udp_port: int_of(o, "udp_port", 0, 0, 65535)? as u16,
            mapping: Mapping {
                flip_x: bool_of(o, "flip_x", false)?,
                flip_z: bool_of(o, "flip_z", false)?,
                scale,
                offset,
            },
            collision_player: str_of(o, "collision_player", &d.collision_player)?,
            collision_radius: int_of(o, "collision_radius", 16, 1, 64)? as u32,
            collision_interval_ms: int_of(o, "collision_interval_ms", 500, 0, 3_600_000)?,
            collision_move_blocks: num_of(o, "collision_move_blocks", d.collision_move_blocks)?
                .max(0.0),
            show_guests: bool_of(o, "show_guests", d.show_guests)?,
            guest_block: str_of(o, "guest_block", &d.guest_block)?,
            guest_entity: int_of(o, "guest_entity", 1, 0, u64::from(u32::MAX))? as u32,
            guest_provider: str_of(o, "guest_provider", &d.guest_provider)?,
            guest_asset: str_of(o, "guest_asset", &d.guest_asset)?,
            heartbeat_ms: int_of(o, "heartbeat_ms", 250, lo, hi)? as u32,
            timeout_ms: int_of(o, "timeout_ms", 2000, 0, u64::from(u32::MAX))? as u32,
            token_auth: bool_of(o, "token_auth", false)?,
            endpoint_file: file_name_of(o, "endpoint_file")?,
        })
    }

    /// The text written on first start: every key with its default.
    pub fn default_text() -> String {
        let d = Self::default();
        let v = serde_json::json!({
            "auto_start": d.auto_start,
            "role": "host",
            "port": d.port,
            "udp_port": d.udp_port,
            "flip_x": false,
            "flip_z": false,
            "scale": 1.0,
            "offset": [0, 0, 0],
            "collision_player": d.collision_player,
            "collision_radius": d.collision_radius,
            "collision_interval_ms": d.collision_interval_ms,
            "collision_move_blocks": d.collision_move_blocks,
            "show_guests": d.show_guests,
            "guest_block": d.guest_block,
            "guest_entity": d.guest_entity,
            "guest_provider": d.guest_provider,
            "guest_asset": d.guest_asset,
            "heartbeat_ms": d.heartbeat_ms,
            "timeout_ms": d.timeout_ms,
            "token_auth": d.token_auth,
            "endpoint_file": d.endpoint_file,
        });
        let mut s = serde_json::to_string_pretty(&v).unwrap_or_default();
        s.push('\n');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_object_gives_the_defaults() {
        assert_eq!(Config::parse("{}").unwrap(), Config::default());
    }

    #[test]
    fn the_written_default_file_parses_back_to_the_defaults() {
        assert_eq!(
            Config::parse(&Config::default_text()).unwrap(),
            Config::default()
        );
    }

    #[test]
    fn keys_are_read_and_unknown_keys_ignored() {
        let c = Config::parse(
            r#"{"role":"guest","port":47811,"flip_x":true,"scale":2.5,"offset":[0,-4,0],
                "token_auth":true,"endpoint_file":"x.json","future_key":[1,2],"auto_start":true}"#,
        )
        .unwrap();
        assert_eq!(c.role, Role::Guest);
        assert_eq!(c.port, 47811);
        assert!(c.mapping.flip_x && !c.mapping.flip_z);
        assert_eq!(c.mapping.scale, 2.5);
        assert_eq!(c.mapping.offset, [0.0, -4.0, 0.0]);
        assert!(c.token_auth && c.auto_start);
        assert_eq!(c.endpoint_file, "x.json");
    }

    #[test]
    fn null_means_absent() {
        let c = Config::parse(r#"{"port":null,"role":null}"#).unwrap();
        assert_eq!(c, Config::default());
    }

    #[test]
    fn wrong_types_and_ranges_are_errors_naming_the_key() {
        for (text, key) in [
            (r#"{"port":"47800"}"#, "port"),
            (r#"{"port":70000}"#, "port"),
            (r#"{"port":-1}"#, "port"),
            (r#"{"role":"both"}"#, "role"),
            (r#"{"flip_x":1}"#, "flip_x"),
            (r#"{"scale":0}"#, "scale"),
            (r#"{"offset":[1,2]}"#, "offset"),
            (r#"{"collision_radius":65}"#, "collision_radius"),
            (r#"{"collision_radius":0}"#, "collision_radius"),
            (r#"{"heartbeat_ms":5}"#, "heartbeat_ms"),
            (r#"{"heartbeat_ms":10001}"#, "heartbeat_ms"),
            (r#"{"token_auth":"yes"}"#, "token_auth"),
            (r#"{"endpoint_file":"/run/x.json"}"#, "endpoint_file"),
            (r#"{"endpoint_file":"../x.json"}"#, "endpoint_file"),
            (r#"{"endpoint_file":".."}"#, "endpoint_file"),
        ] {
            let e = Config::parse(text).unwrap_err();
            assert!(e.contains(key), "{text}: {e}");
        }
        assert!(Config::parse("[]").is_err());
        assert!(Config::parse("{").is_err());
    }
}
