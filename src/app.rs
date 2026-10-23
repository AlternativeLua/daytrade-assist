//! Native app window around the web UI.

use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoop};
use tao::window::WindowBuilder;
use wry::WebViewBuilder;

/// Runs the window on the main thread until it is closed, then exits.
pub fn run_window(url: &str) -> ! {
    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Daytrade Assist")
        .with_inner_size(LogicalSize::new(1360.0, 900.0))
        .with_min_inner_size(LogicalSize::new(720.0, 560.0))
        .build(&event_loop)
        .expect("failed to open the app window");
    let webview = WebViewBuilder::new()
        .with_url(url)
        .build(&window)
        .expect("failed to create the web view");

    event_loop.run(move |event, _, control_flow| {
        let _keep_alive = &webview;
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    })
}
