use std::{convert::Infallible, sync::Arc};

use bytes::Bytes;
use http_body_util::{Empty, Full};
use hyper::{service::service_fn, Request, Response};
use hyper_util::rt::TokioIo;
use tokio::sync::Mutex;
use webrtc::{
  api::{
    interceptor_registry::register_default_interceptors, media_engine::MediaEngine, APIBuilder,
  },
  data_channel::RTCDataChannel,
  ice_transport::{
    ice_candidate::{RTCIceCandidate, RTCIceCandidateInit},
    ice_server::RTCIceServer,
  },
  interceptor::registry::Registry,
  peer_connection::{
    configuration::RTCConfiguration, peer_connection_state::RTCPeerConnectionState,
  },
};

use webrtc_http::{client::RTCClient, server::RTCListener};

#[tokio::main]
async fn main() -> Result<(), webrtc::Error> {
  let mut m = MediaEngine::default();
  let registry = register_default_interceptors(Registry::new(), &mut m)?;

  let api = APIBuilder::new()
    .with_media_engine(m)
    .with_interceptor_registry(registry)
    .build();

  let config = RTCConfiguration {
    ice_servers: vec![RTCIceServer {
      ..Default::default()
    }],
    ..Default::default()
  };

  let peer0 = Arc::new(api.new_peer_connection(config.clone()).await?);
  let peer1 = Arc::new(api.new_peer_connection(config).await?);

  peer0.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
    if s == RTCPeerConnectionState::Failed {
      assert!(false, "peer0 connection has gone to failed exiting");
    }
    println!("peer0 connection state: {s}");
    Box::pin(async {})
  }));

  peer1.on_peer_connection_state_change(Box::new(move |s: RTCPeerConnectionState| {
    if s == RTCPeerConnectionState::Failed {
      assert!(false, "peer1 connection has gone to failed exiting");
    }
    println!("peer1 connection state: {s}");
    Box::pin(async {})
  }));

  let peer0_candidates = Arc::new(Mutex::new(Vec::<RTCIceCandidateInit>::new()));
  let peer1_candidates = Arc::new(Mutex::new(Vec::<RTCIceCandidateInit>::new()));

  let on_ice_candidate_peer1_candidates = peer1_candidates.clone();
  let on_ice_candidate_peer1 = peer1.clone();

  peer0.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
    let pinned_peer1 = on_ice_candidate_peer1.clone();
    let pinned_peer1_candidates = on_ice_candidate_peer1_candidates.clone();

    Box::pin(async move {
      if let Some(candidate) = candidate {
        if pinned_peer1.remote_description().await.is_some() {
          {
            let mut candidates = pinned_peer1_candidates.lock().await;
            for c in candidates.iter() {
              pinned_peer1
                .add_ice_candidate(c.clone())
                .await
                .expect("failed to add ice candidate");
            }
            candidates.clear();
          }
          pinned_peer1
            .add_ice_candidate(
              candidate
                .to_json()
                .expect("failed to convert candidate to json"),
            )
            .await
            .expect("failed to add ice candidate");
        } else {
          pinned_peer1_candidates.lock().await.push(
            candidate
              .to_json()
              .expect("failed to convert candidate to json"),
          );
        }
      }
    })
  }));

  let on_ice_candidate_peer0_candidates = peer0_candidates.clone();
  let on_ice_candidate_peer0 = peer0.clone();

  peer1.on_ice_candidate(Box::new(move |candidate: Option<RTCIceCandidate>| {
    let pinned_peer0 = on_ice_candidate_peer0.clone();
    let pinned_peer0_candidates = on_ice_candidate_peer0_candidates.clone();

    Box::pin(async move {
      if let Some(candidate) = candidate {
        if pinned_peer0.remote_description().await.is_some() {
          {
            let mut candidates = pinned_peer0_candidates.lock().await;
            for c in candidates.iter() {
              pinned_peer0
                .add_ice_candidate(c.clone())
                .await
                .expect("failed to add ice candidate");
            }
            candidates.clear();
          }
          pinned_peer0
            .add_ice_candidate(
              candidate
                .to_json()
                .expect("failed to convert candidate to json"),
            )
            .await
            .expect("failed to add ice candidate");
        } else {
          pinned_peer0_candidates.lock().await.push(
            candidate
              .to_json()
              .expect("failed to convert candidate to json"),
          );
        }
      }
    })
  }));

  let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<RTCClient>(1);

  peer1.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
    let pinned_client_tx = client_tx.clone();

    println!("peer1 data channel has opened");

    Box::pin(async move {
      pinned_client_tx
        .send(RTCClient::new(data_channel))
        .await
        .expect("failed to send client")
    })
  }));

  let data_channel = peer0.create_data_channel("data", None).await?;
  let (listener_tx, mut listener_rx) = tokio::sync::mpsc::channel::<RTCListener>(1);
  let on_open_data_channel = Arc::clone(&data_channel);

  data_channel.on_open(Box::new(move || {
    println!("peer0 data channel has opened");
    Box::pin(async move {
      listener_tx
        .send(RTCListener::new(on_open_data_channel))
        .await
        .expect("failed to send listener")
    })
  }));

  let offer = peer0.create_offer(None).await?;
  let mut peer0_gather_complete = peer0.gathering_complete_promise().await;
  peer0.set_local_description(offer.clone()).await?;

  let _ = peer0_gather_complete.recv().await;

  peer1.set_remote_description(offer).await?;

  let answer = peer1.create_answer(None).await?;
  let mut peer1_gather_complete = peer1.gathering_complete_promise().await;
  peer1.set_local_description(answer.clone()).await?;

  peer0.set_remote_description(answer).await?;

  let _ = peer1_gather_complete.recv().await;

  let listener = listener_rx.recv().await.expect("no listener received");

  let client_handle = tokio::spawn(async move {
    if let Some(client) = client_rx.recv().await {
      let stream = client.connect();
      let io = TokioIo::new(stream);
      let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .expect("handshake failed");

      tokio::task::spawn(async move {
        if let Err(err) = conn.await {
          println!("connection failed: {:?}", err);
        }
      });

      let req = Request::builder()
        .body(Empty::<Bytes>::new())
        .expect("failed to build client request");

      let res = sender
        .send_request(req)
        .await
        .expect("failed to send request");

      assert_eq!(res.status(), 200);
      println!("response {:?}", res);
    } else {
      assert!(false, "no client received");
    }
  });

  let graceful = hyper_util::server::graceful::GracefulShutdown::new();

  let stream = listener
    .accept()
    .await
    .expect("failed to accept webrtc stream");
  let io = TokioIo::new(stream);

  let conn = hyper::server::conn::http1::Builder::new().serve_connection(io, service_fn(hello));
  let graceful_conn = graceful.watch(conn);

  tokio::spawn(async move {
    if let Err(err) = graceful_conn.await {
      eprintln!("error serving connection failed: {:?}", err);
    }
  });

  tokio::select! {
    result = client_handle => {
      match result {
        Ok(_) => {},
        Err(err) => {
          println!("client handle failed: {:?}", err);
        }
      }
    }
    _ = tokio::signal::ctrl_c() => {
      println!("ctrl+c");
    }
  };

  graceful.shutdown().await;

  peer0.close().await?;
  peer1.close().await?;

  Ok(())
}

async fn hello(_: Request<hyper::body::Incoming>) -> Result<Response<Full<Bytes>>, Infallible> {
  Ok(Response::new(Full::new(Bytes::from("Hello, World!"))))
}
