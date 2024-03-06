#![allow(unused_variables)]
#![allow(unused_imports)]
use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    hash::Hash,
    ops::Deref,
    sync::{Arc, Mutex},
};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
    Form, Json, Router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{
    broadcast::{self, Receiver},
    mpsc::{channel, unbounded_channel, Sender, UnboundedSender},
    Mutex as TokioMutex,
};
use tokio::task::JoinHandle;

type AppId = String;
type ChannelsId = String;

#[derive(Debug, Default, Clone)]
struct Apps(HashMap<AppId, Channels>);

#[derive(Debug, Default, Clone)]
struct Channels(HashMap<ChannelsId, ChannelData>);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Event {
    data: Arc<str>,
}

type MoveType = Arc<[Event]>;
#[derive(Debug, Clone)]
struct ChannelData {
    pub events: Arc<Mutex<Vec<Event>>>,
    pub sender: Sender<MoveType>,
    cloner: broadcast::Sender<MoveType>,
}

impl ChannelData {
    pub fn new() -> Self {
        let (cloner, _) = broadcast::channel::<MoveType>(512);
        let (tx, mut rx) = channel::<MoveType>(512);
        let events: Arc<Mutex<Vec<Event>>> = Default::default();

        let send_to_subscribers = cloner.clone();
        let events_clone = events.clone();
        tokio::spawn(async move {
            while let Some(new_events) = rx.recv().await {
                let mut vec = events_clone.lock().unwrap();
                vec.extend(new_events.clone().into_iter().cloned());
                // We don't care if it fails: this just means no subscribers yet
                _ = send_to_subscribers.send(new_events);
            }
        });

        Self {
            events,
            sender: tx,
            cloner,
        }
    }

    fn rx(&self) -> Receiver<Arc<[Event]>> {
        self.cloner.subscribe()
    }
}

#[derive(Clone, Default)]
struct AppState {
    apps: Arc<Mutex<Apps>>,
    users: Arc<Mutex<HashMap<Arc<str>, User>>>,
}

#[derive(Debug, Default)]
struct WebSocket;

impl WebSocket {
    async fn send(&mut self, data: impl Serialize) -> Result<(), ()> {
        Ok(())
    }
    async fn close(&mut self) {}
}

#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq, Hash)]
struct UserData {
    name: Arc<str>,
}

// Don't forget to call drop!
#[derive(Debug, Clone)]
struct User {
    pub data: UserData,
    socket: Arc<TokioMutex<WebSocket>>,
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl User {
    async fn drop_async(self) {
        self.socket.lock().await.close().await;
    }
}

impl Drop for User {
    fn drop(&mut self) {
        for h in self.handles.lock().unwrap().iter_mut() {
            h.abort();
        }
    }
}

async fn create_app(
    State(AppState { apps, .. }): State<AppState>,
    Path((app)): Path<(String)>,
) -> impl IntoResponse {
    match apps.lock().unwrap().0.entry(app) {
        Entry::Occupied(_) => StatusCode::CONFLICT,
        Entry::Vacant(e) => {
            e.insert(Channels::default());
            StatusCode::OK
        }
    }
}
async fn delete_app(
    State(AppState { apps, .. }): State<AppState>,
    Path((app)): Path<(String)>,
) -> impl IntoResponse {
    match apps.lock().unwrap().0.entry(app) {
        Entry::Occupied(_) => StatusCode::CONFLICT,
        Entry::Vacant(e) => {
            e.insert(Channels::default());
            StatusCode::OK
        }
    }
}

async fn create_channel(
    State(AppState { apps, .. }): State<AppState>,
    Path((app, channel)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(ch) = apps.lock().unwrap().0.get_mut(&app) {
        match ch.0.entry(channel) {
            Entry::Vacant(_) => StatusCode::NOT_FOUND,
            Entry::Occupied(e) => {
                e.remove();
                StatusCode::OK
            }
        }
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn delete_channel(
    State(AppState { apps, .. }): State<AppState>,
    Path((app, channel)): Path<(String, String)>,
) -> impl IntoResponse {
    if let Some(ch) = apps.lock().unwrap().0.get_mut(&app) {
        match ch.0.entry(channel) {
            Entry::Vacant(_) => StatusCode::NOT_FOUND,
            Entry::Occupied(e) => {
                e.remove();
                StatusCode::OK
            }
        }
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn get_events(
    State(AppState { apps, .. }): State<AppState>,
    Path((app, channel)): Path<(String, String)>,
) -> Response {
    if let Some(ch) = apps.lock().unwrap().0.get(&app) {
        if let Some(data) = ch.0.get(&channel) {
            let vec = data.events.lock().unwrap();
            return serde_json::to_vec_pretty(vec.as_slice())
                .map_err(|e| e.to_string())
                .into_response();
        }
    }

    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn post_events(
    State(AppState { apps, .. }): State<AppState>,
    Path((app, channel)): Path<(String, String)>,
    Json(events): Json<MoveType>,
) -> Response {
    if let Some(ch) = apps.lock().unwrap().0.get(&app) {
        if let Some(data) = ch.0.get(&channel) {
            let vec = data.sender.send(events);
        }
    }

    axum::http::StatusCode::NOT_FOUND.into_response()
}

async fn delete_user(
    State(AppState { users, .. }): State<AppState>,
    Path(app): Path<String>,
    Form(username): Form<Arc<str>>,
) -> impl IntoResponse {
    let Some(user) = users.lock().unwrap().remove(&username) else {
        return StatusCode::NOT_FOUND;
    };

    user.drop_async().await;
    StatusCode::OK
}

async fn create_user(
    State(AppState { users, .. }): State<AppState>,
    Path(app): Path<String>,
    Form(data): Form<UserData>,
) -> impl IntoResponse {
    let user = User {
        data,
        socket: Default::default(), // TODO!
        handles: Default::default(),
    };

    users.lock().unwrap().insert(user.data.name.clone(), user); // Obviously bad, TODO

    StatusCode::OK
}

async fn subscribe_to_channel(
    State(AppState {
        apps,
        users,
    }): State<AppState>,
    Path((app, channel)): Path<(String, String)>,
    Form(username): Form<Arc<str>>,
) -> impl IntoResponse {
    if !users.lock().unwrap().contains_key(&username) {
        return StatusCode::NOT_FOUND;
    }

    if let Some(ch) = apps.lock().unwrap().0.get(&app) {
        if let Some(data) = ch.0.get(&channel) {
            let mut rx = data.rx();
            let task = tokio::spawn({
                let users = users.clone();
                let username = username.clone();
                async move {
                    loop {
                        if let Ok(events) = rx.recv().await {
                            let socket = {
                                let mut users = users.lock().unwrap();
                                let user = users.get_mut(&username).expect("why is there no key?");
                                user.socket.clone()
                            };

                            socket
                                .lock()
                                .await
                                .send(events)
                                .await
                                .expect("Handle this error!");
                        }
                    }
                }
            });

            let mut user = users.lock().unwrap();
            let user = user.get_mut(&username).expect("why is there no key?");
            user.handles.lock().unwrap().push(task);
        }
    }

    StatusCode::OK
}

#[shuttle_runtime::main]
async fn main() -> shuttle_axum::ShuttleAxum {
    let router = Router::new()
        .route("/:app/create", put(create_app))
        .route("/:app/create", delete(delete_app))
        .route("/:app/:channel/create", put(create_channel))
        .route("/:app/:channel/create", delete(delete_channel))
        .route("/:app/:channel/events", post(post_events))
        .route("/:app/user", put(create_user))
        .route("/:app/user", delete(delete_user))
        .route("/:app/:channel/subscribe", put(subscribe_to_channel))
        .with_state(AppState::default());

    Ok(router.into())
}
