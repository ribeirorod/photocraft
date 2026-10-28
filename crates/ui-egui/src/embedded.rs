//! A windowless app session: the app's `logic` + `ui` run frame by frame in an off-screen
//! [`egui::Context`] and are drawn by the CPU rasterizer ([`crate::softpaint`]), with
//! control-protocol requests answered by the app's own handler. Hosts that show PhotoCraft
//! inside their own UI (Creative Suite) run it through `photocraft-cli embed`: no window, no GPU,
//! no display server, on any system.

use std::sync::Arc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use egui::{Color32, ColorImage, RawInput, ViewportCommand, ViewportId};
use serde_json::{Value, json};

use crate::softpaint::{self, TextureStore};
use crate::{ControlRequest, PhotocraftApp};

/// Background behind the panels (eframe's default clear colour).
const CLEAR: Color32 = Color32::from_rgb(12, 12, 12);

pub struct Embedded {
    pub app: PhotocraftApp,
    ctx: egui::Context,
    textures: TextureStore,
    shapes: Vec<egui::epaint::ClippedShape>,
    /// Logical size (points) and scale.
    pub size: egui::Vec2,
    pub pixels_per_point: f32,
    start: Instant,
    events: Vec<egui::Event>,
    control: Sender<ControlRequest>,
    quit: bool,
}

impl Embedded {
    /// Wrap `app` (its control channel is replaced by the session's).
    pub fn new(app: PhotocraftApp, size: [f32; 2], pixels_per_point: f32) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Embedded {
            app: app.with_control(rx),
            ctx: egui::Context::default(),
            textures: TextureStore::default(),
            shapes: Vec::new(),
            size: egui::vec2(size[0], size[1]),
            pixels_per_point,
            start: Instant::now(),
            events: Vec::new(),
            control: tx,
            quit: false,
        }
    }

    /// `app.quit` (or the window's close) was requested.
    pub fn quit_requested(&self) -> bool {
        self.quit
    }

    fn raw_input(&mut self) -> RawInput {
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, self.size);
        let mut raw = RawInput {
            screen_rect: Some(rect),
            time: Some(self.start.elapsed().as_secs_f64()),
            focused: true,
            events: std::mem::take(&mut self.events),
            max_texture_side: Some(16384),
            ..Default::default()
        };
        let info = raw.viewports.entry(ViewportId::ROOT).or_default();
        info.native_pixels_per_point = Some(self.pixels_per_point);
        info.inner_rect = Some(rect);
        info.outer_rect = Some(rect);
        info.focused = Some(true);
        raw
    }

    /// Run one frame.
    pub fn step(&mut self) {
        let mut raw = self.raw_input();
        self.app.input_hook(&self.ctx, &mut raw);
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(raw, |ui| {
            app.logic_frame(ui.ctx());
            app.ui_frame(ui);
        });
        self.textures.apply(std::mem::take(&mut out.textures_delta));
        self.shapes = std::mem::take(&mut out.shapes);
        self.pixels_per_point = out.pixels_per_point;
        let commands = out.viewport_output.remove(&ViewportId::ROOT).map(|v| v.commands).unwrap_or_default();
        for c in commands {
            match c {
                // A screenshot is the frame just drawn, answered on the next one as eframe does.
                ViewportCommand::Screenshot(user_data) => {
                    let image = Arc::new(self.paint());
                    self.events.push(egui::Event::Screenshot { viewport_id: ViewportId::ROOT, user_data, image });
                }
                ViewportCommand::InnerSize(s) if s.x >= 64.0 && s.y >= 64.0 && s.x <= 8192.0 && s.y <= 8192.0 => self.size = s,
                ViewportCommand::Close => self.quit = true,
                _ => {}
            }
        }
    }

    /// Something is still happening (input to play out, a repaint requested soon).
    pub fn busy(&self) -> bool {
        !self.events.is_empty() || self.ctx.has_requested_repaint()
    }

    /// Rasterize the last frame.
    pub fn paint(&self) -> ColorImage {
        let prims = self.ctx.tessellate(self.shapes.clone(), self.pixels_per_point);
        let size = [(self.size.x * self.pixels_per_point).round() as usize, (self.size.y * self.pixels_per_point).round() as usize];
        softpaint::paint(&prims, &self.textures, size, self.pixels_per_point, CLEAR)
    }

    /// Send a control-protocol request and run frames until it is answered (and its injected
    /// input has played out). Returns `{"ok": …, "result"|"error": …}`.
    pub fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Value {
        let (req, rx) = ControlRequest::new(method, params);
        if self.control.send(req).is_err() {
            return json!({"ok": false, "error": "control channel closed"});
        }
        let t0 = Instant::now();
        let reply = loop {
            self.step();
            if let Ok(r) = rx.try_recv() {
                break r;
            }
            if t0.elapsed() > timeout {
                return json!({"ok": false, "error": "timeout"});
            }
            std::thread::sleep(Duration::from_millis(2));
        };
        // A couple more frames so a click or key the request injected takes effect.
        for _ in 0..3 {
            self.step();
        }
        reply
    }
}
