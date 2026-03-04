pub fn workspace_cargo_toml(name: &str) -> String {
    format!(
        r#"[workspace]
resolver = "2"
members = [
    "crates/{name}",
    "crates/{name}_models",
    "crates/{name}_web",
]

[workspace.package]
edition = "2024"

[workspace.dependencies]
mahalo = {{ git = "https://github.com/your-org/mahalo.git" }}
rebar-core = {{ git = "https://github.com/your-org/rebar.git" }}
tokio = {{ version = "1", features = ["full"] }}
tracing = "0.1"
tracing-subscriber = "0.3"
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
async-trait = "0.1"
http = "1"
"#
    )
}

pub fn app_cargo_toml(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition.workspace = true

[dependencies]
{name}_web = {{ path = "../{name}_web" }}
mahalo.workspace = true
rebar-core.workspace = true
tokio.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
"#
    )
}

pub fn app_main_rs(name: &str) -> String {
    let web_crate = name.replace('-', "_");
    format!(
        r#"use std::net::SocketAddr;
use std::sync::Arc;

use mahalo::MahaloEndpoint;
use rebar_core::Runtime;
use {web_crate}::router::build_router;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {{
    tracing_subscriber::fmt::init();

    let router = build_router();
    let addr: SocketAddr = "127.0.0.1:4000".parse()?;
    let runtime = Runtime::new();

    tracing::info!("Starting {name} on {{addr}}");

    let endpoint = MahaloEndpoint::new(router, addr, Arc::new(runtime));
    endpoint.start().await?;

    Ok(())
}}
"#
    )
}

pub fn models_cargo_toml(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}_models"
version = "0.1.0"
edition.workspace = true

[dependencies]
serde.workspace = true
serde_json.workspace = true
"#
    )
}

pub fn models_lib_rs() -> String {
    "// Models go here\n".to_string()
}

pub fn web_cargo_toml(name: &str) -> String {
    format!(
        r#"[package]
name = "{name}_web"
version = "0.1.0"
edition.workspace = true

[dependencies]
{name}_models = {{ path = "../{name}_models" }}
mahalo.workspace = true
async-trait.workspace = true
serde_json.workspace = true
tracing.workspace = true
http = "1"
"#
    )
}

pub fn web_lib_rs() -> String {
    r#"pub mod channels;
pub mod controllers;
pub mod router;
"#
    .to_string()
}

pub fn web_router_rs(name: &str) -> String {
    let _ = name;
    r#"use std::sync::Arc;

use mahalo::{Conn, MahaloRouter, plug_fn};
use http::StatusCode;
// mahalo:imports

pub fn build_router() -> MahaloRouter {
    MahaloRouter::new()
        .get("/health", plug_fn(health))
        // mahalo:routes
}

async fn health(conn: Conn) -> Conn {
    conn.put_status(StatusCode::OK)
        .put_resp_body("ok")
}
"#
    .to_string()
}

pub fn controllers_mod_rs() -> String {
    "// mahalo:modules\n".to_string()
}

pub fn channels_mod_rs() -> String {
    "// mahalo:modules\n".to_string()
}
