use std::sync::Arc;

use dashmap::DashMap;
use tokio::{
  io::{simplex, AsyncWriteExt, SimplexStream, WriteHalf},
  sync::oneshot::channel,
};
use webrtc::data_channel::{data_channel_message::DataChannelMessage, RTCDataChannel};

use crate::util::{PollRTCDataChannel, RTCDataChannelStream, DEFAULT_READ_BUF_SIZE};

#[derive(Clone)]
pub struct RTCClient {
  streams: Arc<DashMap<u32, WriteHalf<SimplexStream>>>,
  data_channel: Arc<RTCDataChannel>,
}

unsafe impl Send for RTCClient {}
unsafe impl Sync for RTCClient {}

impl RTCClient {
  pub fn new(data_channel: Arc<RTCDataChannel>) -> Self {
    let streams: Arc<DashMap<u32, WriteHalf<SimplexStream>>> = Arc::new(DashMap::new());

    let on_message_streams = streams.clone();

    data_channel.on_message(Box::new(move |mut msg: DataChannelMessage| {
      if msg.data.len() < 4 {
        eprintln!("received message with less than 4 bytes");
        return Box::pin(async move {});
      }
      let stream_id_bytes = msg.data.split_to(4);
      let mut stream_id_byte_slice: [u8; 4] = [0u8; 4];
      stream_id_byte_slice.clone_from_slice(stream_id_bytes.as_ref());
      let stream_id = u32::from_be_bytes(stream_id_byte_slice);

      let pinned_streams = on_message_streams.clone();

      Box::pin(async move {
        if let Some(mut stream_sender) = pinned_streams.get_mut(&stream_id) {
          match stream_sender.value_mut().write_all(msg.data.as_ref()).await {
            Ok(_) => {}
            Err(e) => {
              panic!("error writing to stream: {}", e);
            }
          }
        } else {
          eprintln!("received message for unknown stream");
        }
      })
    }));

    data_channel.on_close(Box::new(move || {
      Box::pin(async move {
        // TODO: do not allow any new connections after this
      })
    }));

    Self {
      streams,
      data_channel,
    }
  }

  pub fn connect(&self) -> RTCDataChannelStream {
    let stream_id = rand::random::<u32>();
    let (receiver, sender) = simplex(DEFAULT_READ_BUF_SIZE);
    let (shutdown_sender, shutdown_receiver) = channel();

    let remove_streams = self.streams.clone();
    tokio::task::spawn(async move {
      match shutdown_receiver.await {
        Ok(_) => {
          remove_streams.remove(&stream_id);
        }
        Err(e) => {
          eprintln!("error receiving shutdown: {}", e);
        }
      }
    });

    let poll_data_channel = PollRTCDataChannel::new(stream_id, self.data_channel.clone());
    let stream = RTCDataChannelStream::new(poll_data_channel, receiver, shutdown_sender);
    self.streams.insert(stream_id, sender);

    stream
  }
}
