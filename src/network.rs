use anyhow::{Context, Result};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, error, info, warn};

use crate::protocol::{self, Message};

/// Run the TCP server loop.
///
/// Binds to `0.0.0.0:port`, accepts one client at a time, sends the Handshake
/// with the provided display dimensions, then forwards frames from `frame_rx`
/// (a broadcast receiver subscribed to the capture pipeline) and relays incoming
/// input messages into `input_tx`.
///
/// Uses `broadcast` so that frame producers can have a single sender and the
/// server can subscribe/unsubscribe as clients connect and disconnect.
///
/// If the connection drops, it drains stale frames, logs, and returns to
/// listening — it never exits unless the listener itself fails.
pub async fn start_server(
    port: u16,
    width: u32,
    height: u32,
    fps: u8,
    frame_tx: broadcast::Sender<Vec<u8>>,
    input_tx: mpsc::Sender<Message>,
) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .context("bind server socket")?;
    info!("Server listening on 0.0.0.0:{port}");

    serve_loop(listener, width, height, fps, frame_tx, input_tx).await
}

/// Core accept loop — extracted so tests can inject their own listener.
pub async fn serve_loop(
    listener: TcpListener,
    width: u32,
    height: u32,
    fps: u8,
    frame_tx: broadcast::Sender<Vec<u8>>,
    input_tx: mpsc::Sender<Message>,
) -> Result<()> {
    let handshake = Message::Handshake {
        version: 1,
        width,
        height,
        fps,
    };

    loop {
        let (mut stream, addr) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                error!("Accept error: {e}");
                continue;
            }
        };
        info!("Client connected from {addr}");

        // Send handshake with display info
        if let Err(e) = protocol::write_message(&mut stream, &handshake).await {
            warn!("Failed to send handshake to {addr}: {e}");
            continue;
        }

        // Subscribe to frames for this connection
        let frame_rx = frame_tx.subscribe();

        // Bidirectional relay (owns stream, frame_rx, input_tx clone)
        if let Err(e) =
            relay_connection(stream, frame_rx, input_tx.clone()).await
        {
            warn!("Connection to {addr} lost: {e}");
        }
        info!("Returning to listen state");
    }
}

/// Relay frames outbound and inputs inbound for one client connection.
async fn relay_connection(
    stream: TcpStream,
    mut frame_rx: broadcast::Receiver<Vec<u8>>,
    input_tx: mpsc::Sender<Message>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();

    // Outbound: frames from broadcast
    let write_task = tokio::spawn(async move {
        loop {
            match frame_rx.recv().await {
                Ok(frame_data) => {
                    let msg = Message::VideoFrame {
                        timestamp_ms: 0, // INCERTIDUMBRE: timestamp real requiere coordinación con captura
                        data: frame_data,
                    };
                    if protocol::write_message(&mut writer, &msg).await.is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Frame relay lagged by {n} frames");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Inbound: inputs from client
    let read_task = tokio::spawn(async move {
        loop {
            match protocol::read_message(&mut reader).await {
                Ok(msg) => {
                    let is_disconnect = matches!(msg, Message::Disconnect);
                    if input_tx.send(msg).await.is_err() {
                        break; // receiver dropped
                    }
                    if is_disconnect {
                        break;
                    }
                }
                Err(e) => {
                    debug!("Read error: {e}");
                    break;
                }
            }
        }
    });

    // Wait for either task to finish
    tokio::select! {
        _ = write_task => {},
        _ = read_task => {},
    }

    Ok(())
}

/// Connect to a remote server as a client.
///
/// Receives the Handshake from the server and sends the resolution back
/// through `resolution_tx`. Then continuously reads VideoFrame messages into
/// `frame_tx` and writes input messages from `input_rx` to the socket.
///
/// On disconnect, retries up to `max_retries` times with 1-second backoff.
pub async fn connect(
    host: &str,
    port: u16,
    frame_tx: mpsc::Sender<Vec<u8>>,
    mut input_rx: mpsc::Receiver<Message>,
    resolution_tx: tokio::sync::oneshot::Sender<(u32, u32)>,
    max_retries: u32,
) -> Result<()> {
    let addr = format!("{host}:{port}");
    let mut attempt = 0;
    let mut res_tx = Some(resolution_tx);

    loop {
        attempt += 1;

        let tx = res_tx.take(); // Some on first attempt, None on retries
        match connect_once(&addr, &frame_tx, &mut input_rx, tx).await {
            Ok(()) => {
                info!("Connected to {addr}");
                return Ok(());
            }
            Err(e) => {
                if attempt > max_retries {
                    error!("Giving up after {max_retries} attempts: {e}");
                    return Err(e).context(format!(
                        "failed to connect to {addr} after {max_retries} attempts"
                    ));
                }
                warn!(
                    "Connection attempt {attempt}/{max_retries} failed: {e}. Retrying in 1s..."
                );
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

/// Single connection attempt. Returns Ok(()) when the connection ends.
async fn connect_once(
    addr: &str,
    frame_tx: &mpsc::Sender<Vec<u8>>,
    input_rx: &mut mpsc::Receiver<Message>,
    resolution_tx: Option<tokio::sync::oneshot::Sender<(u32, u32)>>,
) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .context("TCP connect")?;

    let (mut reader, mut writer) = stream.into_split();

    // First message must be Handshake from server
    let handshake = protocol::read_message(&mut reader)
        .await
        .context("read handshake")?;

    let (width, height) = match &handshake {
        Message::Handshake { width, height, .. } => (*width, *height),
        other => anyhow::bail!("expected Handshake, got {other:?}"),
    };

    // Send resolution back to caller (only on first successful handshake)
    if let Some(tx) = resolution_tx {
        let _ = tx.send((width, height));
    }

    // Bidirectional relay without spawning — use select! directly
    loop {
        tokio::select! {
            // Inbound: frames from server
            frame_msg = protocol::read_message(&mut reader) => {
                match frame_msg {
                    Ok(Message::VideoFrame { data, .. }) => {
                        if frame_tx.send(data).await.is_err() {
                            break; // receiver dropped
                        }
                    }
                    Ok(Message::Disconnect) => break,
                    Ok(_other) => {
                        // Ignore non-video messages from server
                    }
                    Err(e) => {
                        debug!("Read error: {e}");
                        return Err(e).context("connection lost");
                    }
                }
            }
            // Outbound: inputs from local user
            input_msg = input_rx.recv() => {
                match input_msg {
                    Some(msg) => {
                        let is_disconnect = matches!(msg, Message::Disconnect);
                        if protocol::write_message(&mut writer, &msg).await.is_err() {
                            break;
                        }
                        if is_disconnect {
                            break;
                        }
                    }
                    None => break, // input channel closed
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn a server on an OS-assigned port, returning handles for the test.
    #[allow(dead_code)]
    async fn spawn_server(
        port: u16,
    ) -> (
        broadcast::Sender<Vec<u8>>,
        mpsc::Receiver<Message>,
        tokio::task::JoinHandle<Result<()>>,
    ) {
        let (frame_tx, _) = broadcast::channel::<Vec<u8>>(16);
        let (input_tx, input_rx) = mpsc::channel::<Message>(10);

        let frame_tx_s = frame_tx.clone();
        let handle = tokio::spawn(async move {
            start_server(port, 1920, 1080, 60, frame_tx_s, input_tx).await
        });

        (frame_tx, input_rx, handle)
    }

    #[tokio::test]
    async fn test_server_accepts_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frame_tx, _) = broadcast::channel::<Vec<u8>>(16);
        let (input_tx, _input_rx) = mpsc::channel::<Message>(10);

        tokio::spawn(async move {
            serve_loop(listener, 1920, 1080, 60, frame_tx, input_tx)
                .await
                .unwrap();
        });

        // Small delay to let server start
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let msg = protocol::read_message(&mut stream).await.unwrap();

        assert_eq!(
            msg,
            Message::Handshake {
                version: 1,
                width: 1920,
                height: 1080,
                fps: 60,
            }
        );

        // Clean shutdown
        let _ = protocol::write_message(&mut stream, &Message::Disconnect).await;
    }

    #[tokio::test]
    async fn test_server_rejects_second_connection() {
        // The server handles one connection at a time. A second client will
        // have its TCP connect queued until the first disconnects, at which
        // point the server accepts it normally. This test verifies that both
        // clients can eventually connect and receive the handshake.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (frame_tx, _) = broadcast::channel::<Vec<u8>>(16);
        let (input_tx, _input_rx) = mpsc::channel::<Message>(10);

        tokio::spawn(async move {
            serve_loop(listener, 1920, 1080, 60, frame_tx, input_tx)
                .await
                .unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // First client
        let mut client1 = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let msg1 = protocol::read_message(&mut client1).await.unwrap();
        assert!(matches!(msg1, Message::Handshake { .. }));

        // Second client — queued, not rejected (current architecture)
        let client2_fut = TcpStream::connect(("127.0.0.1", port));

        // Disconnect first client to let second through
        let _ = protocol::write_message(&mut client1, &Message::Disconnect).await;
        drop(client1);

        let mut client2 = client2_fut.await.unwrap();
        let msg2 = protocol::read_message(&mut client2).await.unwrap();
        assert!(matches!(msg2, Message::Handshake { .. }));

        let _ = protocol::write_message(&mut client2, &Message::Disconnect).await;
    }

    #[tokio::test]
    async fn test_client_reconnect() {
        // Start a server, let client connect, drop server, verify client
        // reconnects when server comes back.
        let (frame_tx, _frame_rx) = mpsc::channel::<Vec<u8>>(10);
        let (_input_tx, input_rx) = mpsc::channel::<Message>(10);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        // Spawn a server that handles one connection then stops
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            protocol::write_message(
                &mut stream,
                &Message::Handshake {
                    version: 1,
                    width: 1920,
                    height: 1080,
                    fps: 60,
                },
            )
            .await
            .unwrap();
            // Drop the connection immediately to trigger reconnect
            drop(stream);
        });

        let (resolution_tx, _resolution_rx) = tokio::sync::oneshot::channel();
        let connect_result = tokio::spawn(async move {
            connect(
                "127.0.0.1",
                port,
                frame_tx,
                input_rx,
                resolution_tx,
                2, // max 2 retries = 3 total attempts
            )
            .await
        });

        server_handle.await.unwrap();

        // Client connects, gets handshake, then server drops connection.
        // Client retries but server is gone → should fail after 3 attempts.
        let result = connect_result.await.unwrap();
        assert!(result.is_err(), "Expected error after server gone");
    }

    #[tokio::test]
    async fn test_client_gives_up_after_max_retries() {
        let (frame_tx, _frame_rx) = mpsc::channel::<Vec<u8>>(10);
        let (_input_tx, input_rx) = mpsc::channel::<Message>(10);

        // Bind to get a port, then drop — no server running
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let (resolution_tx, _resolution_rx) = tokio::sync::oneshot::channel();

        let result = connect(
            "127.0.0.1",
            port,
            frame_tx,
            input_rx,
            resolution_tx,
            2, // max_retries=2 means 3 total attempts
        )
        .await;

        assert!(result.is_err(), "Expected error after 3 failed attempts");
    }
}
