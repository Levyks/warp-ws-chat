use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use futures_util::stream::SplitStream;
use tokio::sync::{mpsc, RwLock};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::UnboundedReceiverStream;
use warp::Filter;
use warp::ws::{Message, WebSocket};

type Id = String;

struct Room {
    id: Id,
    users: HashMap<usize, UnboundedSender<Message>>,
}

impl Room {
    fn new(id: Id) -> Self {
        Self {
            id,
            users: HashMap::new(),
        }
    }
}

type WrappedRoom = Arc<RwLock<Room>>;
type State = Arc<DashMap<Id, WrappedRoom>>;

static NEXT_USER_ID: AtomicUsize = AtomicUsize::new(1);

#[tokio::main]
async fn main() {
    let state = State::default();
    let with_state = warp::any().map(move || state.clone());

    let hello = warp::path!("room" / Id)
        .and(warp::ws())
        .and(with_state)
        .map(|id: Id, ws: warp::ws::Ws, state: State| {
            let room = get_or_create(&state, id.clone(), || Arc::new(RwLock::new(Room::new(id))));
            ws.on_upgrade(move |socket| handle_ws(socket, room))
        });

    warp::serve(hello)
        .run(([127, 0, 0, 1], 3030))
        .await;
}

fn get_or_create<K, V>(map: &DashMap<K, V>, key: K, value_factory: impl FnOnce() -> V) -> V
    where
        K: Eq + std::hash::Hash + Clone,
        V: Clone,
{
    let value = map.get(&key);
    match value {
        Some(value) => value.clone(),
        None => {
            let value = value_factory();
            map.insert(key, value.clone());
            value
        }
    }
}

fn split_and_spawn_flusher(ws: WebSocket) -> (UnboundedSender<Message>, SplitStream<WebSocket>) {
    let (mut user_ws_tx, user_ws_rx) = ws.split();
    let (tx, rx) = mpsc::unbounded_channel::<Message>();
    let mut rx = UnboundedReceiverStream::new(rx);

    tokio::task::spawn(async move {
        while let Some(message) = rx.next().await {
            user_ws_tx
                .send(message)
                .await
                .unwrap_or_else(|e| {
                    eprintln!("websocket send error: {}", e);
                })
        }
    });

    (tx, user_ws_rx)
}

async fn handle_ws(ws: WebSocket, room: WrappedRoom) {
    let room_id = room.read().await.id.clone();
    let user_id = NEXT_USER_ID.fetch_add(1, Ordering::Relaxed);

    let (tx, mut rx) = split_and_spawn_flusher(ws);

    room.write().await.users.insert(user_id, tx);
    println!("user {} connected to room {}", user_id, room_id);

    while let Some(result) = rx.next().await {
        let msg = match result {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("websocket error(uid={}): {}", user_id, e);
                break;
            }
        };
        handle_message(user_id, msg, room.clone()).await;
    }

    room.write().await.users.remove(&user_id);
    println!("user {} disconnected from room {}", user_id, room_id);
}

async fn handle_message(user_id: usize, msg: Message, room: WrappedRoom) {
    // Skip any non-Text messages...
    let msg = if let Ok(s) = msg.to_str() {
        s
    } else {
        return;
    };

    let new_msg = format!("<User#{}>: {}", user_id, msg);

    for (&id, tx) in room.read().await.users.iter() {
        if user_id != id {
            let _ = tx.send(Message::text(new_msg.clone()));
        }
    }
}