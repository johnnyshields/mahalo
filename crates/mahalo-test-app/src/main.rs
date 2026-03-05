//! # Ice Cream Store - Mahalo Demo Application
//!
//! Demonstrates all Mahalo framework features using an ice cream store theme:
//! - Pipelines & Plugs (API, browser, auth)
//! - Multiple Scopes (/api, /)
//! - Controllers (FlavorController, OrderController) via resources()
//! - Typed Assigns (RequestId, CurrentUser)
//! - WebSocket Channels (OrderChannel, StoreChannel)
//! - PubSub for broadcasting
//! - Telemetry with custom events and spans
//! - Pipeline halting (auth plug)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::StatusCode;
use serde::Serialize;

use tera::{Tera, Context};

use mahalo::{
    AssignKey, Channel, ChannelError, ChannelRouter, ChannelSocket, Conn, Controller,
    MahaloEndpoint, MahaloRouter, Pipeline, PubSub, Reply, StaticFiles, Telemetry,
    plug_fn, BoxFuture,
};

// ---------------------------------------------------------------------------
// Data Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct Flavor {
    id: u64,
    name: String,
    description: String,
    price_cents: u64,
    in_stock: bool,
}

#[derive(Debug, Clone, Serialize)]
struct Order {
    id: u64,
    customer_name: String,
    items: Vec<OrderItem>,
    status: String,
    created_at: String,
}

#[derive(Debug, Clone, Serialize)]
struct OrderItem {
    flavor_id: u64,
    scoops: u32,
    topping: Option<String>,
}

// ---------------------------------------------------------------------------
// In-Memory Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Store {
    flavors: Arc<Mutex<Vec<Flavor>>>,
    orders: Arc<Mutex<Vec<Order>>>,
    telemetry: Telemetry,
}

impl Store {
    fn new(telemetry: Telemetry) -> Self {
        let flavors = vec![
            Flavor { id: 1, name: "Coconut Dream".into(), description: "Creamy coconut with toasted flakes".into(), price_cents: 450, in_stock: true },
            Flavor { id: 2, name: "Pineapple Paradise".into(), description: "Tangy pineapple sorbet".into(), price_cents: 400, in_stock: true },
            Flavor { id: 3, name: "Passion Fruit Swirl".into(), description: "Tart passion fruit with vanilla swirl".into(), price_cents: 500, in_stock: true },
            Flavor { id: 4, name: "Guava Sunset".into(), description: "Sweet guava with a hint of lime".into(), price_cents: 475, in_stock: true },
            Flavor { id: 5, name: "Mango Tango".into(), description: "Rich mango with chili flakes".into(), price_cents: 450, in_stock: true },
            Flavor { id: 6, name: "Lychee Blossom".into(), description: "Delicate lychee with rose water".into(), price_cents: 525, in_stock: true },
            Flavor { id: 7, name: "Papaya Cream".into(), description: "Smooth papaya with coconut cream".into(), price_cents: 450, in_stock: false },
        ];

        Store {
            flavors: Arc::new(Mutex::new(flavors)),
            orders: Arc::new(Mutex::new(Vec::new())),
            telemetry,
        }
    }
}

// ---------------------------------------------------------------------------
// Typed Assign Keys
// ---------------------------------------------------------------------------

struct RequestId;
impl AssignKey for RequestId {
    type Value = String;
}

struct CurrentUser;
impl AssignKey for CurrentUser {
    type Value = String;
}

// Channel socket assign key for customer name
struct CustomerName;
impl AssignKey for CustomerName {
    type Value = String;
}

// ---------------------------------------------------------------------------
// Flavor Controller
// ---------------------------------------------------------------------------

struct FlavorController {
    store: Store,
}

impl Controller for FlavorController {
    fn index(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let flavors = self.store.flavors.lock().unwrap();
            let json = serde_json::to_string(&*flavors).unwrap();
            conn.put_status(StatusCode::OK).put_resp_body(json)
        })
    }

    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id: u64 = conn
                .path_params
                .get("id")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let flavors = self.store.flavors.lock().unwrap();
            match flavors.iter().find(|f| f.id == id) {
                Some(flavor) => {
                    let json = serde_json::to_string(flavor).unwrap();
                    conn.put_status(StatusCode::OK).put_resp_body(json)
                }
                None => conn
                    .put_status(StatusCode::NOT_FOUND)
                    .put_resp_body(r#"{"error":"flavor not found"}"#),
            }
        })
    }

    fn create(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        let telemetry = self.store.telemetry.clone();
        Box::pin(async move {
            let body: serde_json::Value = match serde_json::from_slice(&conn.body) {
                Ok(v) => v,
                Err(_) => {
                    return conn
                        .put_status(StatusCode::BAD_REQUEST)
                        .put_resp_body(r#"{"error":"invalid JSON"}"#)
                }
            };

            let json = {
                let mut flavors = self.store.flavors.lock().unwrap();
                let next_id = flavors.iter().map(|f| f.id).max().unwrap_or(0) + 1;

                let flavor = Flavor {
                    id: next_id,
                    name: body["name"].as_str().unwrap_or("Unknown").to_string(),
                    description: body["description"].as_str().unwrap_or("").to_string(),
                    price_cents: body["price_cents"].as_u64().unwrap_or(0),
                    in_stock: body["in_stock"].as_bool().unwrap_or(true),
                };

                let json = serde_json::to_string(&flavor).unwrap();
                flavors.push(flavor);
                json
            };

            telemetry
                .execute(
                    &["ice_cream", "flavor", "created"],
                    HashMap::new(),
                    HashMap::new(),
                )
                .await;

            conn.put_status(StatusCode::CREATED).put_resp_body(json)
        })
    }

    fn update(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id: u64 = conn
                .path_params
                .get("id")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let body: serde_json::Value = match serde_json::from_slice(&conn.body) {
                Ok(v) => v,
                Err(_) => {
                    return conn
                        .put_status(StatusCode::BAD_REQUEST)
                        .put_resp_body(r#"{"error":"invalid JSON"}"#)
                }
            };

            let mut flavors = self.store.flavors.lock().unwrap();
            match flavors.iter_mut().find(|f| f.id == id) {
                Some(flavor) => {
                    if let Some(name) = body["name"].as_str() {
                        flavor.name = name.to_string();
                    }
                    if let Some(desc) = body["description"].as_str() {
                        flavor.description = desc.to_string();
                    }
                    if let Some(price) = body["price_cents"].as_u64() {
                        flavor.price_cents = price;
                    }
                    if let Some(stock) = body["in_stock"].as_bool() {
                        flavor.in_stock = stock;
                    }
                    let json = serde_json::to_string(flavor).unwrap();
                    conn.put_status(StatusCode::OK).put_resp_body(json)
                }
                None => conn
                    .put_status(StatusCode::NOT_FOUND)
                    .put_resp_body(r#"{"error":"flavor not found"}"#),
            }
        })
    }

    fn delete(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id: u64 = conn
                .path_params
                .get("id")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let mut flavors = self.store.flavors.lock().unwrap();
            let len_before = flavors.len();
            flavors.retain(|f| f.id != id);

            if flavors.len() < len_before {
                conn.put_status(StatusCode::OK)
                    .put_resp_body(r#"{"deleted":true}"#)
            } else {
                conn.put_status(StatusCode::NOT_FOUND)
                    .put_resp_body(r#"{"error":"flavor not found"}"#)
            }
        })
    }
}

// ---------------------------------------------------------------------------
// Order Controller
// ---------------------------------------------------------------------------

struct OrderController {
    store: Store,
    pubsub: PubSub,
}

impl Controller for OrderController {
    fn index(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let orders = self.store.orders.lock().unwrap();
            let json = serde_json::to_string(&*orders).unwrap();
            conn.put_status(StatusCode::OK).put_resp_body(json)
        })
    }

    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id: u64 = conn
                .path_params
                .get("id")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let orders = self.store.orders.lock().unwrap();
            match orders.iter().find(|o| o.id == id) {
                Some(order) => {
                    let json = serde_json::to_string(order).unwrap();
                    conn.put_status(StatusCode::OK).put_resp_body(json)
                }
                None => conn
                    .put_status(StatusCode::NOT_FOUND)
                    .put_resp_body(r#"{"error":"order not found"}"#),
            }
        })
    }

    fn create(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        let telemetry = self.store.telemetry.clone();
        let pubsub = self.pubsub.clone();
        Box::pin(async move {
            let body: serde_json::Value = match serde_json::from_slice(&conn.body) {
                Ok(v) => v,
                Err(_) => {
                    return conn
                        .put_status(StatusCode::BAD_REQUEST)
                        .put_resp_body(r#"{"error":"invalid JSON"}"#)
                }
            };

            let (json, order_id) = {
                let mut orders = self.store.orders.lock().unwrap();
                let next_id = orders.iter().map(|o| o.id).max().unwrap_or(0) + 1;

                let items: Vec<OrderItem> = body["items"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .map(|item| OrderItem {
                                flavor_id: item["flavor_id"].as_u64().unwrap_or(0),
                                scoops: item["scoops"].as_u64().unwrap_or(1) as u32,
                                topping: item["topping"].as_str().map(|s| s.to_string()),
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let order = Order {
                    id: next_id,
                    customer_name: body["customer_name"]
                        .as_str()
                        .unwrap_or("Guest")
                        .to_string(),
                    items,
                    status: "pending".to_string(),
                    created_at: chrono_now(),
                };

                let json = serde_json::to_string(&order).unwrap();
                let order_id = order.id;
                orders.push(order);
                (json, order_id)
            };

            // Emit telemetry event
            let mut meta = HashMap::new();
            meta.insert(
                "order_id".to_string(),
                serde_json::json!(order_id),
            );
            telemetry
                .execute(
                    &["ice_cream", "order", "placed"],
                    HashMap::new(),
                    meta,
                )
                .await;

            // Broadcast via PubSub
            pubsub.broadcast(
                &format!("order:{order_id}"),
                "order_created",
                serde_json::json!({"order_id": order_id}),
            );

            conn.put_status(StatusCode::CREATED).put_resp_body(json)
        })
    }

    fn update(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id: u64 = conn
                .path_params
                .get("id")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let body: serde_json::Value = match serde_json::from_slice(&conn.body) {
                Ok(v) => v,
                Err(_) => {
                    return conn
                        .put_status(StatusCode::BAD_REQUEST)
                        .put_resp_body(r#"{"error":"invalid JSON"}"#)
                }
            };

            let mut orders = self.store.orders.lock().unwrap();
            match orders.iter_mut().find(|o| o.id == id) {
                Some(order) => {
                    if let Some(status) = body["status"].as_str() {
                        order.status = status.to_string();
                    }
                    let json = serde_json::to_string(order).unwrap();
                    conn.put_status(StatusCode::OK).put_resp_body(json)
                }
                None => conn
                    .put_status(StatusCode::NOT_FOUND)
                    .put_resp_body(r#"{"error":"order not found"}"#),
            }
        })
    }

    fn delete(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id: u64 = conn
                .path_params
                .get("id")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);

            let mut orders = self.store.orders.lock().unwrap();
            let len_before = orders.len();
            orders.retain(|o| o.id != id);

            if orders.len() < len_before {
                conn.put_status(StatusCode::OK)
                    .put_resp_body(r#"{"deleted":true}"#)
            } else {
                conn.put_status(StatusCode::NOT_FOUND)
                    .put_resp_body(r#"{"error":"order not found"}"#)
            }
        })
    }
}

// ---------------------------------------------------------------------------
// WebSocket Channels
// ---------------------------------------------------------------------------

struct OrderChannel {
    store: Store,
}

#[async_trait]
impl Channel for OrderChannel {
    async fn join(
        &self,
        topic: &str,
        payload: &serde_json::Value,
        socket: &mut ChannelSocket,
    ) -> Result<serde_json::Value, ChannelError> {
        let customer = payload["customer_name"]
            .as_str()
            .unwrap_or("Guest")
            .to_string();

        tracing::info!(topic = %topic, customer = %customer, "Customer joined order channel");
        socket.assign::<CustomerName>(customer.clone());

        // Extract order ID from topic (e.g., "order:42")
        let order_id = parse_order_id(topic);

        let orders = self.store.orders.lock().unwrap();
        let status = orders
            .iter()
            .find(|o| o.id == order_id)
            .map(|o| o.status.clone())
            .unwrap_or_else(|| "not_found".to_string());

        Ok(serde_json::json!({
            "status": status,
            "customer": customer,
        }))
    }

    async fn handle_in(
        &self,
        event: &str,
        payload: &serde_json::Value,
        socket: &mut ChannelSocket,
    ) -> Result<Option<Reply>, ChannelError> {
        match event {
            "update_status" => {
                let order_id = parse_order_id(&socket.topic);

                let new_status = payload["status"]
                    .as_str()
                    .unwrap_or("unknown")
                    .to_string();

                let mut orders = self.store.orders.lock().unwrap();
                if let Some(order) = orders.iter_mut().find(|o| o.id == order_id) {
                    order.status = new_status.clone();
                }

                // Broadcast status change to all subscribers
                socket.broadcast(
                    "status_changed",
                    serde_json::json!({
                        "order_id": order_id,
                        "status": new_status,
                    }),
                );

                Ok(Some(Reply::ok(serde_json::json!({"updated": true}))))
            }
            "add_item" => {
                let order_id = parse_order_id(&socket.topic);

                let item = OrderItem {
                    flavor_id: payload["flavor_id"].as_u64().unwrap_or(0),
                    scoops: payload["scoops"].as_u64().unwrap_or(1) as u32,
                    topping: payload["topping"].as_str().map(|s| s.to_string()),
                };

                let mut orders = self.store.orders.lock().unwrap();
                if let Some(order) = orders.iter_mut().find(|o| o.id == order_id) {
                    order.items.push(item.clone());

                    socket.broadcast(
                        "item_added",
                        serde_json::json!({
                            "order_id": order_id,
                            "item": item,
                        }),
                    );

                    Ok(Some(Reply::ok(serde_json::json!({"added": true}))))
                } else {
                    Ok(Some(Reply::error(
                        serde_json::json!({"reason": "order not found"}),
                    )))
                }
            }
            _ => Ok(None),
        }
    }

    async fn terminate(&self, reason: &str, socket: &mut ChannelSocket) {
        let customer = socket
            .get_assign::<CustomerName>()
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string());
        tracing::info!(
            customer = %customer,
            reason = %reason,
            "Customer left order channel"
        );
    }
}

struct StoreChannel {
    store: Store,
}

#[async_trait]
impl Channel for StoreChannel {
    async fn join(
        &self,
        _topic: &str,
        payload: &serde_json::Value,
        socket: &mut ChannelSocket,
    ) -> Result<serde_json::Value, ChannelError> {
        let customer = payload["customer_name"]
            .as_str()
            .unwrap_or("Guest")
            .to_string();

        tracing::info!(customer = %customer, "Customer entered the store lobby");
        socket.assign::<CustomerName>(customer.clone());

        socket.broadcast(
            "customer_arrived",
            serde_json::json!({"customer": customer}),
        );

        Ok(serde_json::json!({
            "message": format!("Welcome to the Mahalo Ice Cream Store, {customer}!"),
        }))
    }

    async fn handle_in(
        &self,
        event: &str,
        payload: &serde_json::Value,
        socket: &mut ChannelSocket,
    ) -> Result<Option<Reply>, ChannelError> {
        match event {
            "announcement" => {
                let message = payload["message"]
                    .as_str()
                    .unwrap_or("No message")
                    .to_string();

                socket.broadcast(
                    "store_announcement",
                    serde_json::json!({"message": message}),
                );

                Ok(Some(Reply::ok(serde_json::json!({"announced": true}))))
            }
            "request_menu" => {
                let menu_payload = {
                    let flavors = self.store.flavors.lock().unwrap();
                    let menu: Vec<serde_json::Value> = flavors
                        .iter()
                        .filter(|f| f.in_stock)
                        .map(|f| {
                            serde_json::json!({
                                "name": f.name,
                                "price": format!("${:.2}", f.price_cents as f64 / 100.0),
                            })
                        })
                        .collect();
                    serde_json::json!({"flavors": menu})
                };

                socket.push("menu", &menu_payload).await;

                Ok(Some(Reply::ok(serde_json::json!({"sent": true}))))
            }
            "update_price" => {
                let flavor_id = payload["flavor_id"].as_u64().unwrap_or(0);
                let new_price_cents = payload["price_cents"].as_u64().unwrap_or(0);

                let mut flavors = self.store.flavors.lock().unwrap();
                if let Some(flavor) = flavors.iter_mut().find(|f| f.id == flavor_id) {
                    flavor.price_cents = new_price_cents;
                    let price_str = format_price(new_price_cents);
                    let flavor_name = flavor.name.clone();

                    socket.broadcast(
                        "price_updated",
                        serde_json::json!({
                            "flavor_id": flavor_id,
                            "price": price_str,
                            "flavor_name": flavor_name,
                        }),
                    );

                    Ok(Some(Reply::ok(serde_json::json!({"updated": true}))))
                } else {
                    Ok(Some(Reply::error(serde_json::json!({"reason": "flavor not found"}))))
                }
            }
            _ => Ok(None),
        }
    }

    async fn terminate(&self, reason: &str, socket: &mut ChannelSocket) {
        let customer = socket
            .get_assign::<CustomerName>()
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string());
        tracing::info!(
            customer = %customer,
            reason = %reason,
            "Customer left the store"
        );
    }
}

struct SupportChannel;

#[async_trait]
impl Channel for SupportChannel {
    async fn join(
        &self,
        _topic: &str,
        payload: &serde_json::Value,
        socket: &mut ChannelSocket,
    ) -> Result<serde_json::Value, ChannelError> {
        let customer = payload["customer_name"]
            .as_str()
            .unwrap_or("Guest")
            .to_string();
        tracing::info!(customer = %customer, "Customer joined support chat");
        socket.assign::<CustomerName>(customer.clone());

        Ok(serde_json::json!({
            "message": format!("🌺 Aloha {customer}! How can we help you today?"),
        }))
    }

    async fn handle_in(
        &self,
        event: &str,
        payload: &serde_json::Value,
        socket: &mut ChannelSocket,
    ) -> Result<Option<Reply>, ChannelError> {
        match event {
            "chat_message" => {
                let msg = payload["message"]
                    .as_str()
                    .unwrap_or("")
                    .to_lowercase();

                let response = if msg.contains("hour") || msg.contains("open") || msg.contains("close") {
                    "🕐 We're open Mon-Wed 10am-9pm, Thu 10am-10pm, Fri 10am-11pm, Sat 9am-11pm, Sun 9am-8pm! 🌞"
                } else if msg.contains("flavor") || msg.contains("menu") {
                    "🍦 We have 7 amazing flavors! Check out /menu for the full list! 🌴"
                } else if msg.contains("price") || msg.contains("cost") {
                    "💰 Scoops range from $4.00 to $5.25! Check our specials for deals! 🎉"
                } else if msg.contains("special") || msg.contains("deal") {
                    "🌟 Mahalo Monday: Buy 2 get 1 free! Tropical Thursday: 20% off! Aloha Hour: Half-price 3-5pm! 🎉"
                } else if msg.contains("order") {
                    "🛒 Head to /order to place your order! We'll get scooping right away! 🍨"
                } else if msg.contains("thank") || msg.contains("mahalo") {
                    "🤙 Mahalo to YOU! Come back anytime! 🌺✨"
                } else if msg.contains("hello") || msg.contains("hi") || msg.contains("aloha") {
                    "🌺 Aloha! Welcome to Mahalo Ice Cream! Ask me about flavors, hours, or specials! 🍦"
                } else {
                    "🍦 Great question! Ask me about our flavors, hours, specials, or toppings! 🌴"
                };

                // Broadcast the reply so all connected clients see it
                socket.broadcast(
                    "chat_reply",
                    serde_json::json!({"message": response}),
                );

                Ok(Some(Reply::ok(serde_json::json!({"response": response}))))
            }
            _ => Ok(None),
        }
    }

    async fn terminate(&self, reason: &str, socket: &mut ChannelSocket) {
        let customer = socket
            .get_assign::<CustomerName>()
            .cloned()
            .unwrap_or_else(|| "Unknown".to_string());
        tracing::info!(customer = %customer, reason = %reason, "Customer left support chat");
    }
}

// ---------------------------------------------------------------------------
// Helper: simple timestamp
// ---------------------------------------------------------------------------

/// Extract order ID from a topic string like "order:42".
fn parse_order_id(topic: &str) -> u64 {
    topic
        .strip_prefix("order:")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn chrono_now() -> String {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    format!("{}s", d.as_secs())
}

// ---------------------------------------------------------------------------
// Template Rendering Helpers
// ---------------------------------------------------------------------------

fn format_price(cents: u64) -> String {
    format!("${:.2}", cents as f64 / 100.0)
}

fn format_flavor(f: &Flavor) -> serde_json::Value {
    serde_json::json!({
        "id": f.id,
        "name": f.name,
        "description": f.description,
        "price": format_price(f.price_cents),
        "in_stock": f.in_stock,
    })
}

fn render_template(conn: Conn, tera: &Tera, name: &str, context: &Context) -> Conn {
    match tera.render(name, context) {
        Ok(html) => conn.put_status(StatusCode::OK).put_resp_body(html),
        Err(e) => {
            tracing::error!(template = name, error = %e, "Template render error");
            conn.put_status(StatusCode::INTERNAL_SERVER_ERROR)
                .put_resp_body(format!("<h1>500 - Render Error</h1><p>{e}</p>"))
        }
    }
}

fn render_home(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let mut context = Context::new();
    let flavors = store.flavors.lock().unwrap();
    let featured: Vec<serde_json::Value> = flavors.iter().take(3).map(|f| format_flavor(f)).collect();
    context.insert("flavors", &featured);
    render_template(conn, tera, "home.html", &context)
}

fn render_menu(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let mut context = Context::new();
    let flavors = store.flavors.lock().unwrap();
    let all_flavors: Vec<serde_json::Value> = flavors.iter().map(|f| format_flavor(f)).collect();
    context.insert("flavors", &all_flavors);

    let toppings: Vec<serde_json::Value> = vec![
        serde_json::json!({"name": "Macadamia Nuts", "price": "$0.75"}),
        serde_json::json!({"name": "Toasted Coconut", "price": "$0.50"}),
        serde_json::json!({"name": "Mochi Bits", "price": "$1.00"}),
        serde_json::json!({"name": "Li Hing Mui Powder", "price": "$0.50"}),
        serde_json::json!({"name": "Hot Fudge", "price": "$0.75"}),
        serde_json::json!({"name": "Passion Fruit Drizzle", "price": "$0.75"}),
    ];
    context.insert("toppings", &toppings);

    let specials: Vec<serde_json::Value> = vec![
        serde_json::json!({"name": "Mahalo Monday", "description": "Buy 2 scoops, get 1 free!", "days": "Monday"}),
        serde_json::json!({"name": "Tropical Thursday", "description": "All tropical flavors 20% off", "days": "Thursday"}),
        serde_json::json!({"name": "Aloha Hour", "description": "Half-price single scoops from 3-5pm daily", "days": "Every day"}),
    ];
    context.insert("specials", &specials);

    render_template(conn, tera, "menu.html", &context)
}

fn render_order(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let mut context = Context::new();
    let flavors = store.flavors.lock().unwrap();
    let in_stock: Vec<serde_json::Value> = flavors.iter().filter(|f| f.in_stock).map(|f| format_flavor(f)).collect();
    context.insert("flavors", &in_stock);
    render_template(conn, tera, "order.html", &context)
}

fn render_order_status(conn: Conn, tera: &Tera) -> Conn {
    let order_id = conn.path_params.get("id").cloned().unwrap_or_else(|| "0".to_string());
    let mut context = Context::new();
    context.insert("order_id", &order_id);
    render_template(conn, tera, "order_status.html", &context)
}

fn render_about(conn: Conn, tera: &Tera) -> Conn {
    let mut context = Context::new();

    let hours: Vec<serde_json::Value> = vec![
        serde_json::json!({"day": "Monday", "time": "10:00 AM - 9:00 PM"}),
        serde_json::json!({"day": "Tuesday", "time": "10:00 AM - 9:00 PM"}),
        serde_json::json!({"day": "Wednesday", "time": "10:00 AM - 9:00 PM"}),
        serde_json::json!({"day": "Thursday", "time": "10:00 AM - 10:00 PM"}),
        serde_json::json!({"day": "Friday", "time": "10:00 AM - 11:00 PM"}),
        serde_json::json!({"day": "Saturday", "time": "9:00 AM - 11:00 PM"}),
        serde_json::json!({"day": "Sunday", "time": "9:00 AM - 8:00 PM"}),
    ];
    context.insert("hours", &hours);

    let specials: Vec<serde_json::Value> = vec![
        serde_json::json!({"name": "Mahalo Monday", "description": "Buy 2 scoops, get 1 free!", "days": "Monday"}),
        serde_json::json!({"name": "Tropical Thursday", "description": "All tropical flavors 20% off", "days": "Thursday"}),
        serde_json::json!({"name": "Aloha Hour", "description": "Half-price single scoops from 3-5pm daily", "days": "Every day"}),
    ];
    context.insert("specials", &specials);

    render_template(conn, tera, "about.html", &context)
}

fn render_flavor(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let id: u64 = conn.path_params.get("id").and_then(|s| s.parse().ok()).unwrap_or(0);
    let flavors = store.flavors.lock().unwrap();
    match flavors.iter().find(|f| f.id == id) {
        Some(flavor) => {
            let mut context = Context::new();
            context.insert("flavor", &format_flavor(flavor));
            render_template(conn, tera, "flavor.html", &context)
        }
        None => {
            conn.put_status(StatusCode::NOT_FOUND)
                .put_resp_body("<h1>🍦 Flavor not found!</h1><p><a href=\"/menu\">Back to Menu</a></p>")
        }
    }
}

// ---------------------------------------------------------------------------
// Toppings & specials data
// ---------------------------------------------------------------------------

fn toppings_json() -> String {
    serde_json::json!({
        "toppings": [
            {"name": "Macadamia Nuts", "price_cents": 75},
            {"name": "Toasted Coconut", "price_cents": 50},
            {"name": "Mochi Bits", "price_cents": 100},
            {"name": "Li Hing Mui Powder", "price_cents": 50},
            {"name": "Hot Fudge", "price_cents": 75},
            {"name": "Passion Fruit Drizzle", "price_cents": 75},
        ]
    })
    .to_string()
}

fn specials_json() -> String {
    serde_json::json!({
        "specials": [
            {
                "name": "Mahalo Monday",
                "description": "Buy 2 scoops, get 1 free!",
                "days": ["Monday"]
            },
            {
                "name": "Tropical Thursday",
                "description": "All tropical flavors 20% off",
                "days": ["Thursday"]
            },
            {
                "name": "Aloha Hour",
                "description": "Half-price single scoops from 3-5pm daily",
                "days": ["Every day"]
            }
        ]
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Start PubSub
    let pubsub = PubSub::start();

    // Set up telemetry
    let telemetry = Telemetry::new(512);

    telemetry
        .attach(&["mahalo"], |event| {
            tracing::debug!(
                name = ?event.name,
                measurements = ?event.measurements,
                "[telemetry] framework event"
            );
        })
        .await;

    telemetry
        .attach(&["ice_cream"], |event| {
            tracing::info!(
                name = ?event.name,
                metadata = ?event.metadata,
                "[telemetry] app event"
            );
        })
        .await;

    // Create the store
    let store = Store::new(telemetry.clone());

    // Initialize Tera templates
    let template_dir = format!("{}/templates/**/*", env!("CARGO_MANIFEST_DIR"));
    let tera = Arc::new(Tera::new(&template_dir).expect("Failed to load templates"));

    // Static files plug
    let static_dir = format!("{}/static", env!("CARGO_MANIFEST_DIR"));
    let static_files = StaticFiles::new("/static", static_dir);

    // Build controllers
    let flavor_controller = Arc::new(FlavorController {
        store: store.clone(),
    });
    let order_controller = Arc::new(OrderController {
        store: store.clone(),
        pubsub: pubsub.clone(),
    });

    // --- Pipelines ---

    let browser_pipeline = Pipeline::new("browser").plug(plug_fn(|conn: Conn| async {
        conn.put_resp_header("content-type", "text/html; charset=utf-8")
    }));

    let api_pipeline = Pipeline::new("api")
        .plug(plug_fn(|conn: Conn| async {
            conn.put_resp_header("content-type", "application/json")
        }))
        .plug(plug_fn(|conn: Conn| async {
            let id = format!("req-{:x}", rand_id());
            conn.assign::<RequestId>(id)
        }));

    let auth_pipeline = Pipeline::new("auth").plug(plug_fn(|conn: Conn| async {
        match conn.headers.get("x-api-key") {
            Some(_key) => conn.assign::<CurrentUser>("authenticated-user".to_string()),
            None => conn
                .put_status(StatusCode::UNAUTHORIZED)
                .put_resp_body(r#"{"error":"Missing x-api-key header"}"#)
                .halt(),
        }
    }));

    // --- Standalone plug closures that capture store/telemetry ---

    let store_for_menu = store.clone();
    let telemetry_for_menu = telemetry.clone();

    // --- Router ---

    let router = MahaloRouter::new()
        .pipeline(browser_pipeline)
        .pipeline(api_pipeline)
        .pipeline(auth_pipeline)
        // Browser routes
        .scope("/", &["browser"], |s| {
            let tera_home = tera.clone();
            let store_home = store.clone();
            s.get("/", plug_fn(move |conn: Conn| {
                let tera = tera_home.clone();
                let store = store_home.clone();
                async move { render_home(conn, &tera, &store) }
            }));

            let tera_menu = tera.clone();
            let store_menu = store.clone();
            s.get("/menu", plug_fn(move |conn: Conn| {
                let tera = tera_menu.clone();
                let store = store_menu.clone();
                async move { render_menu(conn, &tera, &store) }
            }));

            let tera_order = tera.clone();
            let store_order = store.clone();
            s.get("/order", plug_fn(move |conn: Conn| {
                let tera = tera_order.clone();
                let store = store_order.clone();
                async move { render_order(conn, &tera, &store) }
            }));

            let tera_status = tera.clone();
            s.get("/orders/:id", plug_fn(move |conn: Conn| {
                let tera = tera_status.clone();
                async move { render_order_status(conn, &tera) }
            }));

            let tera_about = tera.clone();
            s.get("/about", plug_fn(move |conn: Conn| {
                let tera = tera_about.clone();
                async move { render_about(conn, &tera) }
            }));

            let tera_flavor = tera.clone();
            let store_flavor = store.clone();
            s.get("/flavors/:id", plug_fn(move |conn: Conn| {
                let tera = tera_flavor.clone();
                let store = store_flavor.clone();
                async move { render_flavor(conn, &tera, &store) }
            }));
        })
        .get(
            "/health",
            plug_fn(|conn: Conn| async {
                conn.put_status(StatusCode::OK)
                    .put_resp_header("content-type", "application/json")
                    .put_resp_body(r#"{"status":"healthy","store":"open"}"#)
            }),
        )
        // API scope - public endpoints (flavors, menu, hours, about, toppings, specials)
        .scope("/api", &["api"], |s| {
            // Menu (with telemetry span)
            s.get(
                "/menu",
                plug_fn(move |conn: Conn| {
                    let store = store_for_menu.clone();
                    let telemetry = telemetry_for_menu.clone();
                    async move {
                        let menu_json = telemetry
                            .span(
                                &["ice_cream", "menu", "build"],
                                HashMap::new(),
                                || async {
                                    let flavors = store.flavors.lock().unwrap();
                                    let menu: Vec<serde_json::Value> = flavors
                                        .iter()
                                        .filter(|f| f.in_stock)
                                        .map(|f| {
                                            serde_json::json!({
                                                "name": f.name,
                                                "price": format!("${:.2}", f.price_cents as f64 / 100.0),
                                                "description": f.description,
                                            })
                                        })
                                        .collect();
                                    serde_json::json!({"menu": menu}).to_string()
                                },
                            )
                            .await;
                        conn.put_status(StatusCode::OK).put_resp_body(menu_json)
                    }
                }),
            );

            // Store hours
            s.get(
                "/hours",
                plug_fn(|conn: Conn| async {
                    conn.put_status(StatusCode::OK).put_resp_body(
                        serde_json::json!({
                            "hours": {
                                "monday": "10:00 AM - 9:00 PM",
                                "tuesday": "10:00 AM - 9:00 PM",
                                "wednesday": "10:00 AM - 9:00 PM",
                                "thursday": "10:00 AM - 10:00 PM",
                                "friday": "10:00 AM - 11:00 PM",
                                "saturday": "9:00 AM - 11:00 PM",
                                "sunday": "9:00 AM - 8:00 PM"
                            },
                            "timezone": "HST"
                        })
                        .to_string(),
                    )
                }),
            );

            // About
            s.get(
                "/about",
                plug_fn(|conn: Conn| async {
                    conn.put_status(StatusCode::OK).put_resp_body(
                        serde_json::json!({
                            "name": "Mahalo Ice Cream Store",
                            "tagline": "Tropical flavors, island vibes",
                            "location": "Honolulu, HI",
                            "established": 2024,
                            "framework": "Built with Mahalo - a Phoenix-like web framework for Rust"
                        })
                        .to_string(),
                    )
                }),
            );

            // Flavors CRUD (public - no auth required)
            s.resources("/flavors", flavor_controller);

            // Toppings
            s.get(
                "/toppings",
                plug_fn(|conn: Conn| async {
                    conn.put_status(StatusCode::OK)
                        .put_resp_body(toppings_json())
                }),
            );

            // Daily specials
            s.get(
                "/specials",
                plug_fn(|conn: Conn| async {
                    conn.put_status(StatusCode::OK)
                        .put_resp_body(specials_json())
                }),
            );
        })
        // Orders scope - auth-protected
        .scope("/api", &["api", "auth"], |s| {
            s.resources("/orders", order_controller);
        });

    // --- Channel Router ---

    let channel_router = ChannelRouter::new()
        .channel(
            "order:*",
            Arc::new(OrderChannel {
                store: store.clone(),
            }),
        )
        .channel("store:lobby", Arc::new(StoreChannel { store: store.clone() }))
        .channel("support:*", Arc::new(SupportChannel));

    // --- Start Server ---

    let runtime = Arc::new(rebar_core::runtime::Runtime::new(4));
    let addr: std::net::SocketAddr = "127.0.0.1:4000".parse().unwrap();

    let endpoint = MahaloEndpoint::new(router, addr, runtime)
        .channel_router(channel_router)
        .pubsub(pubsub.clone())
        .error_handler(|status: StatusCode, conn: Conn| {
            conn.put_status(status)
                .put_resp_header("content-type", "text/html; charset=utf-8")
                .put_resp_body(format!(
                    r#"<!DOCTYPE html><html><head><title>{code} - Mahalo Ice Cream</title>
<link rel="stylesheet" href="/static/css/style.css"></head>
<body><nav class="nav"><div class="nav-content"><a href="/" class="nav-logo">🍦 Mahalo Ice Cream</a>
<div class="nav-links"><a href="/">🏠 Home</a><a href="/menu">📋 Menu</a><a href="/order">🛒 Order</a><a href="/about">🌺 About</a></div></div></nav>
<div class="container" style="text-align:center;padding:4rem 1rem">
<h1>🏝️ {code}</h1><p>Oops! We couldn't find what you're looking for. 😅</p>
<a href="/" class="btn">🏠 Back to Home</a></div>
<footer class="footer"><p>🌴 &copy; 2024 Mahalo Ice Cream Store 🌴</p></footer></body></html>"#,
                    code = status.as_u16()
                ))
        })
        .after(static_files);

    // Background task: fluctuate prices every 15 seconds for real-time demo
    let price_store = store.clone();
    let price_pubsub = pubsub.clone();
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(15)).await;
            tick += 1;

            let update = {
                let mut flavors = price_store.flavors.lock().unwrap();
                // Pick a flavor to update based on tick
                let idx = (tick as usize) % flavors.len();
                let flavor = &mut flavors[idx];

                // Fluctuate price by -25 to +25 cents
                let delta = ((tick * 7 + idx as u64 * 13) % 51) as i64 - 25;
                let new_price = (flavor.price_cents as i64 + delta).max(200) as u64;
                flavor.price_cents = new_price;

                (flavor.id, flavor.name.clone(), new_price)
            };

            let price_str = format_price(update.2);
            tracing::info!(
                flavor = %update.1,
                new_price = %price_str,
                "💰 Price updated!"
            );

            price_pubsub.broadcast(
                "store:lobby",
                "price_updated",
                serde_json::json!({
                    "flavor_id": update.0,
                    "price": price_str,
                    "flavor_name": update.1,
                }),
            );
        }
    });

    tracing::info!("🍦 Starting Mahalo Ice Cream Store on http://{addr}");
    tracing::info!("🔌 WebSocket available at ws://{addr}/ws");
    tracing::info!("💬 Support chat on the About page!");
    tracing::info!("💰 Prices update in real-time every 15s!");
    tracing::info!("🌐 Try: curl http://{addr}/api/menu");

    if let Err(e) = endpoint.start().await {
        tracing::error!("Server error: {e}");
    }
}

/// Simple pseudo-random ID generator (no external dep needed).
fn rand_id() -> u64 {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    d.as_nanos() as u64 ^ (d.as_secs() << 32)
}
