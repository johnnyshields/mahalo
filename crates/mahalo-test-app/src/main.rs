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
//! - Server-Sent Events (SSE) for price streaming
//! - Pipeline halting (auth plug)

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::StatusCode;
use serde::Serialize;

use tera::{Tera, Context};

use mahalo::{
    AssignKey, Channel, ChannelError, ChannelRouter, ChannelSocket, Conn, Controller,
    MahaloEndpoint, MahaloRouter, Pipeline, PubSub, Reply, StaticFiles, Telemetry,
    plug_fn, BoxFuture,
    sse_response, Event, SseOptions, KeepAlive,
};

// ---------------------------------------------------------------------------
// Data Models
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
struct Flavor {
    id: u64,
    name: String,
    summary: String,
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
    scoop_flavors: Vec<u64>,
    vessel: String,
    toppings: Vec<String>,
}

// ---------------------------------------------------------------------------
// In-Memory Store
// ---------------------------------------------------------------------------

/// Shared store data (Send + Sync, shared across worker threads).
#[derive(Clone)]
struct StoreData {
    flavors: Arc<Mutex<Vec<Flavor>>>,
    orders: Arc<Mutex<Vec<Order>>>,
}

/// Thread-local store with telemetry (created per-worker).
#[derive(Clone)]
struct Store {
    data: StoreData,
    telemetry: Telemetry,
}

impl StoreData {
    fn new() -> Self {
        let flavors = vec![
            Flavor {
                id: 1, name: "Coconut Dream".into(),
                summary: "Creamy coconut with toasted flakes".into(),
                description: "We source our coconuts from a small grove in Moloa\u{2018}a on Kaua\u{2018}i\u{2019}s north shore, where the trees catch the trade winds rolling off Kalihiwai Ridge. The meat is hand-pressed the same morning it\u{2019}s picked, then slow-churned with a pinch of Hawaiian sea salt from Hanapepe. Each scoop is finished with toasted coconut flakes roasted low and slow at our shop in Kapa\u{2018}a\u{2014}crunchy, buttery, and impossible to stop eating.".into(),
                price_cents: 450, in_stock: true,
            },
            Flavor {
                id: 2, name: "Pineapple Paradise".into(),
                summary: "Tangy pineapple sorbet".into(),
                description: "This bright, dairy-free sorbet starts with Sugarloaf pineapples grown on the red dirt hillsides above Po\u{2018}ip\u{016b}. They\u{2019}re sweeter and less acidic than anything you\u{2019}ll find on the mainland, with a floral perfume that hits you the second we crack one open. We juice them whole\u{2014}core and all\u{2014}and freeze the base fast to lock in that just-picked tang. It tastes like standing barefoot in a Kaua\u{2018}i pineapple field with the sun on your shoulders.".into(),
                price_cents: 400, in_stock: true,
            },
            Flavor {
                id: 3, name: "Passion Fruit Swirl".into(),
                summary: "Tart passion fruit with vanilla swirl".into(),
                description: "Liliko\u{2018}i vines grow wild along the fences and trailheads all over Kaua\u{2018}i, and our neighbors in Kilauea drop off bags of ripe fruit at our back door every week. We strain the seeds out (mostly) and fold that electric-yellow pulp into a tart, custardy base. The vanilla swirl comes from real Tahitian vanilla beans\u{2014}the same variety that thrives in Kaua\u{2018}i\u{2019}s humid valleys. One bite and you\u{2019}ll taste the wild, untamed side of the Garden Isle.".into(),
                price_cents: 500, in_stock: true,
            },
            Flavor {
                id: 4, name: "Guava Sunset".into(),
                summary: "Sweet guava with a hint of lime".into(),
                description: "Pink guava trees line the back roads from Wailua to Anahola, dropping fruit so ripe it perfumes the whole neighborhood. We collect ours from a family farm tucked behind Sleeping Giant, where the volcanic soil gives the fruit an almost candy-like sweetness. A squeeze of fresh lime from our own trees in Kapa\u{2018}a cuts through the richness and keeps every spoonful bright. The color is a deep sunset pink\u{2014}no dye needed, just pure Kaua\u{2018}i guava doing its thing.".into(),
                price_cents: 475, in_stock: true,
            },
            Flavor {
                id: 5, name: "Mango Tango".into(),
                summary: "Rich mango with chili flakes".into(),
                description: "Every summer, the Hayden and Rapoza mango trees across Kaua\u{2018}i go absolutely wild, and we stockpile the best of the harvest from orchards along the Coconut Coast. The base is thick, almost chewy with real mango fiber\u{2014}none of that thin, syrupy stuff. We finish each batch with a dusting of dried Hawaiian chili flakes from a small farm in Hanapepe Valley. The heat sneaks up on you after the sweetness fades, like the afternoon sun breaking through the clouds over Waimea Canyon.".into(),
                price_cents: 450, in_stock: true,
            },
            Flavor {
                id: 6, name: "Lychee Blossom".into(),
                summary: "Delicate lychee with rose water".into(),
                description: "Kaua\u{2018}i\u{2019}s lychee season is short and frantic\u{2014}the trees around \u{2018}\u{014c}lelo and Kalaheo burst with fruit for just a few weeks in June, and everyone races to pick before the birds get them all. We peel and pit each one by hand, keeping that translucent, floral sweetness intact. A few drops of rose water made from Damask roses grown at a small botanical garden near Princeville give it an almost perfume-like elegance. This is our most delicate flavor, and regulars plan their visits around lychee season.".into(),
                price_cents: 525, in_stock: true,
            },
            Flavor {
                id: 7, name: "Papaya Cream".into(),
                summary: "Smooth papaya with coconut cream".into(),
                description: "Sunrise papayas from the east side of Kaua\u{2018}i are the heart of this flavor\u{2014}smaller and sweeter than the big ones you see at the supermarket, with salmon-orange flesh that melts on your tongue. We blend them with rich coconut cream from the same Moloa\u{2018}a grove that supplies our Coconut Dream, creating something silky and tropical that tastes like breakfast on a lanai overlooking Kalapak\u{012b} Bay. When it\u{2019}s in stock, it goes fast\u{2014}so grab a scoop while you can.".into(),
                price_cents: 450, in_stock: false,
            },
        ];

        StoreData {
            flavors: Arc::new(Mutex::new(flavors)),
            orders: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

impl Store {
    fn new(data: StoreData, telemetry: Telemetry) -> Self {
        Store { data, telemetry }
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
            let flavors = self.store.data.flavors.lock().unwrap();
            let json = serde_json::to_string(&*flavors).unwrap();
            conn.put_status(StatusCode::OK).put_resp_body(json)
        })
    }

    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id = parse_id_param(&conn);

            let flavors = self.store.data.flavors.lock().unwrap();
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
                let mut flavors = self.store.data.flavors.lock().unwrap();
                let next_id = flavors.iter().map(|f| f.id).max().unwrap_or(0) + 1;

                let flavor = Flavor {
                    id: next_id,
                    name: body["name"].as_str().unwrap_or("Unknown").to_string(),
                    summary: body["summary"].as_str().unwrap_or("").to_string(),
                    description: body["description"].as_str().unwrap_or("").to_string(),
                    price_cents: body["price_cents"].as_u64().unwrap_or(0),
                    in_stock: body["in_stock"].as_bool().unwrap_or(true),
                };

                let json = serde_json::to_string(&flavor).unwrap();
                flavors.push(flavor);
                json
            };

            telemetry.execute(
                    &["ice_cream", "flavor", "created"],
                    HashMap::new(),
                    HashMap::new(),
                );

            conn.put_status(StatusCode::CREATED).put_resp_body(json)
        })
    }

    fn update(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id = parse_id_param(&conn);

            let body: serde_json::Value = match serde_json::from_slice(&conn.body) {
                Ok(v) => v,
                Err(_) => {
                    return conn
                        .put_status(StatusCode::BAD_REQUEST)
                        .put_resp_body(r#"{"error":"invalid JSON"}"#)
                }
            };

            let mut flavors = self.store.data.flavors.lock().unwrap();
            match flavors.iter_mut().find(|f| f.id == id) {
                Some(flavor) => {
                    if let Some(name) = body["name"].as_str() {
                        flavor.name = name.to_string();
                    }
                    if let Some(summary) = body["summary"].as_str() {
                        flavor.summary = summary.to_string();
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
            let id = parse_id_param(&conn);

            let mut flavors = self.store.data.flavors.lock().unwrap();
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
            let orders = self.store.data.orders.lock().unwrap();
            let json = serde_json::to_string(&*orders).unwrap();
            conn.put_status(StatusCode::OK).put_resp_body(json)
        })
    }

    fn show(&self, conn: Conn) -> BoxFuture<'_, Conn> {
        Box::pin(async {
            let id = parse_id_param(&conn);

            let orders = self.store.data.orders.lock().unwrap();
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
                let mut orders = self.store.data.orders.lock().unwrap();
                let next_id = orders.iter().map(|o| o.id).max().unwrap_or(0) + 1;

                let items: Vec<OrderItem> = body["items"]
                    .as_array()
                    .map(|arr| arr.iter().filter_map(|item| parse_order_item(item)).collect())
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
            telemetry.execute(
                    &["ice_cream", "order", "placed"],
                    HashMap::new(),
                    meta,
                );

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
            let id = parse_id_param(&conn);

            let body: serde_json::Value = match serde_json::from_slice(&conn.body) {
                Ok(v) => v,
                Err(_) => {
                    return conn
                        .put_status(StatusCode::BAD_REQUEST)
                        .put_resp_body(r#"{"error":"invalid JSON"}"#)
                }
            };

            let mut orders = self.store.data.orders.lock().unwrap();
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
            let id = parse_id_param(&conn);

            let mut orders = self.store.data.orders.lock().unwrap();
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
    pubsub: PubSub,
}

impl Channel for OrderChannel {
    fn join<'a>(
        &'a self,
        topic: &'a str,
        payload: &'a serde_json::Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<serde_json::Value, ChannelError>> {
        Box::pin(async move {
            let customer = payload["customer_name"]
                .as_str()
                .unwrap_or("Guest")
                .to_string();

            tracing::info!(topic = %topic, customer = %customer, "Customer joined order channel");
            socket.assign::<CustomerName>(customer.clone());

            // Extract order ID from topic (e.g., "order:42")
            let order_id = parse_order_id(topic);

            let status = {
                let orders = self.store.data.orders.lock().unwrap();
                orders
                    .iter()
                    .find(|o| o.id == order_id)
                    .map(|o| o.status.clone())
                    .unwrap_or_else(|| "not_found".to_string())
            };

            // Spawn a local task to simulate order status progression
            if status == "pending" {
                let sim_store = self.store.clone();
                let sim_pubsub = self.pubsub.clone();
                let topic = topic.to_string();
                let sim_order_id = order_id;
                rebar_core::executor::spawn(async move {
                    let statuses = ["preparing", "ready", "delivered"];
                    for next_status in statuses {
                        rebar_core::time::sleep(Duration::from_secs(3)).await;
                        {
                            let mut orders = sim_store.data.orders.lock().unwrap();
                            if let Some(order) = orders.iter_mut().find(|o| o.id == sim_order_id) {
                                order.status = next_status.to_string();
                            }
                        }
                        tracing::info!(order_id = sim_order_id, status = next_status, "📦 Order status updated");
                        sim_pubsub.broadcast(
                            &topic,
                            "status_changed",
                            serde_json::json!({
                                "order_id": sim_order_id,
                                "status": next_status,
                            }),
                        );
                    }
                }).detach();
            }

            Ok(serde_json::json!({
                "status": status,
                "customer": customer,
            }))
        })
    }

    fn handle_in<'a>(
        &'a self,
        event: &'a str,
        payload: &'a serde_json::Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Option<Reply>, ChannelError>> {
        Box::pin(async move {
            match event {
                "update_status" => {
                    let order_id = parse_order_id(&socket.topic);

                    let new_status = payload["status"]
                        .as_str()
                        .unwrap_or("unknown")
                        .to_string();

                    let mut orders = self.store.data.orders.lock().unwrap();
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

                    let item = match parse_order_item(payload) {
                        Some(item) => item,
                        None => {
                            return Ok(Some(Reply::error(
                                serde_json::json!({"error": "at least 1 scoop flavor required"}),
                            )));
                        }
                    };

                    let mut orders = self.store.data.orders.lock().unwrap();
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
                            serde_json::json!({"error": "order not found"}),
                        )))
                    }
                }
                _ => Ok(None),
            }
        })
    }

    fn terminate<'a>(
        &'a self,
        reason: &'a str,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let customer = socket
                .get_assign::<CustomerName>()
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string());
            tracing::info!(
                customer = %customer,
                reason = %reason,
                "Customer left order channel"
            );
        })
    }
}

struct StoreChannel {
    store: Store,
}

impl Channel for StoreChannel {
    fn join<'a>(
        &'a self,
        _topic: &'a str,
        payload: &'a serde_json::Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<serde_json::Value, ChannelError>> {
        Box::pin(async move {
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
        })
    }

    fn handle_in<'a>(
        &'a self,
        event: &'a str,
        payload: &'a serde_json::Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Option<Reply>, ChannelError>> {
        Box::pin(async move {
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
                        let flavors = self.store.data.flavors.lock().unwrap();
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

                    let mut flavors = self.store.data.flavors.lock().unwrap();
                    if let Some(flavor) = flavors.iter_mut().find(|f| f.id == flavor_id) {
                        flavor.price_cents = new_price_cents;
                        let flavor_name = flavor.name.clone();

                        socket.broadcast(
                            "price_updated",
                            price_update_payload(flavor_id, &flavor_name, new_price_cents),
                        );

                        Ok(Some(Reply::ok(serde_json::json!({"updated": true}))))
                    } else {
                        Ok(Some(Reply::error(serde_json::json!({"error": "flavor not found"}))))
                    }
                }
                _ => Ok(None),
            }
        })
    }

    fn terminate<'a>(
        &'a self,
        reason: &'a str,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let customer = socket
                .get_assign::<CustomerName>()
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string());
            tracing::info!(
                customer = %customer,
                reason = %reason,
                "Customer left the store"
            );
        })
    }
}

struct ChatResponder;

impl ChatResponder {
    fn respond(&self, message: &str) -> &'static str {
        let msg = message.to_lowercase();
        if msg.contains("hour") || msg.contains("open") || msg.contains("close") {
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
        }
    }
}

struct SupportChannel;

impl Channel for SupportChannel {
    fn join<'a>(
        &'a self,
        _topic: &'a str,
        payload: &'a serde_json::Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<serde_json::Value, ChannelError>> {
        Box::pin(async move {
            let customer = payload["customer_name"]
                .as_str()
                .unwrap_or("Guest")
                .to_string();
            tracing::info!(customer = %customer, "Customer joined support chat");
            socket.assign::<CustomerName>(customer.clone());

            Ok(serde_json::json!({
                "message": format!("🌺 Aloha {customer}! How can we help you today?"),
            }))
        })
    }

    fn handle_in<'a>(
        &'a self,
        event: &'a str,
        payload: &'a serde_json::Value,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, Result<Option<Reply>, ChannelError>> {
        Box::pin(async move {
            match event {
                "chat_message" => {
                    let msg = payload["message"].as_str().unwrap_or("");
                    let responder = ChatResponder;
                    let response = responder.respond(msg);

                    // Broadcast the reply so all connected clients see it
                    socket.broadcast(
                        "chat_reply",
                        serde_json::json!({"message": response}),
                    );

                    Ok(Some(Reply::ok(serde_json::json!({"response": response}))))
                }
                _ => Ok(None),
            }
        })
    }

    fn terminate<'a>(
        &'a self,
        reason: &'a str,
        socket: &'a mut ChannelSocket,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let customer = socket
                .get_assign::<CustomerName>()
                .cloned()
                .unwrap_or_else(|| "Unknown".to_string());
            tracing::info!(customer = %customer, reason = %reason, "Customer left support chat");
        })
    }
}

// ---------------------------------------------------------------------------
// Helper: simple timestamp
// ---------------------------------------------------------------------------

fn parse_id_param(conn: &Conn) -> u64 {
    conn.path_param("id")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

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

/// Max scoops per item (matches frontend MAX_SCOOPS).
const MAX_SCOOPS: usize = 3;

/// Parse an OrderItem from a JSON value, validating vessel and scoop count.
/// Returns `None` if scoop_flavors is empty (invalid item).
fn parse_order_item(item: &serde_json::Value) -> Option<OrderItem> {
    let mut scoop_flavors: Vec<u64> = item["scoop_flavors"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
        .unwrap_or_default();

    if scoop_flavors.is_empty() {
        return None;
    }
    scoop_flavors.truncate(MAX_SCOOPS);

    let vessel = match item["vessel"].as_str() {
        Some("cone") => "cone",
        _ => "cup",
    }
    .to_string();

    let toppings: Vec<String> = item["toppings"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    Some(OrderItem {
        scoop_flavors,
        vessel,
        toppings,
    })
}

// ---------------------------------------------------------------------------
// Template Rendering Helpers
// ---------------------------------------------------------------------------

fn format_price(cents: u64) -> String {
    format!("${:.2}", cents as f64 / 100.0)
}

fn price_update_payload(flavor_id: u64, flavor_name: &str, price_cents: u64) -> serde_json::Value {
    serde_json::json!({
        "flavor_id": flavor_id,
        "price": format_price(price_cents),
        "flavor_name": flavor_name,
    })
}

fn format_flavor(f: &Flavor) -> serde_json::Value {
    serde_json::json!({
        "id": f.id,
        "name": f.name,
        "summary": f.summary,
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

fn format_specials() -> Vec<serde_json::Value> {
    specials_data()
        .iter()
        .map(|s| serde_json::json!({"name": s.name, "description": s.description, "days": s.days}))
        .collect()
}

fn format_toppings() -> Vec<serde_json::Value> {
    toppings_data()
        .iter()
        .map(|t| serde_json::json!({"name": t.name, "price": format_price(t.price_cents)}))
        .collect()
}

fn render_home(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let mut context = Context::new();
    let flavors = store.data.flavors.lock().unwrap();
    let all_flavors: Vec<serde_json::Value> = flavors.iter().map(|f| format_flavor(f)).collect();
    context.insert("flavors", &all_flavors);
    context.insert("specials", &format_specials());
    render_template(conn, tera, "home.html", &context)
}

fn render_menu(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let mut context = Context::new();
    let flavors = store.data.flavors.lock().unwrap();
    let all_flavors: Vec<serde_json::Value> = flavors.iter().map(|f| format_flavor(f)).collect();
    context.insert("flavors", &all_flavors);
    context.insert("toppings", &format_toppings());
    context.insert("specials", &format_specials());
    render_template(conn, tera, "menu.html", &context)
}

fn render_order(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let mut context = Context::new();
    let flavors = store.data.flavors.lock().unwrap();
    let in_stock: Vec<serde_json::Value> = flavors.iter().filter(|f| f.in_stock).map(|f| format_flavor(f)).collect();
    context.insert("flavors", &in_stock);
    context.insert("toppings", &format_toppings());
    render_template(conn, tera, "order.html", &context)
}

fn render_order_status(conn: Conn, tera: &Tera) -> Conn {
    let order_id = conn.path_param("id").unwrap_or("0").to_string();
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

    context.insert("specials", &format_specials());

    render_template(conn, tera, "about.html", &context)
}

fn render_flavor(conn: Conn, tera: &Tera, store: &Store) -> Conn {
    let id = parse_id_param(&conn);
    let flavors = store.data.flavors.lock().unwrap();
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
// Toppings & specials data (single source of truth)
// ---------------------------------------------------------------------------

struct Topping {
    name: &'static str,
    price_cents: u64,
}

fn toppings_data() -> Vec<Topping> {
    vec![
        Topping { name: "Macadamia Nuts", price_cents: 75 },
        Topping { name: "Toasted Coconut", price_cents: 50 },
        Topping { name: "Mochi Bits", price_cents: 100 },
        Topping { name: "Li Hing Mui Powder", price_cents: 50 },
        Topping { name: "Hot Fudge", price_cents: 75 },
        Topping { name: "Passion Fruit Drizzle", price_cents: 75 },
    ]
}

struct Special {
    name: &'static str,
    description: &'static str,
    days: &'static str,
}

fn specials_data() -> Vec<Special> {
    vec![
        Special { name: "Mahalo Monday", description: "Buy 2 scoops, get 1 free!", days: "Monday" },
        Special { name: "Tropical Thursday", description: "All tropical flavors 20% off", days: "Thursday" },
        Special { name: "Aloha Hour", description: "Half-price single scoops from 3-5pm daily", days: "Every day" },
    ]
}

fn toppings_json() -> String {
    let toppings: Vec<serde_json::Value> = toppings_data()
        .iter()
        .map(|t| serde_json::json!({"name": t.name, "price_cents": t.price_cents}))
        .collect();
    serde_json::json!({"toppings": toppings}).to_string()
}

fn specials_json() -> String {
    let specials: Vec<serde_json::Value> = specials_data()
        .iter()
        .map(|s| serde_json::json!({"name": s.name, "description": s.description, "days": [s.days]}))
        .collect();
    serde_json::json!({"specials": specials}).to_string()
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Start PubSub (Send + Sync — shared across workers)
    let pubsub = PubSub::start();

    // Shared store data (Send + Sync — shared across workers)
    let store_data = StoreData::new();

    // Initialize Tera templates (Send + Sync via Arc)
    let template_dir = format!("{}/templates/**/*", env!("CARGO_MANIFEST_DIR"));
    let tera = Arc::new(Tera::new(&template_dir).expect("Failed to load templates"));

    // Static files directory
    let static_dir = format!("{}/static", env!("CARGO_MANIFEST_DIR"));

    let addr: std::net::SocketAddr = "127.0.0.1:4000".parse().unwrap();

    // --- Build endpoint with factory pattern ---
    // Each factory creates thread-local (!Send) instances per worker thread.

    let endpoint = MahaloEndpoint::new(
        {
            let store_data = store_data.clone();
            let tera = tera.clone();
            let pubsub = pubsub.clone();
            move || {
                // Create per-worker telemetry + store
                let telemetry = make_telemetry();
                let store = Store::new(store_data.clone(), telemetry);
                build_router(store, tera.clone(), pubsub.clone())
            }
        },
        addr,
    )
    .channels({
        let store_data = store_data.clone();
        let pubsub = pubsub.clone();
        move || {
            use mahalo::WsConfig;
            let telemetry = make_telemetry();
            let store = Store::new(store_data.clone(), telemetry);
            let channel_router = ChannelRouter::new()
                .channel(
                    "order:*",
                    Rc::new(OrderChannel {
                        store: store.clone(),
                        pubsub: pubsub.clone(),
                    }),
                )
                .channel("store:lobby", Rc::new(StoreChannel { store: store.clone() }))
                .channel("support:*", Rc::new(SupportChannel));
            WsConfig {
                channel_router: Rc::new(channel_router),
                pubsub: pubsub.clone(),
            }
        }
    })
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
    .after({
        let static_dir = static_dir.clone();
        move || Box::new(StaticFiles::new("/static", static_dir.clone())) as Box<dyn mahalo::Plug>
    });

    // Background task: fluctuate prices every 15 seconds for real-time demo
    let price_store = store_data.clone();
    let price_pubsub = pubsub.clone();
    let startup_seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    std::thread::spawn(move || {
        let mut tick = 0u64;
        loop {
            std::thread::sleep(Duration::from_secs(3));
            tick += 1;

            let update = {
                let mut flavors = price_store.flavors.lock().unwrap();
                // Pick a flavor to update based on tick
                let idx = (tick as usize) % flavors.len();
                let flavor = &mut flavors[idx];

                // Fluctuate price by -25 to +25 cents (seed adds per-session variance)
                let delta = ((tick * 7 + idx as u64 * 13 + startup_seed) % 51) as i64 - 25;
                let new_price = (flavor.price_cents as i64 + delta).max(200) as u64;
                flavor.price_cents = new_price;

                (flavor.id, flavor.name.clone(), new_price)
            };

            tracing::info!(
                flavor = %update.1,
                new_price = %format_price(update.2),
                "💰 Price updated!"
            );

            price_pubsub.broadcast(
                "prices",
                "price_updated",
                price_update_payload(update.0, &update.1, update.2),
            );
        }
    });

    tracing::info!("🍦 Starting Mahalo Ice Cream Store on http://{addr}");
    tracing::info!("🔌 WebSocket available at ws://{addr}/ws");
    tracing::info!("💬 Support chat on the About page!");
    tracing::info!("📡 Prices stream via SSE every 3s at /api/prices/stream");
    tracing::info!("🌐 Try: curl http://{addr}/api/menu");

    if let Err(e) = endpoint.start() {
        tracing::error!("Server error: {e}");
    }
}

/// Create per-worker telemetry with standard handlers attached.
fn make_telemetry() -> Telemetry {
    let telemetry = Telemetry::new(512);
    telemetry.attach(&["mahalo"], |event| {
        tracing::debug!(
            name = ?event.name,
            measurements = ?event.measurements,
            "[telemetry] framework event"
        );
    });
    telemetry.attach(&["ice_cream"], |event| {
        tracing::info!(
            name = ?event.name,
            metadata = ?event.metadata,
            "[telemetry] app event"
        );
    });
    telemetry
}

fn build_router(
    store: Store,
    tera: Arc<Tera>,
    pubsub: PubSub,
) -> MahaloRouter {
    // Build controllers (thread-local, created per worker)
    let flavor_controller: Rc<dyn Controller> = Rc::new(FlavorController {
        store: store.clone(),
    });
    let order_controller: Rc<dyn Controller> = Rc::new(OrderController {
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
    let telemetry_for_menu = store.telemetry.clone();

    // --- Router ---

    MahaloRouter::new()
        .pipeline(browser_pipeline)
        .pipeline(api_pipeline)
        .pipeline(auth_pipeline)
        // Browser routes
        .scope("/", &["browser"], |s| {
            macro_rules! tera_route {
                ($s:expr, $path:expr, $tera:expr, $store:expr, |$c:ident, $t:ident, $st:ident| $body:expr) => {{
                    let tera_c = $tera.clone();
                    let store_c = $store.clone();
                    $s.get($path, plug_fn(move |$c: Conn| {
                        let $t = tera_c.clone();
                        let $st = store_c.clone();
                        async move { $body }
                    }));
                }};
                ($s:expr, $path:expr, $tera:expr, |$c:ident, $t:ident| $body:expr) => {{
                    let tera_c = $tera.clone();
                    $s.get($path, plug_fn(move |$c: Conn| {
                        let $t = tera_c.clone();
                        async move { $body }
                    }));
                }};
            }

            tera_route!(s, "/", tera, store, |conn, tera, store| render_home(conn, &tera, &store));
            tera_route!(s, "/menu", tera, store, |conn, tera, store| render_menu(conn, &tera, &store));
            tera_route!(s, "/order", tera, store, |conn, tera, store| render_order(conn, &tera, &store));
            tera_route!(s, "/orders/:id", tera, |conn, tera| render_order_status(conn, &tera));
            tera_route!(s, "/about", tera, |conn, tera| render_about(conn, &tera));
            tera_route!(s, "/flavors/:id", tera, store, |conn, tera, store| render_flavor(conn, &tera, &store));
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
                                    let flavors = store.data.flavors.lock().unwrap();
                                    let menu: Vec<serde_json::Value> = flavors
                                        .iter()
                                        .filter(|f| f.in_stock)
                                        .map(|f| {
                                            serde_json::json!({
                                                "name": f.name,
                                                "price": format!("${:.2}", f.price_cents as f64 / 100.0),
                                                "summary": f.summary,
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

            // SSE price stream
            {
                let pubsub = pubsub.clone();
                s.get(
                    "/prices/stream",
                    plug_fn(move |conn: Conn| {
                        let pubsub = pubsub.clone();
                        async move {
                            let (conn, sender) = sse_response(
                                conn,
                                SseOptions::default()
                                    .keep_alive(KeepAlive::new(Duration::from_secs(15))),
                            );
                            if let Some(rx) = pubsub.subscribe("prices") {
                                rebar_core::executor::spawn(async move {
                                    loop {
                                        match rx.try_recv() {
                                            Ok(msg) => {
                                                if msg.event == "price_updated" {
                                                    let event = Event::default()
                                                        .event("price_updated")
                                                        .json_data(&*msg.payload);
                                                    if sender.send(event).is_err() {
                                                        tracing::debug!("SSE price client disconnected");
                                                        break;
                                                    }
                                                }
                                            }
                                            Err(std::sync::mpsc::TryRecvError::Empty) => {
                                                rebar_core::time::sleep(
                                                    Duration::from_millis(5),
                                                )
                                                .await;
                                            }
                                            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                                                break;
                                            }
                                        }
                                    }
                                })
                                .detach();
                            } else {
                                tracing::warn!("PubSub subscribe failed for prices SSE stream");
                            }
                            conn
                        }
                    }),
                );
            }
        })
        // Orders scope - auth-protected
        .scope("/api", &["api", "auth"], |s| {
            s.resources("/orders", order_controller);
        })
}

/// Simple pseudo-random ID generator (no external dep needed).
fn rand_id() -> u64 {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    d.as_nanos() as u64 ^ (d.as_secs() << 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- format_price --

    #[test]
    fn test_format_price_basic() {
        assert_eq!(format_price(450), "$4.50");
        assert_eq!(format_price(100), "$1.00");
        assert_eq!(format_price(0), "$0.00");
        assert_eq!(format_price(525), "$5.25");
        assert_eq!(format_price(99), "$0.99");
    }

    // -- format_flavor --

    #[test]
    fn test_format_flavor() {
        let f = Flavor {
            id: 1,
            name: "Test".into(),
            summary: "Desc".into(),
            description: "Long desc".into(),
            price_cents: 450,
            in_stock: true,
        };
        let v = format_flavor(&f);
        assert_eq!(v["id"], 1);
        assert_eq!(v["name"], "Test");
        assert_eq!(v["price"], "$4.50");
        assert_eq!(v["in_stock"], true);
    }

    // -- price_update_payload --

    #[test]
    fn test_price_update_payload() {
        let v = price_update_payload(3, "Vanilla", 525);
        assert_eq!(v["flavor_id"], 3);
        assert_eq!(v["price"], "$5.25");
        assert_eq!(v["flavor_name"], "Vanilla");
    }

    #[test]
    fn test_price_update_payload_zero() {
        let v = price_update_payload(0, "Free Scoop", 0);
        assert_eq!(v["flavor_id"], 0);
        assert_eq!(v["price"], "$0.00");
        assert_eq!(v["flavor_name"], "Free Scoop");
    }

    // -- parse_order_id --

    #[test]
    fn test_parse_order_id() {
        assert_eq!(parse_order_id("order:42"), 42);
        assert_eq!(parse_order_id("order:1"), 1);
        assert_eq!(parse_order_id("order:0"), 0);
        assert_eq!(parse_order_id("order:abc"), 0);
        assert_eq!(parse_order_id("invalid"), 0);
        assert_eq!(parse_order_id(""), 0);
    }

    // -- parse_id_param --

    #[test]
    fn test_parse_id_param() {
        let mut conn = Conn::test();
        conn.path_params.push(("id", smallvec::smallvec![b'7']));
        assert_eq!(parse_id_param(&conn), 7);

        let mut conn2 = Conn::test();
        conn2.path_params.push(("id", smallvec::SmallVec::from_slice(b"abc")));
        assert_eq!(parse_id_param(&conn2), 0);

        let conn3 = Conn::test();
        assert_eq!(parse_id_param(&conn3), 0);
    }

    // -- ChatResponder --

    #[test]
    fn test_chat_responder_hours() {
        let r = ChatResponder;
        assert!(r.respond("What are your hours?").contains("open"));
        assert!(r.respond("when do you open").contains("open"));
        assert!(r.respond("when do you close").contains("open"));
    }

    #[test]
    fn test_chat_responder_flavors() {
        let r = ChatResponder;
        assert!(r.respond("what flavors do you have").contains("flavors"));
        assert!(r.respond("show me the menu").contains("menu"));
    }

    #[test]
    fn test_chat_responder_prices() {
        let r = ChatResponder;
        assert!(r.respond("how much does it cost").contains("$4.00"));
        assert!(r.respond("what are the prices").contains("$4.00"));
    }

    #[test]
    fn test_chat_responder_specials() {
        let r = ChatResponder;
        assert!(r.respond("any specials today?").contains("Mahalo Monday"));
        assert!(r.respond("got any deals?").contains("Mahalo Monday"));
    }

    #[test]
    fn test_chat_responder_order() {
        let r = ChatResponder;
        assert!(r.respond("how do I order").contains("/order"));
    }

    #[test]
    fn test_chat_responder_thanks() {
        let r = ChatResponder;
        assert!(r.respond("thank you!").contains("Mahalo"));
        assert!(r.respond("mahalo!").contains("Mahalo"));
    }

    #[test]
    fn test_chat_responder_greeting() {
        let r = ChatResponder;
        assert!(r.respond("hello there").contains("Aloha"));
        assert!(r.respond("hi").contains("Aloha"));
        assert!(r.respond("aloha!").contains("Aloha"));
    }

    #[test]
    fn test_chat_responder_fallback() {
        let r = ChatResponder;
        assert!(r.respond("tell me about your logo").contains("Great question"));
    }

    // -- toppings_data / specials_data --

    #[test]
    fn test_toppings_data_count() {
        let toppings = toppings_data();
        assert_eq!(toppings.len(), 6);
        assert!(toppings.iter().all(|t| t.price_cents > 0));
    }

    #[test]
    fn test_specials_data_count() {
        let specials = specials_data();
        assert_eq!(specials.len(), 3);
        assert!(specials.iter().all(|s| !s.name.is_empty()));
    }

    #[test]
    fn test_toppings_json_parses() {
        let json: serde_json::Value = serde_json::from_str(&toppings_json()).unwrap();
        let arr = json["toppings"].as_array().unwrap();
        assert_eq!(arr.len(), 6);
        assert_eq!(arr[0]["name"], "Macadamia Nuts");
    }

    #[test]
    fn test_specials_json_parses() {
        let json: serde_json::Value = serde_json::from_str(&specials_json()).unwrap();
        let arr = json["specials"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0]["name"], "Mahalo Monday");
    }

    // -- parse_order_item --

    #[test]
    fn test_parse_order_item_valid() {
        let v = serde_json::json!({"scoop_flavors": [1, 2], "vessel": "cone", "toppings": ["Hot Fudge", "Mochi Bits"]});
        let item = parse_order_item(&v).unwrap();
        assert_eq!(item.scoop_flavors, vec![1, 2]);
        assert_eq!(item.vessel, "cone");
        assert_eq!(item.toppings, vec!["Hot Fudge", "Mochi Bits"]);
    }

    #[test]
    fn test_parse_order_item_defaults_cup() {
        let v = serde_json::json!({"scoop_flavors": [3]});
        let item = parse_order_item(&v).unwrap();
        assert_eq!(item.vessel, "cup");
        assert!(item.toppings.is_empty());
    }

    #[test]
    fn test_parse_order_item_invalid_vessel_defaults_cup() {
        let v = serde_json::json!({"scoop_flavors": [1], "vessel": "banana"});
        let item = parse_order_item(&v).unwrap();
        assert_eq!(item.vessel, "cup");
    }

    #[test]
    fn test_parse_order_item_empty_scoops_returns_none() {
        let v = serde_json::json!({"scoop_flavors": [], "vessel": "cup"});
        assert!(parse_order_item(&v).is_none());
    }

    #[test]
    fn test_parse_order_item_missing_scoops_returns_none() {
        let v = serde_json::json!({"vessel": "cup"});
        assert!(parse_order_item(&v).is_none());
    }

    #[test]
    fn test_parse_order_item_truncates_at_max_scoops() {
        let v = serde_json::json!({"scoop_flavors": [1, 2, 3, 4, 5]});
        let item = parse_order_item(&v).unwrap();
        assert_eq!(item.scoop_flavors.len(), MAX_SCOOPS);
        assert_eq!(item.scoop_flavors, vec![1, 2, 3]);
    }

    #[test]
    fn test_parse_order_item_single_scoop() {
        let v = serde_json::json!({"scoop_flavors": [5], "vessel": "cone"});
        let item = parse_order_item(&v).unwrap();
        assert_eq!(item.scoop_flavors, vec![5]);
        assert_eq!(item.vessel, "cone");
    }

    // -- format_specials / format_toppings --

    #[test]
    fn test_format_specials_count_and_shape() {
        let specials = format_specials();
        assert_eq!(specials.len(), 3);
        for s in &specials {
            assert!(s["name"].is_string());
            assert!(s["description"].is_string());
            assert!(s["days"].is_string());
        }
        assert_eq!(specials[0]["name"], "Mahalo Monday");
    }

    #[test]
    fn test_format_toppings_count_and_shape() {
        let toppings = format_toppings();
        assert_eq!(toppings.len(), 6);
        for t in &toppings {
            assert!(t["name"].is_string());
            assert!(t["price"].is_string());
        }
        assert_eq!(toppings[0]["name"], "Macadamia Nuts");
        assert_eq!(toppings[0]["price"], "$0.75");
    }
}
