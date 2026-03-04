// Core abstractions
pub use mahalo_core::conn::{AssignKey, Conn};
pub use mahalo_core::controller::Controller;
pub use mahalo_core::pipeline::Pipeline;
pub use mahalo_core::plug::{plug_fn, BoxFuture, Plug, PlugFn};

// Router
pub use mahalo_router::MahaloRouter;

// Endpoint (Axum bridge)
pub use mahalo_endpoint::MahaloEndpoint;

// PubSub
pub use mahalo_pubsub::{PubSub, PubSubMessage};

// Channels
pub use mahalo_channel::{handle_websocket, Channel, ChannelError, ChannelRouter, ChannelSocket, PhoenixMessage, Reply};

// Telemetry
pub use mahalo_telemetry::{
    event_name, Telemetry, TelemetryEvent, CHANNEL_HANDLE_IN, CHANNEL_JOIN, ENDPOINT_START,
    ENDPOINT_STOP, ROUTER_DISPATCH,
};
