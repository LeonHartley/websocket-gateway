use byteorder::{BigEndian, ByteOrder};
use bytes::{Buf, BufMut, BytesMut};
use chrono::Local;
use env_logger::Builder;
use futures::stream::SplitStream;
use futures::{SinkExt, Stream};

use futures_util::StreamExt;
use log::LevelFilter;
use log::*;
use std::env;
use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::ReadHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{accept_async, WebSocketStream};
use tokio_util::codec::{Decoder, Encoder, FramedRead, FramedWrite};
use tungstenite::error::Error::{ConnectionClosed, Protocol, Utf8};
use tungstenite::Message;

struct ProxyConfig {
    source_addr: String,
    destination_addr: String,
}

async fn accept_connection(peer: SocketAddr, stream: TcpStream, config: Arc<ProxyConfig>) {
    if let Err(e) = handle_connection(peer, stream, config).await {
        match e {
            ConnectionClosed | Protocol(_) | Utf8 => (),
            err => error!("Error processing connection: {}", err),
        }
    }
}

pub enum ProxyEvent {
    WebSocketClosed,
    WebSocketRead(Vec<u8>),
    TcpClosed,
    TcpRead(Vec<u8>),
}

pub struct NetworkCodec;

impl Encoder<Vec<u8>> for NetworkCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: Vec<u8>, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.put_slice(item.as_slice());
        Ok(())
    }
}

impl Decoder for NetworkCodec {
    type Item = Vec<u8>;
    type Error = std::io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        if src.remaining() < 6 {
            return Ok(None);
        }

        let len = BigEndian::read_i32(src);

        if (src.remaining() - 4) < len as usize {
            return Ok(None);
        }

        Ok(Some(src.split_to(4 + len as usize).to_vec()))
    }
}

struct ProxyStream {
    inbound: SplitStream<WebSocketStream<TcpStream>>,
    outbound: FramedRead<ReadHalf<TcpStream>, NetworkCodec>,
}

impl Stream for ProxyStream {
    type Item = ProxyEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Poll::Ready(Some(Ok(msg))) = Pin::new(&mut self.inbound).poll_next(cx) {
            match msg {
                Message::Binary(buffer) => {
                    return Poll::Ready(Some(ProxyEvent::WebSocketRead(buffer)))
                }
                Message::Text(txt) => {
                    return Poll::Ready(Some(ProxyEvent::WebSocketRead(txt.into_bytes())))
                }
                Message::Close(_) => return Poll::Ready(Some(ProxyEvent::WebSocketClosed)),
                _ => {}
            };
        };

        let result: Option<Result<Vec<u8>, std::io::Error>> =
            futures::ready!(Pin::new(&mut self.outbound).poll_next(cx));

        if let Some(res) = result {
            return match res {
                Ok(buffer) => Poll::Ready(Some(ProxyEvent::TcpRead(buffer))),
                Err(_) => Poll::Ready(Some(ProxyEvent::TcpClosed)),
            };
        };

        Poll::Pending
    }
}

async fn handle_connection(
    peer: SocketAddr,
    stream: TcpStream,
    config: Arc<ProxyConfig>,
) -> Result<(), tungstenite::Error> {
    let ws_stream = accept_async(stream).await.expect("Failed to accept");
    debug!("new WebSocket connection: {}", peer);

    let (mut ws_sender, ws_receiver) = ws_stream.split();
    let upstream = TcpStream::connect(&config.destination_addr).await;
    if !upstream.is_ok() {
        error!(
            "failed to connect to upstream: {}",
            &config.destination_addr
        );
        return Ok(());
    }

    let (tcp_receiver, tcp_sender) = tokio::io::split(upstream.unwrap());
    let mut tcp_sender = FramedWrite::new(tcp_sender, NetworkCodec);

    let mut proxy_stream = ProxyStream {
        inbound: ws_receiver,
        outbound: FramedRead::new(tcp_receiver, NetworkCodec),
    };

    while let Some(event) = proxy_stream.next().await {
        match event {
            ProxyEvent::WebSocketRead(buffer) => {
                debug!("received from websocket, len: {}", buffer.len());
                if !tcp_sender.send(buffer).await.is_ok() {
                    error!("error sending to tcp client");
                }
            }

            ProxyEvent::TcpRead(buffer) => {
                debug!("received from tcp, len: {}", buffer.len());
                if !ws_sender.send(Message::Binary(buffer)).await.is_ok() {
                    error!("error sending to websocket client");
                }
            }

            ProxyEvent::TcpClosed => {
                debug!("tcp closed");
                if !ws_sender.close().await.is_ok() {
                    error!("error closing websocket sender");
                }
                break;
            }

            ProxyEvent::WebSocketClosed => {
                debug!("websocket closed");
                if !tcp_sender.close().await.is_ok() {
                    error!("error closing tcp sender");
                }
                break;
            }
        }
    }

    debug!("connection closed");
    Ok(())
}

#[tokio::main]
async fn main() {
    Builder::new()
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] {} - {}",
                Local::now().format("%Y-%m-%dT%H:%M:%S"),
                record.level(),
                record.target(),
                record.args(),
            )
        })
        .filter(None, LevelFilter::Info)
        .init();

    let config = Arc::new(ProxyConfig {
        source_addr: env::var("WEBSOCKET_SOURCE_ADDR")
            .unwrap_or_else(|_| "0.0.0.0:1233".to_string()),

        destination_addr: env::var("TCP_DESTINATION_ADDR")
            .unwrap_or_else(|_| "localhost:30000".to_string()),
    });

    let mut listener = TcpListener::bind(&config.source_addr)
        .await
        .expect("Can't listen");

    info!("websocket-gateway listening for websocket connections on: {}, forwarding to TCP server: {}", &config.source_addr, &config.destination_addr);

    while let Ok((stream, _)) = listener.accept().await {
        let peer = stream.peer_addr().unwrap();

        tokio::spawn(accept_connection(peer, stream, config.clone()));
    }
}
