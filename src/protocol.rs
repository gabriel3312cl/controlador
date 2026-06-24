use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone)]
pub enum Message {
    Handshake {
        version: u8,
        width: u32,
        height: u32,
        fps: u8,
    },
    VideoFrame {
        timestamp_ms: u64,
        data: Vec<u8>,
    },
    MouseMove {
        x: f32,
        y: f32,
    },
    MouseClick {
        button: u8,
        pressed: bool,
    },
    KeyEvent {
        keycode: u32,
        pressed: bool,
    },
    Disconnect,
}

/// Write a length-prefixed Message to an async writer.
///
/// Wire format: [4 bytes u32 LE = payload len][bincode payload]
pub async fn write_message(
    writer: &mut (impl AsyncWrite + Unpin),
    msg: &Message,
) -> Result<()> {
    let payload = bincode::serialize(msg).context("serialize message")?;
    let len = payload.len() as u32;
    writer
        .write_all(&len.to_le_bytes())
        .await
        .context("write length prefix")?;
    writer
        .write_all(&payload)
        .await
        .context("write payload")?;
    Ok(())
}

/// Read a length-prefixed Message from an async reader.
pub async fn read_message(
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Message> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .context("read length prefix")?;
    let len = u32::from_le_bytes(len_buf) as usize;

    // Reject absurdly large payloads (100 MB cap)
    if len > 100_000_000 {
        bail!("message too large: {} bytes", len);
    }

    let mut payload = vec![0u8; len];
    reader
        .read_exact(&mut payload)
        .await
        .context("read payload")?;
    let msg = bincode::deserialize(&payload).context("deserialize message")?;
    Ok(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: write + read roundtrip over an in-memory duplex stream.
    async fn roundtrip(msg: &Message) -> Message {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);

        let msg_clone = msg.clone();
        let write_handle = tokio::spawn(async move {
            write_message(&mut client, &msg_clone).await.unwrap();
        });

        let read_handle = tokio::spawn(async move {
            read_message(&mut server).await.unwrap()
        });

        write_handle.await.unwrap();
        read_handle.await.unwrap()
    }

    #[tokio::test]
    async fn test_message_roundtrip_handshake() {
        let msg = Message::Handshake {
            version: 1,
            width: 1920,
            height: 1080,
            fps: 120,
        };
        let back = roundtrip(&msg).await;
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn test_message_roundtrip_video_frame() {
        let msg = Message::VideoFrame {
            timestamp_ms: 12345,
            data: vec![0xAB; 1024],
        };
        let back = roundtrip(&msg).await;
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn test_message_roundtrip_mouse_move() {
        let msg = Message::MouseMove { x: 0.5, y: 0.75 };
        let back = roundtrip(&msg).await;
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn test_message_roundtrip_mouse_click() {
        let msg = Message::MouseClick {
            button: 1,
            pressed: true,
        };
        let back = roundtrip(&msg).await;
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn test_message_roundtrip_key_event() {
        let msg = Message::KeyEvent {
            keycode: 56,
            pressed: false,
        };
        let back = roundtrip(&msg).await;
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn test_message_roundtrip_disconnect() {
        let msg = Message::Disconnect;
        let back = roundtrip(&msg).await;
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn test_framing_write_read() {
        // Writes two messages back-to-back, reads them in order.
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);

        let msg1 = Message::Handshake {
            version: 1,
            width: 2560,
            height: 1440,
            fps: 60,
        };
        let msg2 = Message::VideoFrame {
            timestamp_ms: 0,
            data: vec![0xCD; 500],
        };

        let msg1_c = msg1.clone();
        let msg2_c = msg2.clone();

        let writer = tokio::spawn(async move {
            write_message(&mut client, &msg1_c).await.unwrap();
            write_message(&mut client, &msg2_c).await.unwrap();
        });

        let reader = tokio::spawn(async move {
            let a = read_message(&mut server).await.unwrap();
            let b = read_message(&mut server).await.unwrap();
            (a, b)
        });

        writer.await.unwrap();
        let (a, b) = reader.await.unwrap();

        assert_eq!(msg1, a);
        assert_eq!(msg2, b);
    }

    #[tokio::test]
    async fn test_large_frame() {
        // 500 KB frame should roundtrip correctly.
        let msg = Message::VideoFrame {
            timestamp_ms: 999,
            data: vec![0x7E; 500_000],
        };
        let back = roundtrip(&msg).await;
        assert_eq!(msg, back);
    }

    #[tokio::test]
    async fn test_bincode_direct_serialization() {
        // Verify bincode serializes all variants without framing.
        let variants: Vec<Message> = vec![
            Message::Handshake {
                version: 1,
                width: 1920,
                height: 1080,
                fps: 120,
            },
            Message::VideoFrame {
                timestamp_ms: 1,
                data: vec![0; 100],
            },
            Message::MouseMove { x: 0.1, y: 0.9 },
            Message::MouseClick {
                button: 0,
                pressed: false,
            },
            Message::KeyEvent {
                keycode: 42,
                pressed: true,
            },
            Message::Disconnect,
        ];

        for original in variants {
            let bytes = bincode::serialize(&original).unwrap();
            let restored: Message = bincode::deserialize(&bytes).unwrap();
            assert_eq!(original, restored);
        }
    }
}
