// Core abstractions
pub use mahalo_core::conn::{AssignKey, Conn};
pub use mahalo_core::controller::Controller;
pub use mahalo_core::pipeline::Pipeline;
pub use mahalo_core::plug::{plug_fn, BoxFuture, Plug, PlugFn};

// Router
pub use mahalo_router::MahaloRouter;

// Endpoint (Axum bridge)
pub use mahalo_endpoint::{json_error_handler, text_error_handler, ErrorHandler, MahaloEndpoint};

// PubSub
pub use mahalo_pubsub::{PubSub, PubSubMessage};

// Channels
pub use mahalo_channel::{handle_websocket, Channel, ChannelError, ChannelRouter, ChannelSocket, PhoenixMessage, Reply};

// Plugs
pub use mahalo_plug::{
    CsrfProtection, CsrfToken, ETag, RequestId, RequestIdPlug, RequestStartTime, SecureHeaders,
    StaticFiles, request_logger,
};

// Telemetry
pub use mahalo_telemetry::{
    event_name, Telemetry, TelemetryEvent, CHANNEL_HANDLE_IN, CHANNEL_JOIN, ENDPOINT_START,
    ENDPOINT_STOP, ROUTER_DISPATCH,
};
