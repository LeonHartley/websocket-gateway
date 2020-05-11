# websocket-gateway
WebSocket to TCP proxy written in Rust.

Built to enable web-based WebSocket connections on pre-existing TCP-based game servers. 
Works for a specific use-case, may be useful for other things too. 

Built for fun. :)

### Implementation
The proxy is built using `tungstenite` and `tokio-tungstenite` for inbound WebSocket connections and uses a raw async 
Tokio `TcpStream` for the TCP outbound client. The actual proxy stream is an implementation of a `futures::Stream`
which outputs events for both TCP and WebSocket streams such as `Read`, and `Closed`.  

This enables us to efficiently poll a single future and also keeps the stream simple, we can just listen to all events in a single block of code:

```rust
while let Some(event) = proxy_stream.next().await {
    match event {
        ProxyEvent::WebSocketRead(buffer) => {
            debug!("received from websocket, len: {}", buffer.len());
            tcp_sender.send(buffer).await;
        }

        ProxyEvent::TcpRead(buffer) => {
            debug!("received from tcp, len: {}", buffer.len());
            ws_sender.send(Message::Binary(buffer)).await;
        }

        ProxyEvent::TcpClosed => {
            debug!("tcp closed");
            ws_sender.close().await;
            break;
        }

        ProxyEvent::WebSocketClosed => {
            debug!("websocket closed");
            tcp_sender.close().await;
            break;
        }
    }
}
```
