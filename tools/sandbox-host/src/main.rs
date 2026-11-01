//! Runs a `wasm32-wasip2` command component under wasmtime-wasi with the rules Pumpkin applies to a
//! plugin that asks for `network.tcp`, `network.udp` and `network.loopback` and `fs.write.data`
//! (`crates/pumpkin-wasm-host/src/runtime.rs`, `socket_policy_for_permissions`): TCP and UDP are
//! allowed, but only to and from loopback addresses, and the only folder the component sees is the
//! one given as `--data`, preopened as `data` with read and write access. Nothing else is inherited:
//! no environment, no other folders.
//!
//! This is a stand-in for the server, which cannot be built where the plugin is developed (its build
//! needs the game's data files). It runs a component that links the plugin's own code, not the
//! plugin component itself; see the README, "How it is tested".
//!
//! usage: sandbox-host --data DIR COMPONENT.wasm [args for the component...]
//!        sandbox-host --inspect PLUGIN.wasm
//!
//! `--inspect` loads a Pumpkin plugin component with every import that is not WASI replaced by a trap
//! and calls its `get-metadata` export, printing the name, version and permissions it declares. It
//! shows that the component links and what it asks of the server; it does not run the plugin.

use std::net::IpAddr;
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store};
use wasmtime_wasi::p2::bindings::sync::Command;
use wasmtime_wasi::sockets::SocketAddrUse;
use wasmtime_wasi::{FsPerms, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

struct State {
    ctx: WasiCtx,
    table: ResourceTable,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.ctx,
            table: &mut self.table,
        }
    }
}

fn inspect(path: &str) -> wasmtime::Result<()> {
    use wasmtime::component::Val;
    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, path)?;
    let mut linker = Linker::<State>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;
    linker.define_unknown_imports_as_traps(&component)?;
    let mut store = Store::new(
        &engine,
        State {
            ctx: WasiCtxBuilder::new().build(),
            table: ResourceTable::new(),
        },
    );
    let instance = linker.instantiate(&mut store, &component)?;
    // the plugin registers itself in `init-plugin`, which the server calls first
    let init = instance
        .get_func(&mut store, "init-plugin")
        .ok_or_else(|| wasmtime::format_err!("no init-plugin export"))?;
    init.call(&mut store, &[], &mut [])?;
    let (_, iface) = instance
        .get_export(&mut store, None, "pumpkin:plugin/metadata@0.1.0")
        .ok_or_else(|| wasmtime::format_err!("no pumpkin:plugin/metadata@0.1.0 export"))?;
    let (_, func_idx) = instance
        .get_export(&mut store, Some(&iface), "get-metadata")
        .ok_or_else(|| wasmtime::format_err!("no get-metadata"))?;
    let func = instance
        .get_func(&mut store, func_idx)
        .ok_or_else(|| wasmtime::format_err!("get-metadata is not a function"))?;
    let mut out = [Val::Bool(false)];
    func.call(&mut store, &[], &mut out)?;
    println!("{out:?}");
    Ok(())
}

fn main() -> wasmtime::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--inspect") {
        let Some(path) = std::env::args().nth(2) else {
            eprintln!("usage: sandbox-host --inspect PLUGIN.wasm");
            std::process::exit(2);
        };
        return inspect(&path);
    }
    let mut args = std::env::args().skip(1);
    let mut data = None;
    let mut component_path = None;
    let mut rest = Vec::new();
    while let Some(a) = args.next() {
        if component_path.is_some() {
            rest.push(a);
        } else if a == "--data" {
            data = args.next();
        } else {
            component_path = Some(a);
        }
    }
    let (Some(data), Some(component_path)) = (data, component_path) else {
        eprintln!("usage: sandbox-host --data DIR COMPONENT.wasm [args...]");
        std::process::exit(2);
    };

    let mut config = Config::new();
    config.wasm_component_model(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, &component_path)?;
    let mut linker = Linker::<State>::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    let mut builder = WasiCtxBuilder::new();
    builder.inherit_stdout().inherit_stderr();
    builder.args(
        &std::iter::once("sandbox".to_owned())
            .chain(rest)
            .collect::<Vec<_>>(),
    );
    builder.allow_tcp(true);
    builder.allow_udp(true);
    builder.socket_addr_check(|addr, reason| {
        Box::pin(async move { allowed(addr.ip(), addr.port(), reason) })
    });
    builder.preopened_dir(&data, "data", FsPerms::ReadWrite)?;
    let mut store = Store::new(
        &engine,
        State {
            ctx: builder.build(),
            table: ResourceTable::new(),
        },
    );
    let command = Command::instantiate(&mut store, &component, &linker)?;
    match command.wasi_cli_run().call_run(&mut store)? {
        Ok(()) => Ok(()),
        Err(()) => std::process::exit(1),
    }
}

/// Loopback only, as with `network.loopback`. Wasmtime binds the wildcard address with port 0 before an
/// outbound connect or send; Pumpkin lets that one precursor through (the later check on the destination
/// still applies), and so does this.
fn allowed(ip: IpAddr, port: u16, reason: SocketAddrUse) -> bool {
    let implicit_outbound_bind = ip.is_unspecified()
        && port == 0
        && matches!(reason, SocketAddrUse::TcpBind | SocketAddrUse::UdpBind);
    ip.is_loopback() || implicit_outbound_bind
}
