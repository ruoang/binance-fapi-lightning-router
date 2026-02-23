use std::env;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};
use tokio::time::{sleep, Duration};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use futures_util::{StreamExt, SinkExt};
use dashmap::DashMap;
use serde::Deserialize;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use url::Url;
use tracing::{info, error, Level};
use tracing_subscriber::FmtSubscriber;

type HmacSha256 = Hmac<Sha256>;

// ==========================================
// 1. Ultra-low Latency Execution Router
// ==========================================
// Core Feature: Bypasses high-latency REST APIs by utilizing the Binance WebSocket API 
// for order placement and cancellation. 
// Internally uses DashMap and oneshot channels to achieve nanosecond-level 
// asynchronous request-callback matching.

#[derive(Debug)]
pub enum WsOrderCommand {
    PlaceOrder {
        symbol: String,
        side: String,
        price: f64,
        qty: f64,
        reply: oneshot::Sender<Option<u64>>, // Asynchronously returns the order ID
    },
    CancelOrder {
        symbol: String,
        order_id: u64,
        reply: oneshot::Sender<bool>,        // Asynchronously returns the cancellation result
    }
}

pub struct ExecutionGateway {
    api_key: String,
    secret_key: String,
}

impl ExecutionGateway {
    pub fn new(api_key: String, secret_key: String) -> Self {
        Self { api_key, secret_key }
    }

    pub async fn run_router(self, mut cmd_rx: mpsc::Receiver<WsOrderCommand>) {
        let url = "wss://ws-fapi.binance.com/ws-fapi/v1";
        
        // Zero-contention concurrent hash map, used to pass callbacks between I/O and strategy threads
        let active_reqs = Arc::new(DashMap::<String, oneshot::Sender<Option<u64>>>::new());
        let active_cancels = Arc::new(DashMap::<String, oneshot::Sender<bool>>::new());

        loop {
            info!("Attempting to connect to Execution WS...");
            match connect_async(Url::parse(url).unwrap()).await {
                Ok((ws_stream, _)) => {
                    info!("Execution WS Connected Successfully");
                    let (mut write, mut read) = ws_stream.split();
                    let (inner_tx, mut inner_rx) = mpsc::channel::<String>(1000);

                    let reqs_reader = active_reqs.clone();
                    let cancels_reader = active_cancels.clone();
                    
                    // Independent high-priority read thread, dedicated solely to parsing ACKs from the matching engine
                    let reader_handle = tokio::spawn(async move {
                        while let Some(msg) = read.next().await {
                            if let Ok(Message::Text(text)) = msg {
                                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                                    if let Some(id_str) = val["id"].as_str() {
                                        // O(1) extraction of the callback transmitter
                                        if let Some((_, sender)) = reqs_reader.remove(id_str) {
                                            if let Some(res) = val["result"].as_object() {
                                                let _ = sender.send(res["orderId"].as_u64());
                                            } else {
                                                let _ = sender.send(None);
                                            }
                                        } else if let Some((_, sender)) = cancels_reader.remove(id_str) {
                                            let success = val["status"].as_i64().unwrap_or(0) == 200; 
                                            let _ = sender.send(success);
                                        }
                                    }
                                }
                            }
                        }
                    });

                    // Instruction multiplexing loop
                    loop {
                        tokio::select! {
                            // Receives internally signed payloads and sends them to the network
                            Some(msg) = inner_rx.recv() => {
                                if write.send(Message::Text(msg)).await.is_err() { break; }
                            }
                            // Receives order/cancel instructions from the strategy engine and performs nanosecond-level local signing
                            Some(cmd) = cmd_rx.recv() => {
                                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
                                match cmd {
                                    WsOrderCommand::PlaceOrder { symbol, side, price, qty, reply } => {
                                        let mut params = vec![
                                            ("apiKey", self.api_key.clone()),
                                            ("symbol", symbol),
                                            ("side", side),
                                            ("type", "LIMIT".to_string()),
                                            ("timeInForce", "GTX".to_string()), // Post-only (Maker)
                                            ("quantity", format!("{:.3}", qty)),
                                            ("price", format!("{:.2}", price)),
                                            ("timestamp", now.to_string()),
                                        ];
                                        
                                        let req_id = format!("ord_{}", now);
                                        let payload = Self::build_signed_payload(&mut params, &self.secret_key, &req_id, "order.place");
                                        
                                        active_reqs.insert(req_id, reply);
                                        let _ = inner_tx.send(payload).await;
                                    },
                                    WsOrderCommand::CancelOrder { symbol, order_id, reply } => {
                                        let mut params = vec![
                                            ("apiKey", self.api_key.clone()),
                                            ("symbol", symbol),
                                            ("orderId", order_id.to_string()),
                                            ("timestamp", now.to_string()),
                                        ];

                                        let req_id = format!("cxl_{}", now);
                                        let payload = Self::build_signed_payload(&mut params, &self.secret_key, &req_id, "order.cancel");
                                        
                                        active_cancels.insert(req_id, reply);
                                        let _ = inner_tx.send(payload).await;
                                    }
                                }
                            }
                        }
                    }
                    reader_handle.abort();
                }
                Err(e) => {
                    error!("Execution WS Connection Failed: {:?}", e);
                    sleep(Duration::from_millis(1000)).await;
                }
            }
        }
    }

    #[inline(always)]
    fn build_signed_payload(params: &mut Vec<(&str, String)>, secret: &str, req_id: &str, method: &str) -> String {
        params.sort_by(|a, b| a.0.cmp(b.0));
        let query = params.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join("&");
        
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
        mac.update(query.as_bytes());
        let signature = hex::encode(mac.finalize().into_bytes());

        let mut json_params = serde_json::Map::new();
        for (k, v) in params.drain(..) { 
            json_params.insert(k.to_string(), serde_json::Value::String(v)); 
        }
        json_params.insert("signature".to_string(), serde_json::Value::String(signature));
        
        serde_json::json!({
            "id": req_id,
            "method": method,
            "params": json_params
        }).to_string()
    }
}

// ==========================================
// 2. Market Data Streamer
// ==========================================

#[derive(Deserialize)]
struct StreamMessage { stream: String, data: serde_json::Value }

#[derive(Deserialize)]
struct BookTickerEvent {
    b: String, // best bid price
    B: String, // best bid qty
    a: String, // best ask price
    A: String, // best ask qty
}

pub async fn run_market_stream(symbol: String) {
    let ws_base = "fstream.binance.com";
    let streams = format!("{}@bookTicker", symbol.to_lowercase());
    let connect_addr = format!("wss://{}/stream?streams={}", ws_base, streams);

    loop {
        match connect_async(Url::parse(&connect_addr).unwrap()).await {
            Ok((ws_stream, _)) => {
                info!("Market Data WS Connected");
                let (_, mut read) = ws_stream.split();
                
                while let Some(message) = read.next().await {
                    if let Ok(Message::Text(text)) = message {
                        if let Ok(parsed) = serde_json::from_str::<StreamMessage>(&text) {
                            if parsed.stream.contains("bookTicker") {
                                if let Ok(_ticker) = serde_json::from_value::<BookTickerEvent>(parsed.data) {
                                    // Reserved Interface: Send cleaned Ticks to the strategy engine (via ring buffer or lock-free queue)
                                    // e.g., tx.send(BboUpdate { bid: _ticker.b, ask: _ticker.a });
                                }
                            }
                        }
                    }
                }
            }
            Err(_) => sleep(Duration::from_millis(500)).await,
        }
    }
}

// ==========================================
// 3. Application Entry Point & Component Assembly
// ==========================================

#[tokio::main]
async fn main() {
    let subscriber = FmtSubscriber::builder().with_max_level(Level::INFO).finish();
    tracing::subscriber::set_global_default(subscriber).expect("setting default subscriber failed");

    let api_key = env::var("BINANCE_API_KEY").unwrap_or_default();
    let secret_key = env::var("BINANCE_SECRET_KEY").unwrap_or_default();

    let (order_tx, order_rx) = mpsc::channel::<WsOrderCommand>(1024);

    // Start the independent high-frequency order routing service
    let gateway = ExecutionGateway::new(api_key, secret_key);
    tokio::spawn(async move {
        gateway.run_router(order_rx).await;
    });

    // Start the market data gateway
    tokio::spawn(async move {
        run_market_stream("ETHUSDT".to_string()).await;
    });

    info!("Lightning Router Infrastructure Started.");
    
    // Simulate order placement from the strategy engine (for demonstration purposes)
    sleep(Duration::from_secs(3)).await;
    let (reply_tx, reply_rx) = oneshot::channel();
    let _ = order_tx.send(WsOrderCommand::PlaceOrder {
        symbol: "ETHUSDT".to_string(),
        side: "BUY".to_string(),
        price: 1500.00,
        qty: 0.01,
        reply: reply_tx,
    }).await;

    // Asynchronously wait for nanosecond-level confirmation from the matching engine
    if let Ok(Some(order_id)) = reply_rx.await {
        info!("Order placed successfully with ID: {}", order_id);
    }

    // Keep the main thread alive
    tokio::signal::ctrl_c().await.unwrap();
    info!("Shutting down...");
}
