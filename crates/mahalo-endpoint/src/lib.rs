pub mod application;
pub mod endpoint;
pub mod handler;
pub mod http_parse;
pub mod ws_parse;

#[cfg(target_os = "linux")]
pub mod uring;
#[cfg(target_os = "linux")]
pub mod worker;

#[cfg(not(target_os = "linux"))]
pub mod tcp_server;

pub use application::{MahaloApplication, MahaloApplicationBuilder};
pub use endpoint::{json_error_handler, text_error_handler, ErrorHandler, MahaloEndpoint};
