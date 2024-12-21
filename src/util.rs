use std::{
  future::Future,
  io,
  pin::Pin,
  result::Result,
  sync::Arc,
  task::{Context, Poll},
};

use bytes::Bytes;
use tokio::{
  io::{AsyncRead, AsyncWrite, ReadBuf, ReadHalf, SimplexStream},
  sync::oneshot::Sender,
};
use webrtc::data_channel::RTCDataChannel;

pub const DEFAULT_READ_BUF_SIZE: usize = 8192;

pub struct RTCDataChannelStream {
  poll_data_channel: PollRTCDataChannel,
  receiver: ReadHalf<SimplexStream>,
  shutdown: Option<Sender<()>>,
}

unsafe impl Send for RTCDataChannelStream {}
unsafe impl Sync for RTCDataChannelStream {}

impl RTCDataChannelStream {
  pub fn new(
    poll_data_channel: PollRTCDataChannel,
    receiver: ReadHalf<SimplexStream>,
    shutdown: Sender<()>,
  ) -> Self {
    Self {
      poll_data_channel,
      receiver,
      shutdown: Some(shutdown),
    }
  }

  pub fn shutdown(&mut self) -> Result<(), ()> {
    match self.shutdown.take() {
      Some(shutdown) => shutdown.send(()),
      None => Err(()),
    }
  }
}

impl AsyncRead for RTCDataChannelStream {
  fn poll_read(
    self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &mut ReadBuf<'_>,
  ) -> Poll<io::Result<()>> {
    Pin::new(&mut self.get_mut().receiver).poll_read(cx, buf)
  }
}

impl AsyncWrite for RTCDataChannelStream {
  fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
    Pin::new(&mut self.get_mut().poll_data_channel).poll_write(cx, buf)
  }

  fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
    Pin::new(&mut self.get_mut().poll_data_channel).poll_flush(cx)
  }

  fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
    let mut_self = self.get_mut();
    match Pin::new(&mut mut_self.poll_data_channel).poll_shutdown(cx) {
      Poll::Pending => Poll::Pending,
      Poll::Ready(result) => {
        let _ = mut_self.shutdown();
        Poll::Ready(result)
      }
    }
  }
}

pub struct PollRTCDataChannel {
  stream_id_bytes: [u8; 4],
  data_channel: Arc<RTCDataChannel>,
  write_fut: Option<Pin<Box<dyn Future<Output = Result<usize, webrtc::Error>> + Send>>>,
}

impl PollRTCDataChannel {
  pub fn new(stream_id: u32, data_channel: Arc<RTCDataChannel>) -> Self {
    Self {
      stream_id_bytes: stream_id.to_be_bytes(),
      data_channel,
      write_fut: None,
    }
  }
}

impl AsyncWrite for PollRTCDataChannel {
  fn poll_write(
    mut self: Pin<&mut Self>,
    cx: &mut Context<'_>,
    buf: &[u8],
  ) -> Poll<io::Result<usize>> {
    if buf.is_empty() {
      return Poll::Ready(Ok(0));
    }

    if let Some(fut) = self.write_fut.as_mut() {
      match fut.as_mut().poll(cx) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Err(e)) => {
          let data_channel: Arc<RTCDataChannel> = self.data_channel.clone();
          let bytes: Bytes = Bytes::from_owner([&self.stream_id_bytes, buf].concat());
          self.write_fut = Some(Box::pin(async move {
            data_channel.send(&bytes).await.map(|len| len - 4)
          }));
          Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.to_string())))
        }
        Poll::Ready(Ok(_)) => {
          let data_channel = self.data_channel.clone();
          let bytes: Bytes = Bytes::from_owner([&self.stream_id_bytes, buf].concat());
          self.write_fut = Some(Box::pin(async move {
            data_channel.send(&bytes).await.map(|len| len - 4)
          }));
          Poll::Ready(Ok(buf.len()))
        }
      }
    } else {
      let data_channel = self.data_channel.clone();
      let bytes: Bytes = Bytes::from_owner([&self.stream_id_bytes, buf].concat());
      let fut = self.write_fut.insert(Box::pin(async move {
        data_channel.send(&bytes).await.map(|len| len - 4)
      }));

      match fut.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(Ok(buf.len())),
        Poll::Ready(Err(e)) => {
          self.write_fut = None;
          Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.to_string())))
        }
        Poll::Ready(Ok(n)) => {
          self.write_fut = None;
          Poll::Ready(Ok(n))
        }
      }
    }
  }

  fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    match self.write_fut.as_mut() {
      Some(fut) => match fut.as_mut().poll(cx) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(Err(e)) => {
          self.write_fut = None;
          Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e.to_string())))
        }
        Poll::Ready(Ok(_)) => {
          self.write_fut = None;
          Poll::Ready(Ok(()))
        }
      },
      None => Poll::Ready(Ok(())),
    }
  }

  fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
    match self.as_mut().poll_flush(cx) {
      Poll::Pending => return Poll::Pending,
      Poll::Ready(_) => Poll::Ready(Ok(())),
    }
  }
}
