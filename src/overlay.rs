use crate::playback;
use base64::{engine::general_purpose::STANDARD, Engine};
use std::{
  fs,
  path::PathBuf,
  sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
  },
  thread,
  time::{Duration, Instant},
};
use tauri::{AppHandle, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

pub struct Feedback {
  pub title: &'static str,
  pub source: PathBuf,
  pub cache: PathBuf,
  pub next_segment: Option<PathBuf>,
}

pub enum OverlayEvent {
  Show(Feedback),
  Disable,
}

fn create_window(
  app: &AppHandle,
  label: &str,
  url: tauri::Url,
  initial: &str,
) -> tauri::Result<tauri::WebviewWindow> {
  let origin = url.origin();
  let window = WebviewWindowBuilder::new(app, label, WebviewUrl::External(url))
    .title("rsclip capture feedback")
    .inner_size(312.0, 224.0)
    .decorations(false)
    .transparent(true)
    .visible(false)
    .focused(false)
    .focusable(false)
    .skip_taskbar(true)
    .always_on_top(true)
    .content_protected(true)
    .initialization_script(initial)
    .on_navigation(move |target| target.origin() == origin)
    .build()?;
  let _ = window.set_ignore_cursor_events(true);
  Ok(window)
}

pub fn setup(app: &AppHandle, url: tauri::Url, events: mpsc::Receiver<OverlayEvent>) {
  let app = app.clone();
  thread::spawn(move || {
    let sequence = Arc::new(AtomicU64::new(0));
    let mut label = "capture-feedback-0".to_owned();
    for event in events {
      let id = sequence.fetch_add(1, Ordering::Relaxed) + 1;
      let event = match event {
        OverlayEvent::Show(event) => event,
        OverlayEvent::Disable => {
          let previous = std::mem::replace(&mut label, format!("capture-feedback-{id}"));
          let handle = app.clone();
          let _ = app.run_on_main_thread(move || {
            if let Some(window) = handle.get_webview_window(&previous) {
              let _ = window.destroy();
            }
          });
          continue;
        }
      };
      let title = serde_json::to_string(event.title).unwrap();
      let show_label = label.clone();
      let url = url.clone();
      dispatch(&app, &sequence, id, move |handle| {
        let script = format!(
          "window.feedback = {{ id: {id}, title: {title}, image: '' }}; window.renderFeedback?.();"
        );
        let window = if let Some(window) = handle.get_webview_window(&show_label) {
          let _ = window.eval(&script);
          window
        } else {
          let Ok(window) = create_window(&handle, &show_label, url, &script) else {
            return;
          };
          window
        };
        if let Ok(Some(monitor)) = window.primary_monitor() {
          if let Ok(size) = window.outer_size() {
            let area = monitor.work_area();
            let _ = window.set_position(PhysicalPosition::new(
              area.position.x + (area.size.width as i32 - size.width as i32) / 2,
              area.position.y,
            ));
          }
        }
        let _ = window.show();
      });
      let (frame_app, frame_sequence, frame_label) = (app.clone(), sequence.clone(), label.clone());
      thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(2800);
        if let Some(next) = &event.next_segment {
          while !next.is_file() && Instant::now() < deadline {
            if frame_sequence.load(Ordering::Relaxed) != id {
              return;
            }
            thread::sleep(Duration::from_millis(80));
          }
        }
        if frame_sequence.load(Ordering::Relaxed) != id {
          return;
        }
        if let Ok(jpeg) =
          playback::thumbnail(&event.cache, &event.source).and_then(|path| Ok(fs::read(path)?))
        {
          let image = STANDARD.encode(jpeg);
          dispatch(&frame_app, &frame_sequence, id, move |handle| {
            let Some(window) = handle.get_webview_window(&frame_label) else {
              return;
            };
            let _ = window.eval(format!("if (window.feedback?.id === {id}) {{ window.feedback.image = 'data:image/jpeg;base64,{image}'; window.renderFeedback?.(); }}"));
          });
        }
      });
      let (hide_app, hide_sequence, hide_label) = (app.clone(), sequence.clone(), label.clone());
      thread::spawn(move || {
        thread::sleep(Duration::from_secs(4));
        dispatch(&hide_app, &hide_sequence, id, move |handle| {
          if let Some(window) = handle.get_webview_window(&hide_label) {
            let _ = window.hide();
          }
        });
      });
    }
  });
}

fn dispatch(
  app: &AppHandle,
  sequence: &Arc<AtomicU64>,
  id: u64,
  action: impl FnOnce(AppHandle) + Send + 'static,
) {
  let sequence = sequence.clone();
  let handle = app.clone();
  let _ = app.run_on_main_thread(move || {
    if sequence.load(Ordering::Relaxed) == id {
      action(handle);
    }
  });
}
