use std::sync::Arc;

use pixels::{Pixels, PixelsBuilder, SurfaceTexture};
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::WindowEvent;
use winit::event_loop::ActiveEventLoop;
use winit::window::{Window, WindowId};

use crate::net::{note_presented, Shared, UserEvent};

pub struct App {
    shared: Arc<Shared>,
    window: Option<Arc<Window>>,
    pixels: Option<Pixels<'static>>,
    sized: bool,
    latency: Option<crate::net::LatencyMeter>,
}

impl App {
    pub fn new(shared: Arc<Shared>) -> Self {
        Self {
            shared,
            window: None,
            pixels: None,
            sized: false,
            latency: None,
        }
    }

    fn present(&mut self) {
        let Some(pixels) = self.pixels.as_mut() else {
            return;
        };
        let frame = self.shared.latest.lock().expect("frame slot").take();
        let Some(frame) = frame else {
            return;
        };
        if pixels.frame().len() != frame.rgba.len() {
            if let Err(err) = pixels.resize_buffer(frame.width, frame.height) {
                tracing::error!(error = %err, "resize buffer");
                return;
            }
        }
        if !self.sized {
            if let Some(window) = &self.window {
                let _ = window.request_inner_size(LogicalSize::new(
                    (frame.width as f64).min(1600.0),
                    (frame.height as f64).min(900.0),
                ));
            }
            self.sized = true;
        }
        let buffer = pixels.frame_mut();
        if buffer.len() == frame.rgba.len() {
            buffer.copy_from_slice(&frame.rgba);
        }
        if let Err(err) = pixels.render() {
            tracing::error!(error = %err, "present");
            return;
        }
        note_presented(&mut self.latency, &frame, std::time::Instant::now());
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title("rd-client")
            .with_inner_size(LogicalSize::new(960.0, 540.0));
        let window = match event_loop.create_window(attributes) {
            Ok(window) => Arc::new(window),
            Err(err) => {
                tracing::error!(error = %err, "create window");
                event_loop.exit();
                return;
            }
        };
        let size = window.inner_size();
        let surface =
            SurfaceTexture::new(size.width.max(1), size.height.max(1), Arc::clone(&window));
        match PixelsBuilder::new(16, 16, surface)
            .texture_format(pixels::wgpu::TextureFormat::Rgba8Unorm)
            .enable_vsync(false)
            .build()
        {
            Ok(mut pixels) => {
                pixels.set_present_mode(pixels::wgpu::PresentMode::AutoNoVsync);
                tracing::info!(present_mode = ?pixels.present_mode(), "window ready");
                self.pixels = Some(pixels);
                self.window = Some(window);
            }
            Err(err) => {
                tracing::error!(error = %err, "create pixels surface");
                event_loop.exit();
            }
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Frame => self.present(),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.shared
                    .running
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(pixels) = self.pixels.as_mut() {
                    if size.width > 0 && size.height > 0 {
                        if let Err(err) = pixels.resize_surface(size.width, size.height) {
                            tracing::error!(error = %err, "resize surface");
                        }
                    }
                }
            }
            WindowEvent::RedrawRequested => self.present(),
            _ => {}
        }
    }
}
