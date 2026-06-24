// remotedesk — LAN remote desktop.
//
// Single binary: macOS runs as server (capture + encode + serve),
// Windows runs as client (connect + decode + display).
//
// Architecture:
//   macOS server:  capture → encoder → broadcast → serve_loop → TCP
//   Windows client: TCP → connect → mpsc → decoder → UI texture

#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod network;
mod platform;
mod protocol;
mod ui;

use eframe::egui;
use tokio::sync::{broadcast, mpsc};
use ui::RemotedeskApp;

fn main() -> anyhow::Result<()> {
    // Init tracing
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Create tokio runtime
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();

    // Broadcast channel: capture → server.
    // We keep one receiver alive at all times so send() never fails.
    // The server subscribes additional receivers per client connection.
    let (frame_tx, _keep_rx) = broadcast::channel::<Vec<u8>>(16);

    #[cfg(target_os = "macos")]
    {
        // --- Server mode: auto-start capture + server ---

        // Capture one frame to get the real (Retina) resolution.
        // display_resolution() may return logical points; the actual
        // CGDisplayCreateImage returns physical pixels.
        let (first_bgra, width, height, first_bpr) = platform::macos::capture_frame()
            .expect("initial capture");
        tracing::info!("Server: display {width}x{height} (captured)");

        // Encoder at the real resolution
        let mut encoder = platform::macos::VideoEncoder::new(width, height, 120)
            .expect("create encoder");
        tracing::info!(
            "Encoder: codec=0x{:08X}",
            encoder.codec_type
        );

        // Feed the first frame immediately
        if let Err(e) = encoder.encode_frame(&first_bgra, first_bpr) {
            tracing::error!("First frame encode error: {e}");
        }

        // Input channel: client → server
        let (input_tx, mut input_rx) = mpsc::channel::<protocol::Message>(32);

        // Start TCP server
        let server_frame_tx = frame_tx.clone();
        let server_input_tx = input_tx.clone();
        runtime.spawn(async move {
            if let Err(e) = network::start_server(
                7070,
                width,
                height,
                120,
                server_frame_tx,
                server_input_tx,
            )
            .await
            {
                tracing::error!("Server error: {e}");
            }
        });

        // Capture + encode loop on a dedicated thread
        let cap_frame_tx = frame_tx;
        std::thread::spawn(move || {
            loop {
                // Capture frame
                let (bgra, w, h, bpr) = match platform::macos::capture_frame() {
                    Ok(f) => f,
                    Err(e) => {
                        tracing::error!("Capture error: {e}");
                        continue;
                    }
                };

                // If resolution changed, log it (dynamic resize not supported yet)
                if w != width || h != height {
                    tracing::warn!("Resolution changed from {width}x{height} to {w}x{h}");
                }

                // Encode
                if let Err(e) = encoder.encode_frame(&bgra, bpr) {
                    tracing::error!("Encode error: {e}");
                }

                // Drain encoded output into the broadcast channel.
                // Ignore send errors — they happen when no client is connected
                // (our _keep_rx stays alive but doesn't consume; real receivers
                // come from serve_loop subscribing on client connect).
                while let Ok(data) = encoder.output_rx().try_recv() {
                    let _ = cap_frame_tx.send(data);
                }

                // Small sleep to avoid burning CPU
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        });

        // Input relay: forward incoming client inputs to the OS
        runtime.spawn(async move {
            while let Some(msg) = input_rx.recv().await {
                use protocol::Message;
                match msg {
                    Message::MouseMove { x, y } => {
                        let _ = platform::macos::inject_mouse_move(x, y, width, height);
                    }
                    Message::MouseClick { button, pressed } => {
                        let _ = platform::macos::inject_mouse_click(button, pressed);
                    }
                    Message::KeyEvent { keycode, pressed } => {
                        let _ = platform::macos::inject_key_event(keycode as u16, pressed);
                    }
                    Message::Disconnect => {
                        tracing::info!("Client disconnected");
                    }
                    _ => {}
                }
            }
        });
    }

    // --- UI (both platforms) ---

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([340.0, 220.0])
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "remotedesk",
        options,
        Box::new(move |_cc| {
            let mut app = RemotedeskApp::new();
            app.runtime = Some(runtime.handle().clone());

            #[cfg(target_os = "macos")]
            {
                app.state = ui::AppState::Listening;
            }

            Box::new(app)
        }),
    )
    .map_err(|e| anyhow::anyhow!("eframe error: {e}"))?;

    Ok(())
}
