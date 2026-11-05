//! The glue to Pumpkin's plugin API (`pumpkin-plugin-api` 0.2, WebAssembly only). Everything of
//! substance is in `bridge`, `collision`, `mapping` and `config`, which are tested natively;
//! this file only reads the server's state into their types once per tick and shows the guest's
//! entities. It cannot be run without a Pumpkin server, so it is kept small.

use crate::bridge::{Bridge, GuestSink, PlayerSample, Tick, Worlds};
use crate::collision::{BlockSource, Cell};
use crate::config::{Config, Role};
use crate::displays::{DisplayWorld, Displays, Found, KEY, NAMESPACE, Tag};
use pumpkin_plugin_api::command::{
    Command, CommandError, CommandNode, CommandSender, ConsumedArgs,
};
use pumpkin_plugin_api::commands::CommandHandler;
use pumpkin_plugin_api::common::BlockPos;
use pumpkin_plugin_api::permission::{Permission, PermissionDefault, PermissionLevel};
use pumpkin_plugin_api::permissions as perm;
use pumpkin_plugin_api::persistent_data::PersistentDataHolder;
use pumpkin_plugin_api::scheduler::SchedulerExt;
use pumpkin_plugin_api::text::TextComponent;
use pumpkin_plugin_api::{
    BlockDisplayEntity, Context, Entity, EntityType, Plugin, PluginMetadata, Server, World,
    register_plugin, world,
};
use rewrite_bridge::link::Spawn;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Mutex;

/// The permission node of `/link`; operators of level 3 have it by default.
const COMMAND_PERMISSION: &str = "pumpkin-link:command.link";
const CONFIG_FILE: &str = "pumpkin-link.json";
const ENDPOINT_FILE: &str = "endpoint.json";

/// Shared by the block sources of every world in a tick.
type CellCache = Rc<RefCell<HashMap<u16, Rc<Cell>>>>;

struct State {
    bridge: Bridge,
    config_error: Option<String>,
    /// Block state id to its cell, for states that are air, have no shape, or are a full cube.
    cells: CellCache,
    guests: DisplaySink,
    task: Option<u32>,
}

// SAFETY-free note: `Rc` stays inside this state, which is only reachable through the mutex below, and a
// WebAssembly component has one thread.
unsafe impl Send for State {}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn say(sender: &CommandSender, text: &str) {
    sender.send_message(TextComponent::text(text));
}

fn fail(text: &str) -> CommandError {
    CommandError::CommandFailed(TextComponent::text(text))
}

// ---------------------------------------------------------------------------------------------
// Reading the server

fn uuid_text(id: &pumpkin_plugin_api::uuid::Uuid) -> String {
    format!("{:016x}{:016x}", id.high, id.low)
}

fn samples(server: &Server) -> Vec<PlayerSample> {
    server
        .get_all_players()
        .iter()
        .map(|p| {
            let e = p.as_entity();
            let (x, y, z) = p.get_position();
            PlayerSample {
                key: uuid_text(&p.get_id()),
                name: p.get_name(),
                world: e.get_world().get_id(),
                pos: [x, y, z],
                yaw: p.get_yaw(),
                pitch: p.get_pitch(),
            }
        })
        .collect()
}

/// The blocks of the server's worlds, read through the plugin API.
struct WorldBlocks<'a> {
    server: &'a Server,
    cells: CellCache,
    worlds: HashMap<String, WorldSource>,
}

struct WorldSource {
    world: World,
    loaded: HashMap<(i32, i32), bool>,
    cells: CellCache,
}

impl BlockSource for WorldSource {
    fn cell(&mut self, x: i32, y: i32, z: i32) -> Option<Rc<Cell>> {
        let chunk = (x >> 4, z >> 4);
        let world = &self.world;
        let loaded = *self
            .loaded
            .entry(chunk)
            .or_insert_with(|| world.get_chunk(chunk.0, chunk.1).is_some());
        if !loaded {
            return None;
        }
        let pos = BlockPos { x, y, z };
        let id = world.get_block_state_id(pos);
        if let Some(c) = self.cells.borrow().get(&id) {
            return Some(c.clone());
        }
        let state = world.get_block_state(pos);
        let boxes: Vec<[f64; 6]> = state
            .collision_shapes
            .iter()
            .map(|b| [b.min.0, b.min.1, b.min.2, b.max.0, b.max.1, b.max.2])
            .collect();
        let cell = Rc::new(Cell {
            full: state.is_full_cube && boxes.len() == 1,
            boxes,
        });
        // The shapes of a state can be shifted per position for a few blocks (the server's
        // `shape_offset_delta`), so only states with no shape or a plain cube are remembered.
        if cell.boxes.is_empty() || cell.full {
            self.cells.borrow_mut().insert(id, cell.clone());
        }
        Some(cell)
    }
}

impl Worlds for WorldBlocks<'_> {
    fn blocks(&mut self, world: &str) -> Option<&mut dyn BlockSource> {
        if !self.worlds.contains_key(world) {
            let w = self
                .server
                .get_all_worlds()
                .into_iter()
                .find(|w| w.get_id() == world)?;
            self.worlds.insert(
                world.to_owned(),
                WorldSource {
                    world: w,
                    loaded: HashMap::new(),
                    cells: self.cells.clone(),
                },
            );
        }
        self.worlds
            .get_mut(world)
            .map(|s| s as &mut dyn BlockSource)
    }
}

// ---------------------------------------------------------------------------------------------
// The guest's entities, shown as block displays (as minecraft-link does on the client)

/// The server's half of `displays::Displays`: spawn tagged block displays, move them, list the tagged ones.
#[derive(Default)]
struct PumpkinDisplays {
    /// The world the guest's entities are shown in: the server's first world, set on the first tick.
    world: Option<World>,
    /// Every world, refreshed before each sweep.
    worlds: Vec<World>,
    block_state: Option<u16>,
}

impl DisplayWorld for PumpkinDisplays {
    type Handle = Entity;

    fn create(&mut self, tag: &Tag) -> Option<(Entity, u32)> {
        let (Some(world), Some(state)) = (&self.world, self.block_state) else {
            tracing::warn!(
                "pumpkin-link: guest entity {} not shown (no world or no block)",
                tag.guest
            );
            return None;
        };
        let e = world.spawn_entity(EntityType::BlockDisplay, (0.0, 0.0, 0.0));
        // Saved with the entity: the sweep finds it again after a chunk reload or a restart.
        e.set_string(NAMESPACE, KEY, &tag.encode());
        if let Some(b) = BlockDisplayEntity::from_entity(&e) {
            b.set_block_state_id(state);
            if let Some(d) = pumpkin_plugin_api::DisplayEntity::from_entity(&e) {
                // centre the block on the entity's origin
                d.set_transformation(
                    pumpkin_plugin_api::TransformationBuilder::new()
                        .translation(-0.5, -0.5, -0.5)
                        .build(),
                );
            }
        }
        let id = e.get_id();
        tracing::info!(
            "pumpkin-link: guest entity {} shown as block display (server entity id {id})",
            tag.guest
        );
        Some((e, id))
    }

    fn place(&mut self, e: &Entity, pos: [f64; 3], yaw: f32, pitch: f32) {
        e.teleport((pos[0], pos[1], pos[2]), e.get_world());
        e.set_rotation(yaw, pitch);
    }

    fn remove(&mut self, e: &Entity) {
        tracing::info!(
            "pumpkin-link: guest entity removed (server entity id {})",
            e.get_id()
        );
        e.remove();
    }

    fn scan(&mut self) -> Vec<Found<Entity>> {
        let mut out = Vec::new();
        for w in &self.worlds {
            for e in w.get_entities() {
                if let Some(tag) = e.get_string(NAMESPACE, KEY).and_then(|t| Tag::parse(&t)) {
                    out.push(Found {
                        server_id: e.get_id(),
                        handle: e,
                        tag,
                    });
                }
            }
        }
        out
    }
}

struct DisplaySink {
    inner: Displays<PumpkinDisplays>,
    ticks: u32,
}

impl DisplaySink {
    fn new(block_state: Option<u16>) -> Self {
        let world = PumpkinDisplays {
            block_state,
            ..PumpkinDisplays::default()
        };
        Self {
            inner: Displays::new(world, format!("{:x}", rewrite_bridge::link::now_us())),
            ticks: 0,
        }
    }

    /// Once a second while a link runs, every two seconds otherwise: remove what the world holds of ours
    /// that should not be there, and re-bind a reloaded display (see `displays`).
    fn tick(&mut self, server: &Server, running: bool) {
        // Every tick until it exists, before the bridge's tick: with `auto_start` a guest can spawn within the
        // first ticks of the server, and a spawn that finds no world is lost until the sweep retries it.
        let w = self.inner.world();
        if w.world.is_none() {
            w.world = server.get_all_worlds().into_iter().next();
        }
        self.ticks += 1;
        if self.ticks < if running { 20 } else { 40 } {
            return;
        }
        self.ticks = 0;
        w.worlds = server.get_all_worlds();
        self.inner.sweep();
    }
}

impl GuestSink for DisplaySink {
    fn spawn(&mut self, entity: u32, _spawn: &Spawn) {
        self.inner.spawn(entity);
    }

    fn pose(&mut self, entity: u32, pos: [f64; 3], yaw: f32, pitch: f32, _teleport: bool) {
        self.inner.pose(entity, pos, yaw, pitch);
    }

    fn despawn(&mut self, entity: u32) {
        self.inner.despawn(entity);
    }

    fn clear(&mut self) {
        let n = self.inner.len();
        if n > 0 {
            tracing::info!("pumpkin-link: {n} guest entities cleared");
        }
        self.inner.clear();
    }
}

fn resolve_block(name: &str) -> Option<u16> {
    world::get_block_by_name(name).map(|b| b.default_state_id)
}

// ---------------------------------------------------------------------------------------------
// The tick and the commands

fn on_tick(server: Server) {
    // A callback can be re-entered while the host runs one of our calls; never wait for the lock.
    let Ok(mut guard) = STATE.try_lock() else {
        return;
    };
    let Some(st) = guard.as_mut() else {
        return;
    };
    let running = st.bridge.is_running();
    st.guests.tick(&server, running);
    if !running {
        return;
    }
    let players = samples(&server);
    let State {
        bridge,
        cells,
        guests,
        ..
    } = st;
    let mut worlds = WorldBlocks {
        server: &server,
        cells: cells.clone(),
        worlds: HashMap::new(),
    };
    bridge.tick(Tick {
        now: std::time::Instant::now(),
        players: &players,
        worlds: &mut worlds,
        guests,
    });
}

fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Result<R, CommandError> {
    let mut guard = STATE
        .try_lock()
        .map_err(|_| fail("the link is busy, try again"))?;
    let st = guard
        .as_mut()
        .ok_or_else(|| fail("the plugin is not loaded"))?;
    Ok(f(st))
}

struct Status;
struct Start(Role);
struct Stop;
struct Focus(rewrite_bridge::link::Role);

impl CommandHandler for Status {
    fn handle(
        &self,
        sender: CommandSender,
        _s: Server,
        _a: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let lines = with_state(|st| {
            let mut l = Vec::new();
            if let Some(e) = &st.config_error {
                l.push(format!("config: {e} (defaults in use)"));
            }
            l.extend(st.bridge.status());
            l
        })?;
        for l in lines {
            say(&sender, &l);
        }
        Ok(1)
    }
}

impl CommandHandler for Start {
    fn handle(
        &self,
        sender: CommandSender,
        server: Server,
        _a: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let role = self.0;
        with_state(|st| {
            let w = st.guests.inner.world();
            if w.world.is_none() {
                w.world = server.get_all_worlds().into_iter().next();
            }
            st.bridge.start(role)
        })?
        .map_err(|e| fail(&e))?;
        say(
            &sender,
            &format!(
                "link started as {}",
                if role == Role::Host { "host" } else { "guest" }
            ),
        );
        Ok(1)
    }
}

impl CommandHandler for Stop {
    fn handle(
        &self,
        sender: CommandSender,
        _s: Server,
        _a: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        with_state(|st| {
            let State { bridge, guests, .. } = st;
            bridge.stop(guests);
        })?;
        say(&sender, "link stopped");
        Ok(1)
    }
}

impl CommandHandler for Focus {
    fn handle(
        &self,
        sender: CommandSender,
        _s: Server,
        _a: ConsumedArgs,
    ) -> Result<i32, CommandError> {
        let to = self.0;
        with_state(|st| st.bridge.set_focus(to))?.map_err(|e| fail(&e))?;
        say(&sender, "input focus moved");
        Ok(1)
    }
}

pub struct PumpkinLink;

impl Plugin for PumpkinLink {
    fn new() -> Self {
        PumpkinLink
    }

    fn metadata(&self) -> PluginMetadata {
        PluginMetadata {
            name: "pumpkin-link".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            authors: vec!["patodois".into()],
            description: "Speaks the rewrite-bridge passthrough link protocol on 127.0.0.1.".into(),
            dependencies: vec![],
            // Why each one: README, "Permissions".
            permissions: vec![
                perm::NETWORK_TCP.into(),
                perm::NETWORK_UDP.into(),
                perm::NETWORK_LOOPBACK.into(),
                perm::FS_WRITE_DATA.into(),
            ],
        }
    }

    fn on_load(&self, context: Context) -> Result<(), String> {
        let dir = context.get_data_folder();
        let cfg_path = format!("{dir}/{CONFIG_FILE}");
        let (cfg, config_error) = match std::fs::read_to_string(&cfg_path) {
            Ok(text) => match Config::parse(&text) {
                Ok(c) => (c, None),
                Err(e) => (Config::default(), Some(format!("{CONFIG_FILE}: {e}"))),
            },
            Err(_) => {
                if let Err(e) = std::fs::write(&cfg_path, Config::default_text()) {
                    tracing::warn!("pumpkin-link: cannot write {CONFIG_FILE}: {e}");
                }
                (Config::default(), None)
            }
        };
        if let Some(e) = &config_error {
            tracing::error!("pumpkin-link: {e}; using the defaults and not starting the link");
        }
        let endpoint_name = if cfg.endpoint_file.is_empty() {
            ENDPOINT_FILE
        } else {
            cfg.endpoint_file.as_str()
        };
        let endpoint = std::path::PathBuf::from(format!("{dir}/{endpoint_name}"));
        let auto = cfg.auto_start && config_error.is_none();
        let role = cfg.role;
        let block_state = resolve_block(&cfg.guest_block);
        let guests = DisplaySink::new(block_state);
        if cfg.show_guests && block_state.is_none() {
            tracing::warn!(
                "pumpkin-link: guest_block names no block of this server; guests are not shown"
            );
        }
        let mut bridge = Bridge::new(cfg, endpoint);
        if auto {
            match bridge.start(role) {
                Ok(()) => tracing::info!("pumpkin-link: started as {role:?}"),
                Err(e) => tracing::error!("pumpkin-link: {e}"),
            }
        }
        let task = context.schedule_repeating_task(1, 1, on_tick);
        *STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(State {
            bridge,
            config_error,
            cells: CellCache::default(),
            guests,
            task: Some(task),
        });

        let _ = context.register_permission(&Permission {
            node: COMMAND_PERMISSION.into(),
            description: "Use /link".into(),
            default: PermissionDefault::Op(PermissionLevel::Three),
            children: vec![],
        });
        let command = Command::new(
            &["link".to_string()],
            "The rewrite-bridge link of this server",
        )
        .then(CommandNode::literal("status").execute(Status))
        .then(CommandNode::literal("host").execute(Start(Role::Host)))
        .then(CommandNode::literal("guest").execute(Start(Role::Guest)))
        .then(CommandNode::literal("stop").execute(Stop))
        .then(
            CommandNode::literal("focus")
                .then(CommandNode::literal("host").execute(Focus(rewrite_bridge::link::Role::Host)))
                .then(
                    CommandNode::literal("guest").execute(Focus(rewrite_bridge::link::Role::Guest)),
                ),
        )
        .execute(Status);
        context.register_command(command, COMMAND_PERMISSION);
        Ok(())
    }

    fn on_unload(&self, _context: Context) -> Result<(), String> {
        let taken = STATE.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut st) = taken {
            if let Some(t) = st.task.take() {
                pumpkin_plugin_api::scheduler::cancel_task(t);
            }
            let State { bridge, guests, .. } = &mut st;
            bridge.stop(guests);
        }
        Ok(())
    }
}

register_plugin!(PumpkinLink);
