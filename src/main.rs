#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app_audio;
mod audio;
mod desktop;
mod diagnostics;
mod hotkeys;
mod instance;
mod maintenance;
mod mixer;
mod monitors;
mod overlay;
mod performance;
mod playback;
mod recorder;
mod settings;
mod web;

use anyhow::{Context, Result};
use recorder::Recorder;
use std::{
  net::TcpListener,
  sync::{mpsc, Arc, Mutex},
  thread,
};
use tauri::{
  menu::{Menu, MenuItem},
  tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
  Manager, RunEvent, Theme, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use windows::{
  core::{w, PCWSTR},
  Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR},
};

fn main() {
  let default_hook = std::panic::take_hook();
  std::panic::set_hook(Box::new(move |info| { diagnostics::record("panic", &info.to_string()); default_hook(info); }));
  if let Err(error) = run() {
    diagnostics::record("startup", &format!("{error:#}"));
    let message: Vec<_> = format!("rsclip could not start.\n\n{error:#}")
      .encode_utf16()
      .chain(Some(0))
      .collect();
    unsafe { MessageBoxW(None, PCWSTR(message.as_ptr()), w!("rsclip"), MB_ICONERROR) };
  }
}

fn run() -> Result<()> {
  let Some(mut instance) = instance::Instance::acquire()? else { return Ok(()); };
  let (feedback, feedback_events) = mpsc::channel();
  let recorder = Recorder::new(feedback)?;
  let start_hidden = recorder.starts_hidden() && !std::env::args().any(|arg| arg == "--show-window");
  let recorder = Arc::new(Mutex::new(recorder));
  maintenance::start(recorder.clone());
  let listener = TcpListener::bind(("127.0.0.1", 0))?;
  let host = listener.local_addr()?.to_string();
  let url: tauri::Url = format!("http://{host}/").parse()?;
  let origin = url.origin();
  let token = uuid::Uuid::new_v4().simple().to_string();
  let web_recorder = recorder.clone();
  let (commands, queue) = mpsc::channel();
  let web_commands = commands.clone();

  let app = tauri::Builder::default()
    .setup(move |app| {
      let overlay_url = url.join("overlay.html")?;
      let window = WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
        .title("rsclip")
        .inner_size(1280.0, 840.0)
        .min_inner_size(620.0, 480.0)
        .visible(!start_hidden)
        .focused(!start_hidden)
        .theme(Some(Theme::Dark))
        .on_navigation(move |target| target.origin() == origin)
        .build()?;
      let quit = MenuItem::with_id(app, "quit", "Quit rsclip", true, None::<&str>)?;
      let menu = Menu::with_items(app, &[&quit])?;
      let icon = app
        .default_window_icon()
        .context("Missing rsclip tray icon")?;
      TrayIconBuilder::with_id("rsclip")
        .icon(icon.clone())
        .tooltip("rsclip")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| {
          if event.id().as_ref() == "quit" {
            app.exit(0);
          }
        })
        .on_tray_icon_event(|tray, event| {
          if matches!(
            event,
            TrayIconEvent::Click {
              button: MouseButton::Left,
              button_state: MouseButtonState::Up,
              ..
            }
          ) {
            if let Some(window) = tray.app_handle().get_webview_window("main") {
              let _ = window.show();
              let _ = window.unminimize();
              let _ = window.set_focus();
            }
          }
        })
        .build(app)?;
      overlay::setup(app.handle(), overlay_url, feedback_events);
      let close_window = window.clone();
      window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
          api.prevent_close();
          let _ = close_window.eval("window.pauseClipPlayback?.()");
          let _ = close_window.set_fullscreen(false);
          let _ = close_window.hide();
        }
      });
      let handle = app.handle().clone();
      thread::spawn(move || web::serve(listener, host, token, web_recorder, web_commands, handle));
      Ok(())
    })
    .build(tauri::generate_context!())?;

  instance.listen(app.handle().clone());
  let background_recorder = recorder.clone();
  let mut worker = Some(thread::spawn(move || {
    hotkeys::monitor(background_recorder, queue)
  }));
  app.run(move |_, event| {
    if matches!(event, RunEvent::Exit) {
      let _ = commands.send(hotkeys::Command::Stop);
      if let Some(worker) = worker.take() {
        let _ = worker.join();
      }
      recorder.lock().unwrap().shutdown();
    }
  });
  Ok(())
}
