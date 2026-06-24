// UI module — egui/eframe application.

use eframe::egui;
use std::net::Ipv4Addr;
use std::time::Instant;

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum AppState {
    Idle,
    Listening,
    Connecting,
    Connected {
        peer_ip: String,
        width: u32,
        height: u32,
    },
    Error(String),
}

// ---------------------------------------------------------------------------
// Main app struct
// ---------------------------------------------------------------------------

pub struct RemotedeskApp {
    pub state: AppState,
    local_ip: String,
    pub host_input: String,
    error_since: Option<Instant>,

    // Framebuffer for client fullscreen mode
    framebuffer: Vec<u8>,
    framebuffer_w: u32,
    framebuffer_h: u32,
    texture_handle: Option<egui::TextureHandle>,
    frame_dirty: bool,

    // Channels (set by main.rs)
    pub frame_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    pub input_tx: Option<tokio::sync::mpsc::UnboundedSender<crate::protocol::Message>>,
    pub runtime: Option<tokio::runtime::Handle>,
}

impl RemotedeskApp {
    pub fn new() -> Self {
        let local_ip = local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "desconocida".to_string());

        Self {
            state: AppState::Idle,
            local_ip,
            host_input: String::new(),
            error_since: None,
            framebuffer: Vec::new(),
            framebuffer_w: 0,
            framebuffer_h: 0,
            texture_handle: None,
            frame_dirty: false,
            frame_rx: None,
            input_tx: None,
            runtime: None,
        }
    }

    pub fn set_state(&mut self, state: AppState, ctx: &egui::Context) {
        self.state = state;
        ctx.request_repaint();
    }

    /// Validate an IPv4 address string.
    pub fn validate_ip(input: &str) -> bool {
        input.parse::<Ipv4Addr>().is_ok()
    }
}

// ---------------------------------------------------------------------------
// eframe::App implementation
// ---------------------------------------------------------------------------

impl eframe::App for RemotedeskApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Poll for new frame data
        self.poll_frames();

        // Auto-clear error after 4 seconds
        if let AppState::Error(_) = &self.state {
            if let Some(since) = self.error_since {
                if since.elapsed().as_secs() >= 4 {
                    self.state = AppState::Idle;
                    self.error_since = None;
                }
            }
        }

        // Upload texture if new frame arrived
        if self.frame_dirty && !self.framebuffer.is_empty() {
            self.upload_texture(ctx);
            self.frame_dirty = false;
        }

        match &self.state {
            AppState::Idle | AppState::Listening | AppState::Connecting => {
                self.render_connection_panel(ctx);
            }
            AppState::Connected {
                peer_ip,
                width,
                height,
            } => {
                self.render_fullscreen(ctx, peer_ip, *width, *height);
            }
            AppState::Error(msg) => {
                let msg = msg.clone();
                self.render_connection_panel(ctx);
                egui::TopBottomPanel::top("error_bar").show(ctx, move |ui| {
                    ui.colored_label(egui::Color32::RED, msg);
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

impl RemotedeskApp {
    fn poll_frames(&mut self) {
        if let Some(ref mut rx) = self.frame_rx {
            while let Ok(data) = rx.try_recv() {
                self.framebuffer = data;
                self.frame_dirty = true;
            }
        }
    }

    fn upload_texture(&mut self, ctx: &egui::Context) {
        let size = [self.framebuffer_w as usize, self.framebuffer_h as usize];
        let pixels = self.framebuffer.as_slice();

        // INCERTIDUMBRE: the framebuffer may be BGRA (from macOS capture) or
        // encoded H.264 data. For a proper client, this should be decoded RGBA.
        let color_image = egui::ColorImage::from_rgba_unmultiplied(size, pixels);

        if let Some(ref mut handle) = self.texture_handle {
            handle.set(color_image, egui::TextureOptions::LINEAR);
        } else {
            self.texture_handle = Some(ctx.load_texture(
                "framebuffer",
                color_image,
                egui::TextureOptions::default(),
            ));
        }
    }

    fn render_connection_panel(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.vertical_centered(|ui| {
                ui.heading("remotedesk");
                ui.separator();
                ui.label("Tu IP en esta red");
                ui.monospace(&self.local_ip);
                ui.add_space(12.0);

                ui.label("IP del host:");
                let can_connect = Self::validate_ip(&self.host_input)
                    && !matches!(self.state, AppState::Connecting)
                    && !matches!(self.state, AppState::Connected { .. });

                let editable = !matches!(self.state, AppState::Connecting)
                    && !matches!(self.state, AppState::Connected { .. });

                let response = ui.add_enabled(
                    editable,
                    egui::TextEdit::singleline(&mut self.host_input)
                        .hint_text("192.168.1.100")
                        .desired_width(200.0),
                );

                if response.lost_focus()
                    && ui.input(|i| i.key_pressed(egui::Key::Enter))
                    && can_connect
                {
                    self.try_connect(ctx);
                }

                ui.add_space(8.0);

                let connect_btn = ui.add_enabled(
                    can_connect,
                    egui::Button::new("Conectar").min_size(egui::vec2(200.0, 30.0)),
                );

                if connect_btn.clicked() {
                    self.try_connect(ctx);
                }

                ui.add_space(12.0);

                match &self.state {
                    AppState::Idle => {
                        ui.label("● Listo");
                    }
                    AppState::Listening => {
                        ui.label("● Esperando conexión entrante");
                    }
                    AppState::Connecting => {
                        ui.spinner();
                        ui.label("Conectando...");
                    }
                    _ => {}
                }
            });
        });
    }

    fn render_fullscreen(
        &self,
        ctx: &egui::Context,
        peer_ip: &str,
        width: u32,
        height: u32,
    ) {
        egui::CentralPanel::default().show(ctx, |ui| {
            if let Some(ref handle) = self.texture_handle {
                let available = ui.available_size();
                let img_aspect = width as f32 / height as f32;
                let panel_aspect = available.x / available.y;
                let (dw, dh) = if panel_aspect > img_aspect {
                    (available.y * img_aspect, available.y)
                } else {
                    (available.x, available.x / img_aspect)
                };

                ui.centered_and_justified(|ui| {
                    let texture_id = handle.id();
                    ui.image(egui::ImageSource::Texture(egui::load::SizedTexture::new(
                        texture_id,
                        egui::vec2(dw, dh),
                    )));
                });
            }
        });

        // Semi-transparent overlay
        egui::Area::new("overlay".into())
            .fixed_pos(egui::pos2(12.0, 12.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(egui::Color32::from_black_alpha(128))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| {
                        ui.colored_label(egui::Color32::WHITE, format!("● {peer_ip}"));
                        ui.colored_label(egui::Color32::WHITE, format!("{width}x{height}"));
                    });
            });
    }

    fn try_connect(&mut self, ctx: &egui::Context) {
        // This is a stub — the real connection orchestration is in main.rs.
        // Here we just set the state to trigger UI feedback.
        self.state = AppState::Connecting;
        ctx.request_repaint();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ip_validation() {
        assert!(RemotedeskApp::validate_ip("192.168.1.1"));
        assert!(RemotedeskApp::validate_ip("10.0.0.1"));
        assert!(RemotedeskApp::validate_ip("127.0.0.1"));
        assert!(!RemotedeskApp::validate_ip(""));
        assert!(!RemotedeskApp::validate_ip("hello"));
        assert!(!RemotedeskApp::validate_ip("192.168.1"));
        assert!(!RemotedeskApp::validate_ip("256.0.0.1"));
    }

    #[test]
    fn test_state_transitions() {
        // Verify states exist and can be cloned/compared
        let states = vec![
            AppState::Idle,
            AppState::Listening,
            AppState::Connecting,
            AppState::Connected {
                peer_ip: "192.168.1.1".into(),
                width: 1920,
                height: 1080,
            },
            AppState::Error("test".into()),
        ];

        for state in &states {
            let cloned = state.clone();
            assert_eq!(state, &cloned);
        }
    }
}
