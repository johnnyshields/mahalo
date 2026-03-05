# 🍦 Mahalo Ice Cream Store

A full-featured demo application built with the [Mahalo](../../) web framework for Rust. It simulates a tropical ice cream shop and exercises every major framework feature in a single, runnable app.

## Quick Start

```bash
cargo run -p mahalo-test-app
```

Then open **http://127.0.0.1:4000** in your browser.

## What's Inside

### Pages

| Route | Description |
|-------|-------------|
| `/` | Home page with featured flavors |
| `/menu` | Full menu with toppings and specials |
| `/order` | Place an order |
| `/orders/:id` | Real-time order status tracker |
| `/flavors/:id` | Individual flavor detail |
| `/about` | Store info, hours, and support chat |

### API Endpoints

| Route | Auth | Description |
|-------|------|-------------|
| `GET /api/menu` | No | In-stock flavors (with telemetry span) |
| `GET /api/hours` | No | Store hours |
| `GET /api/about` | No | Store info |
| `GET /api/toppings` | No | Available toppings |
| `GET /api/specials` | No | Daily specials |
| `GET /api/flavors` | No | Full CRUD via `resources()` |
| `* /api/orders` | Yes | Order CRUD (requires `x-api-key` header) |
| `GET /health` | No | Health check |

### WebSocket Channels

Connect at `ws://127.0.0.1:4000/ws` using the Phoenix wire protocol:

- **`order:*`** - Join with a customer name, track order status updates, add items
- **`store:lobby`** - Store-wide announcements and live price updates
- **`support:*`** - Chat with the support bot (keyword-based responses)

### Framework Features Demonstrated

- **Pipelines & Plugs** - `browser`, `api`, and `auth` pipelines
- **Controllers** - `FlavorController` and `OrderController` via `resources()`
- **Typed Assigns** - `RequestId`, `CurrentUser`, `CustomerName`
- **WebSocket Channels** - Three channel implementations with Phoenix-compatible protocol
- **PubSub** - Broadcasting order events and price updates
- **Telemetry** - Custom events and spans for menu builds
- **Static Files** - CSS served from `/static`
- **Tera Templates** - HTML pages with shared base layout
- **Error Handler** - Custom HTML error pages
- **Real-time Prices** - Background task fluctuates prices every 15 seconds and broadcasts via PubSub

## Try It Out

```bash
# Browse the menu
curl http://127.0.0.1:4000/api/menu | jq

# Check toppings
curl http://127.0.0.1:4000/api/toppings | jq

# Place an order (auth required)
curl -X POST http://127.0.0.1:4000/api/orders \
  -H "x-api-key: any-key" \
  -H "content-type: application/json" \
  -d '{"customer_name": "Kai", "items": [{"flavor_id": 1, "scoops": 2, "topping": "Macadamia Nuts"}]}'
```
