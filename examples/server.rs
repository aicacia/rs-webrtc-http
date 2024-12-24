use std::{convert::Infallible, env::var, sync::Arc};

use axum::{extract::Request, response::Response};
use bytes::Bytes;
use dashmap::DashMap;
use http_body_util::{BodyExt, Full};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use peer::{peer::SignalMessage, Peer, PeerOptions};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use webrtc::{
  api::{
    interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
  },
  ice_transport::ice_server::RTCIceServer,
  interceptor::registry::Registry,
  peer_connection::configuration::RTCConfiguration,
};
use webrtc_http::server::RTCListener;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
  dotenvy::dotenv()?;

  let mut m = MediaEngine::default();
  let registry = register_default_interceptors(Registry::new(), &mut m)?;

  let api = Arc::new(
    APIBuilder::new()
      .with_media_engine(m)
      .with_interceptor_registry(registry)
      .build(),
  );
  let peer_options = PeerOptions {
    connection_config: Some(RTCConfiguration {
      ice_servers: vec![RTCIceServer {
        ..Default::default()
      }],
      ..Default::default()
    }),
    ..Default::default()
  };

  let token = authenticate().await?;
  let ws_url = format!(
    "{}/server/websocket?token={}",
    var("P2P_WS_URL").expect("P2P_WS_URL not defined"),
    urlencoding::encode(&token)
  );
  let (ws, _) = tungstenite::client::connect(ws_url)?;
  let socket = Arc::new(Mutex::new(ws));

  let peers = Arc::new(DashMap::<String, Peer>::new());

  loop {
    let msg = socket.lock().await.read()?;
    if msg.is_close() {
      break;
    }
    let data = msg.into_data().to_vec();
    let json = serde_json::from_slice::<IncomingMessage>(&data)?;

    match json {
      IncomingMessage::Join { from } => {
        let mut peer_options = peer_options.clone();
        peer_options.id = Some(from.clone());
        let peer = Peer::new(api.clone(), peer_options);

        let on_signal_socket = socket.clone();
        let on_signal_from = from.clone();
        peer.on_signal(Box::new(move |data| {
          let msg_json = serde_json::to_string(&OutgoingMessage {
            to: on_signal_from.clone(),
            payload: data,
          })
          .expect("failed to serialize signal message");
          let msg = tungstenite::Message::text(msg_json);
          let pinned_socket = on_signal_socket.clone();
          Box::pin(async move {
            let mut ws = pinned_socket.lock().await;
            ws.write(msg).expect("failed to write to websocket");
            ws.flush().expect("failed to flush websocket");
          })
        }));

        let on_connect_peer = peer.clone();
        peer.on_connect(Box::new(move || {
          let data_channel = on_connect_peer
            .get_data_channel()
            .expect("failed to get data channel");

          Box::pin(async move {
            let stream = RTCListener::new(data_channel)
              .accept()
              .await
              .expect("failed to accept webrtc stream");
            let io = TokioIo::new(stream);

            let conn =
              hyper::server::conn::http1::Builder::new().serve_connection(io, service_fn(echo));

            tokio::spawn(async move {
              if let Err(err) = conn.await {
                eprintln!("error serving connection failed: {:?}", err);
              }
            });
          })
        }));

        peers.insert(from, peer);
      }
      IncomingMessage::Leave { from } => {
        println!("{} left", from);
      }
      IncomingMessage::Message { from, payload } => {
        if let Some(peer) = peers.get(&from) {
          peer.signal(payload).await?;
        }
      }
    }
  }

  Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum IncomingMessage {
  #[serde(rename = "join")]
  Join { from: String },
  #[serde(rename = "leave")]
  Leave { from: String },
  #[serde(rename = "message")]
  Message {
    from: String,
    payload: SignalMessage,
  },
}

#[derive(Serialize)]
pub struct OutgoingMessage {
  to: String,
  payload: SignalMessage,
}

#[derive(Serialize)]
pub struct AuthenticateBody {
  id: String,
  password: String,
}

async fn authenticate() -> anyhow::Result<String> {
  let body = AuthenticateBody {
    id: "some-globally-unique-id".to_owned(),
    password: "password".to_owned(),
  };
  let token = reqwest::Client::new()
    .post(format!(
      "{}/server",
      var("P2P_API_URL").expect("P2P_API_URL not defined")
    ))
    .bearer_auth(var("JWT_TOKEN").expect("JWT_TOKEN not defined"))
    .json(&body)
    .send()
    .await?
    .text()
    .await?;

  Ok(token)
}

async fn echo(req: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
  let body = match req.into_body().collect().await {
    Ok(collected) => collected.to_bytes(),
    Err(e) => {
      eprintln!("error: {}", e);
      Bytes::new()
    }
  };
  Ok(Response::new(Full::new(body)))
}
