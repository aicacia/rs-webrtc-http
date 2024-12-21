use std::{io, sync::Arc};

use dashmap::DashMap;
use tokio::{
  io::{simplex, AsyncWriteExt},
  runtime::Handle,
  sync::{
    mpsc::{self, Receiver},
    oneshot::channel,
    Mutex,
  },
  task::block_in_place,
};
use webrtc::data_channel::{data_channel_message::DataChannelMessage, RTCDataChannel};

use crate::util::{PollRTCDataChannel, RTCDataChannelStream, DEFAULT_READ_BUF_SIZE};

#[derive(Clone)]
pub struct RTCListener {
  receiver: Arc<Mutex<Receiver<RTCDataChannelStream>>>,
}

unsafe impl Send for RTCListener {}
unsafe impl Sync for RTCListener {}

impl RTCListener {
  pub fn new(data_channel: Arc<RTCDataChannel>) -> Self {
    let (stream_sender, stream_receiver) = mpsc::channel(1024);
    let stream_receiver = Arc::new(Mutex::new(stream_receiver));
    let streams = Arc::new(DashMap::new());

    let on_message_streams = streams.clone();
    let on_message_data_channel = data_channel.clone();

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
      let pinned_stream_sender = stream_sender.clone();
      let pinned_data_channel = on_message_data_channel.clone();

      Box::pin(async move {
        let remove_streams = pinned_streams.clone();

        let mut stream_sender = pinned_streams.entry(stream_id).or_insert_with(move || {
          // TODO get max message size of transports
          let (receiver, sender) = simplex(DEFAULT_READ_BUF_SIZE);
          let (shutdown_sender, shutdown_receiver) = channel();

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

          match block_in_place(move || {
            let poll_rtc_data_channel = PollRTCDataChannel::new(stream_id, pinned_data_channel);
            let rtc_data_channel_stream =
              RTCDataChannelStream::new(poll_rtc_data_channel, receiver, shutdown_sender);
            Handle::current().block_on(pinned_stream_sender.send(rtc_data_channel_stream))
          }) {
            Ok(_) => {}
            Err(e) => {
              panic!("error sending stream to accept: {}", e);
            }
          }

          sender
        });

        match stream_sender.value_mut().write_all(msg.data.as_ref()).await {
          Ok(_) => {}
          Err(e) => {
            panic!("error writing to stream: {}", e);
          }
        }
      })
    }));

    let on_close_stream_sender = stream_receiver.clone();

    data_channel.on_close(Box::new(move || {
      let pinned_stream_sender = on_close_stream_sender.clone();

      Box::pin(async move {
        pinned_stream_sender.lock().await.close();
      })
    }));

    Self {
      receiver: stream_receiver,
    }
  }

  pub async fn accept(&self) -> io::Result<RTCDataChannelStream> {
    match self.receiver.lock().await.recv().await {
      Some(stream) => Ok(stream),
      None => Err(io::Error::new(
        io::ErrorKind::Other,
        "accept called on closed RTCListener",
      )),
    }
  }

  pub async fn close(&self) {
    self.receiver.lock().await.close();
  }
}

#[cfg(feature = "axum")]
impl axum::serve::Listener for RTCListener {
  type Addr = ();
  type Io = RTCDataChannelStream;

  async fn accept(&mut self) -> (Self::Io, Self::Addr) {
    loop {
      match Self::accept(self).await {
        Ok(stream) => return (stream, ()),
        Err(e) => {
          eprintln!("error accepting stream: {}", e);
          tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
      }
    }
  }

  fn local_addr(&self) -> io::Result<Self::Addr> {
    Ok(())
  }
}
