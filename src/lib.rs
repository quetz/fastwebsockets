// Copyright 2023 Divy Srivastava <dj.srivastava23@gmail.com>
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! _fastwebsockets_ is a minimal, fast WebSocket server implementation.
//!
//! [https://github.com/littledivy/fastwebsockets](https://github.com/littledivy/fastwebsockets)
//!
//! Passes the _Autobahn|TestSuite_ and fuzzed with LLVM's _libfuzzer_.
//!
//! You can use it as a raw websocket frame parser and deal with spec compliance yourself, or you can use it as a full-fledged websocket server.
//!
//! # Example
//!
//! ```
//! use tokio::net::TcpStream;
//! use fastwebsockets::{WebSocket, OpCode, Role};
//! use anyhow::Result;
//!
//! async fn handle(
//!   socket: TcpStream,
//! ) -> Result<()> {
//!   let mut ws = WebSocket::after_handshake(socket, Role::Server);
//!   ws.set_writev(false);
//!   ws.set_auto_close(true);
//!   ws.set_auto_pong(true);
//!
//!   loop {
//!     let frame = ws.read_frame().await?;
//!     match frame.opcode {
//!       OpCode::Close => break,
//!       OpCode::Text | OpCode::Binary => {
//!         ws.write_frame(frame).await?;
//!       }
//!       _ => {}
//!     }
//!   }
//!   Ok(())
//! }
//! ```
//!
//! ## Fragmentation
//!
//! By default, fastwebsockets will give the application raw frames with FIN set. Other
//! crates like tungstenite which will give you a single message with all the frames
//! concatenated.
//!
//! For concanated frames, use `FragmentCollector`:
//! ```
//! use fastwebsockets::{FragmentCollector, WebSocket, Role};
//! use tokio::net::TcpStream;
//! use anyhow::Result;
//!
//! async fn handle(
//!   socket: TcpStream,
//! ) -> Result<()> {
//!   let mut ws = WebSocket::after_handshake(socket, Role::Server);
//!   let mut ws = FragmentCollector::new(ws);
//!   let incoming = ws.read_frame().await?;
//!   // Always returns full messages
//!   assert!(incoming.fin);
//!   Ok(())
//! }
//! ```
//!
//! _permessage-deflate is not supported yet._
//!
//! ## HTTP Upgrades
//!
//! Enable the `upgrade` feature to do server-side upgrades and client-side
//! handshakes.
//!
//! This feature is powered by [hyper](https://docs.rs/hyper).
//!
//! ```
//! use fastwebsockets::upgrade::upgrade;
//! use hyper::{Request, Body, Response};
//! use anyhow::Result;
//!
//! async fn server_upgrade(
//!   mut req: Request<Body>,
//! ) -> Result<Response<Body>> {
//!   let (response, fut) = upgrade(&mut req)?;
//!
//!   tokio::spawn(async move {
//!     let ws = fut.await;
//!     // Do something with the websocket
//!   });
//!
//!   Ok(response)
//! }
//! ```
//!
//! Use the `handshake` module for client-side handshakes.
//!
//! ```
//! use fastwebsockets::handshake;
//! use fastwebsockets::FragmentCollector;
//! use hyper::{Request, Body, upgrade::Upgraded, header::{UPGRADE, CONNECTION}};
//! use tokio::net::TcpStream;
//! use std::future::Future;
//! use anyhow::Result;
//!
//! async fn connect() -> Result<FragmentCollector<Upgraded>> {
//!   let stream = TcpStream::connect("localhost:9001").await?;
//!
//!   let req = Request::builder()
//!     .method("GET")
//!     .uri("http://localhost:9001/")
//!     .header("Host", "localhost:9001")
//!     .header(UPGRADE, "websocket")
//!     .header(CONNECTION, "upgrade")
//!     .header(
//!       "Sec-WebSocket-Key",
//!       fastwebsockets::handshake::generate_key(),
//!     )
//!     .header("Sec-WebSocket-Version", "13")
//!     .body(Body::empty())?;
//!
//!   let (ws, _) = handshake::client(&SpawnExecutor, req, stream).await?;
//!   Ok(FragmentCollector::new(ws))
//! }
//!
//! // Tie hyper's executor to tokio runtime
//! struct SpawnExecutor;
//!
//! impl<Fut> hyper::rt::Executor<Fut> for SpawnExecutor
//! where
//!   Fut: Future + Send + 'static,
//!   Fut::Output: Send + 'static,
//! {
//!   fn execute(&self, fut: Fut) {
//!     tokio::task::spawn(fut);
//!   }
//! }
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]

mod close;
mod error;
mod fragment;
mod frame;
/// Client handshake.
#[cfg(feature = "upgrade")]
#[cfg_attr(docsrs, doc(cfg(feature = "upgrade")))]
pub mod handshake;
mod mask;
/// HTTP upgrades.
#[cfg(feature = "upgrade")]
#[cfg_attr(docsrs, doc(cfg(feature = "upgrade")))]
pub mod upgrade;

use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

pub use crate::close::CloseCode;
pub use crate::error::WebSocketError;
pub use crate::fragment::FragmentCollector;
pub use crate::frame::Frame;
pub use crate::frame::OpCode;
pub use crate::frame::Payload;
pub use crate::mask::unmask;

#[derive(Copy, Clone, PartialEq)]
pub enum Role {
  Server,
  Client,
}

pub(crate) struct WriteHalf {
  role: Role,
  closed: bool,
  vectored: bool,
  auto_apply_mask: bool,
  writev_threshold: usize,
  write_buffer: Vec<u8>,
}

pub(crate) struct ReadHalf {
  role: Role,
  spill: Option<Vec<u8>>,
  auto_apply_mask: bool,
  auto_close: bool,
  auto_pong: bool,
  writev_threshold: usize,
  max_message_size: usize,
}

/// WebSocket protocol implementation over an async stream.
pub struct WebSocket<S> {
  stream: S,
  write_half: WriteHalf,
  read_half: ReadHalf,
}

const RECV_SIZE: usize = 16384;

impl<'f, S> WebSocket<S> {
  /// Creates a new `WebSocket` from a stream that has already completed the WebSocket handshake.
  ///
  /// Use the `upgrade` feature to handle server upgrades and client handshakes.
  ///
  /// # Example
  ///
  /// ```
  /// use tokio::net::TcpStream;
  /// use fastwebsockets::{WebSocket, OpCode, Role};
  /// use anyhow::Result;
  ///
  /// async fn handle_client(
  ///   socket: TcpStream,
  /// ) -> Result<()> {
  ///   let mut ws = WebSocket::after_handshake(socket, Role::Server);
  ///   // ...
  ///   Ok(())
  /// }
  /// ```
  pub fn after_handshake(stream: S, role: Role) -> Self
  where
    S: AsyncReadExt + AsyncWriteExt + Unpin,
  {
    Self {
      stream,
      write_half: WriteHalf {
        role,
        closed: false,
        auto_apply_mask: true,
        vectored: true,
        writev_threshold: 1024,
        write_buffer: Vec::with_capacity(2),
      },
      read_half: ReadHalf {
        role,
        spill: None,
        auto_apply_mask: true,
        auto_close: true,
        auto_pong: true,
        writev_threshold: 1024,
        max_message_size: 64 << 20,
      },
    }
  }

  /// Consumes the `WebSocket` and returns the underlying stream.
  #[inline]
  pub fn into_inner(self) -> S {
    // self.write_half.into_inner().stream
    self.stream
  }

  /// Consumes the `WebSocket` and returns the underlying stream.
  #[inline]
  pub(crate) fn into_parts_internal(self) -> (S, ReadHalf, WriteHalf) {
    (self.stream, self.read_half, self.write_half)
  }

  /// Sets whether to use vectored writes. This option does not guarantee that vectored writes will be always used.
  ///
  /// Default: `true`
  pub fn set_writev(&mut self, vectored: bool) {
    self.write_half.vectored = vectored;
  }

  pub fn set_writev_threshold(&mut self, threshold: usize) {
    self.read_half.writev_threshold = threshold;
    self.write_half.writev_threshold = threshold;
  }

  /// Sets whether to automatically close the connection when a close frame is received. When set to `false`, the application will have to manually send close frames.
  ///
  /// Default: `true`
  pub fn set_auto_close(&mut self, auto_close: bool) {
    self.read_half.auto_close = auto_close;
  }

  /// Sets whether to automatically send a pong frame when a ping frame is received.
  ///
  /// Default: `true`
  pub fn set_auto_pong(&mut self, auto_pong: bool) {
    self.read_half.auto_pong = auto_pong;
  }

  /// Sets the maximum message size in bytes. If a message is received that is larger than this, the connection will be closed.
  ///
  /// Default: 64 MiB
  pub fn set_max_message_size(&mut self, max_message_size: usize) {
    self.read_half.max_message_size = max_message_size;
  }

  /// Sets whether to automatically apply the mask to the frame payload.
  ///
  /// Default: `true`
  pub fn set_auto_apply_mask(&mut self, auto_apply_mask: bool) {
    self.read_half.auto_apply_mask = auto_apply_mask;
    self.write_half.auto_apply_mask = auto_apply_mask;
  }

  /// Writes a frame to the stream.
  ///
  /// # Example
  ///
  /// ```
  /// use fastwebsockets::{WebSocket, Frame, OpCode};
  /// use tokio::net::TcpStream;
  /// use anyhow::Result;
  ///
  /// async fn send(
  ///   ws: &mut WebSocket<TcpStream>
  /// ) -> Result<()> {
  ///   let mut frame = Frame::binary(vec![0x01, 0x02, 0x03].into());
  ///   ws.write_frame(frame).await?;
  ///   Ok(())
  /// }
  /// ```
  pub async fn write_frame(
    &mut self,
    frame: Frame<'f>,
  ) -> Result<(), WebSocketError>
  where
    S: AsyncReadExt + AsyncWriteExt + Unpin + Send,
  {
    self.write_half.write_frame(&mut self.stream, frame).await?;
    Ok(())
  }

  /// Reads a frame from the stream.
  ///
  /// This method will unmask the frame payload. For fragmented frames, use `FragmentCollector::read_frame`.
  ///
  /// Text frames payload is guaranteed to be valid UTF-8.
  ///
  /// # Example
  ///
  /// ```
  /// use fastwebsockets::{OpCode, WebSocket, Frame};
  /// use tokio::net::TcpStream;
  /// use anyhow::Result;
  ///
  /// async fn echo(
  ///   ws: &mut WebSocket<TcpStream>
  /// ) -> Result<()> {
  ///   let frame = ws.read_frame().await?;
  ///   match frame.opcode {
  ///     OpCode::Text | OpCode::Binary => {
  ///       ws.write_frame(frame).await?;
  ///     }
  ///     _ => {}
  ///   }
  ///   Ok(())
  /// }
  /// ```
  pub async fn read_frame(&mut self) -> Result<Frame<'f>, WebSocketError>
  where
    S: AsyncReadExt + AsyncWriteExt + Unpin + Send,
  {
    loop {
      let (res, obligated_send) =
        self.read_half.read_frame_inner(&mut self.stream).await;
      let is_closed = self.write_half.closed;
      if let Some(frame) = obligated_send {
        if !is_closed {
          self.write_half.write_frame(&mut self.stream, frame).await?;
        }
      }
      if let Some(frame) = res? {
        if is_closed && frame.opcode != OpCode::Close {
          return Err(WebSocketError::ConnectionClosed);
        }
        break Ok(frame);
      }
    }
  }
}

impl ReadHalf {
  /// Attempt to read a single frame from from the incoming stream, returning any send obligations if
  /// `auto_close` or `auto_pong` are enabled. Callers to this function are obligated to send the
  /// frame in the latter half of the tuple if one is specified, unless the write half of this socket
  /// has been closed.
  ///
  /// XXX: Do not expose this method to the public API.
  /// Lifetime requirements for safe recv buffer use are not enforced.
  pub(crate) async fn read_frame_inner<'f, S>(
    &mut self,
    stream: &mut S,
  ) -> (Result<Option<Frame<'f>>, WebSocketError>, Option<Frame<'f>>)
  where
    S: AsyncReadExt + Unpin,
  {
    let mut frame = match self.parse_frame_header(stream).await {
      Ok(frame) => frame,
      Err(e) => return (Err(e), None),
    };

    if self.role == Role::Server && self.auto_apply_mask {
      frame.unmask()
    };

    match frame.opcode {
      OpCode::Close if self.auto_close => {
        match frame.payload.len() {
          0 => {}
          1 => return (Err(WebSocketError::InvalidCloseFrame), None),
          _ => {
            let code = close::CloseCode::from(u16::from_be_bytes(
              frame.payload[0..2].try_into().unwrap(),
            ));

            #[cfg(feature = "simd")]
            if simdutf8::basic::from_utf8(&frame.payload[2..]).is_err() {
              return (Err(WebSocketError::InvalidUTF8), None);
            };

            #[cfg(not(feature = "simd"))]
            if std::str::from_utf8(&frame.payload[2..]).is_err() {
              return (Err(WebSocketError::InvalidUTF8), None);
            };

            if !code.is_allowed() {
              return (
                Err(WebSocketError::InvalidCloseCode),
                Some(Frame::close(1002, &frame.payload[2..])),
              );
            }
          }
        };

        let obligated_send = Frame::close_raw(frame.payload.to_owned().into());
        (Ok(Some(frame)), Some(obligated_send))
      }
      OpCode::Ping if self.auto_pong => {
        (Ok(None), Some(Frame::pong(frame.payload)))
      }
      OpCode::Text => {
        if frame.fin && !frame.is_utf8() {
          (Err(WebSocketError::InvalidUTF8), None)
        } else {
          (Ok(Some(frame)), None)
        }
      }
      _ => (Ok(Some(frame)), None),
    }
  }

  async fn parse_frame_header<'a, S>(
    &mut self,
    stream: &mut S,
  ) -> Result<Frame<'a>, WebSocketError>
  where
    S: AsyncReadExt + Unpin,
  {
    macro_rules! eof {
      ($n:expr) => {{
        let n = $n;
        if n == 0 {
          return Err(WebSocketError::UnexpectedEOF);
        }
        n
      }};
    }

    let mut nread = self.spill.as_ref().map(|v| v.len()).unwrap_or(0);
    let mut head = self.spill.take().unwrap_or(vec![0u8; RECV_SIZE]);

    while nread < 2 {
      nread += eof!(stream.read(&mut head[nread..]).await?);
    }

    let fin = head[0] & 0b10000000 != 0;

    let rsv1 = head[0] & 0b01000000 != 0;
    let rsv2 = head[0] & 0b00100000 != 0;
    let rsv3 = head[0] & 0b00010000 != 0;

    if rsv1 || rsv2 || rsv3 {
      return Err(WebSocketError::ReservedBitsNotZero);
    }

    let opcode = frame::OpCode::try_from(head[0] & 0b00001111)?;
    let masked = head[1] & 0b10000000 != 0;

    let length_code = head[1] & 0x7F;
    let extra = match length_code {
      126 => 2,
      127 => 8,
      _ => 0,
    };

    let length: usize = if extra > 0 {
      while nread < 2 + extra {
        nread += eof!(stream.read(&mut head[nread..]).await?);
      }

      match extra {
        2 => u16::from_be_bytes(head[2..4].try_into().unwrap()) as usize,
        8 => usize::from_be_bytes(head[2..10].try_into().unwrap()),
        _ => unreachable!(),
      }
    } else {
      length_code as usize
    };

    let mask = match masked {
      true => {
        while nread < 2 + extra + 4 {
          nread += eof!(stream.read(&mut head[nread..]).await?);
        }

        Some(head[2 + extra..2 + extra + 4].try_into().unwrap())
      }
      false => None,
    };

    if frame::is_control(opcode) && !fin {
      return Err(WebSocketError::ControlFrameFragmented);
    }

    if opcode == OpCode::Ping && length > 125 {
      return Err(WebSocketError::PingFrameTooLarge);
    }

    if length >= self.max_message_size {
      return Err(WebSocketError::FrameTooLarge);
    }

    let required = 2 + extra + mask.map(|_| 4).unwrap_or(0) + length;
    if required > nread {
      // Allocate more space
      let mut new_head = head.clone();
      new_head.resize(required, 0);

      stream.read_exact(&mut new_head[nread..]).await?;
      return Ok(Frame::new(
        fin,
        opcode,
        mask,
        Payload::Owned(new_head[required - length..].to_vec()),
      ));
    } else if nread > required {
      // We read too much
      self.spill = Some(head[required..nread].to_vec());
    }

    let payload = &head[required - length..required];
    let payload = Payload::Owned(payload.to_vec());
    let frame = Frame::new(fin, opcode, mask, payload);
    Ok(frame)
  }
}

impl WriteHalf {
  /// Writes a frame to the provided stream.
  pub async fn write_frame<'a, S>(
    &'a mut self,
    stream: &mut S,
    mut frame: Frame<'a>,
  ) -> Result<(), WebSocketError>
  where
    S: AsyncWriteExt + Unpin + Send,
  {
    if self.role == Role::Client && self.auto_apply_mask {
      frame.mask();
    }

    if frame.opcode == OpCode::Close {
      self.closed = true;
    }

    if self.vectored && frame.payload.len() > self.writev_threshold {
      frame.writev(stream).await?;
    } else {
      let text = frame.write(&mut self.write_buffer);
      stream.write_all(text).await?;
    }

    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const _: () = {
    const fn assert_unsync<S>() {
      // Generic trait with a blanket impl over `()` for all types.
      trait AmbiguousIfImpl<A> {
        // Required for actually being able to reference the trait.
        fn some_item() {}
      }

      impl<T: ?Sized> AmbiguousIfImpl<()> for T {}

      // Used for the specialized impl when *all* traits in
      // `$($t)+` are implemented.
      #[allow(dead_code)]
      struct Invalid;

      impl<T: ?Sized + Sync> AmbiguousIfImpl<Invalid> for T {}

      // If there is only one specialized trait impl, type inference with
      // `_` can be resolved and this can compile. Fails to compile if
      // `$x` implements `AmbiguousIfImpl<Invalid>`.
      let _ = <S as AmbiguousIfImpl<_>>::some_item;
    }
    assert_unsync::<WebSocket<tokio::net::TcpStream>>();
  };
}
