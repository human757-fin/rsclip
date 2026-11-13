use anyhow::{Context, Result};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc};
use tauri::Manager;
use windows::{core::{w, PCWSTR}, Win32::{Foundation::{CloseHandle, GetLastError, HANDLE, ERROR_ALREADY_EXISTS, WAIT_OBJECT_0}, System::Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE}}};

struct Handles { mutex: HANDLE, activate: HANDLE }
// Windows kernel handles support concurrent waits/signals. Arc keeps both alive until the waiter exits.
unsafe impl Send for Handles {}
unsafe impl Sync for Handles {}
impl Drop for Handles {
  fn drop(&mut self) { unsafe { let _ = CloseHandle(self.activate); let _ = CloseHandle(self.mutex); } }
}
pub struct Instance {
  handles: Arc<Handles>,
  stop: Arc<AtomicBool>,
  worker: Option<std::thread::JoinHandle<()>>,
}
impl Instance {
  pub fn acquire() -> Result<Option<Self>> {
    Self::acquire_named(w!("Local\\rsclip.app.rsclip.desktop.instance"), w!("Local\\rsclip.app.rsclip.desktop.activate"))
  }
  fn acquire_named(mutex_name: PCWSTR, event_name: PCWSTR) -> Result<Option<Self>> {
    unsafe {
      // Create the event first: another launch can signal it even while the primary starts up.
      let activate = CreateEventW(None, false, false, event_name)?;
      let mutex = match CreateMutexW(None, false, mutex_name) {
        Ok(handle) => handle,
        Err(error) => { let _ = CloseHandle(activate); return Err(error.into()); }
      };
      let exists = GetLastError() == ERROR_ALREADY_EXISTS;
      let handles = Arc::new(Handles { mutex, activate });
      if exists { SetEvent(handles.activate).context("Could not activate the running rsclip")?; return Ok(None); }
      Ok(Some(Self { handles, stop: Arc::new(AtomicBool::new(false)), worker: None }))
    }
  }
  pub fn listen(&mut self, app: tauri::AppHandle) {
    let handles = self.handles.clone();
    let stop = self.stop.clone();
    self.worker = Some(std::thread::spawn(move || loop {
      if unsafe { WaitForSingleObject(handles.activate, INFINITE) } != WAIT_OBJECT_0 || stop.load(Ordering::Acquire) { break; }
      let handle = app.clone();
      let _ = app.run_on_main_thread(move || {
        if let Some(window) = handle.get_webview_window("main") {
          let _ = window.show();
          let _ = window.unminimize();
          let _ = window.set_focus();
        }
      });
    }));
  }
}
impl Drop for Instance {
  fn drop(&mut self) {
    self.stop.store(true, Ordering::Release);
    unsafe { let _ = SetEvent(self.handles.activate); }
    if let Some(worker) = self.worker.take() { let _ = worker.join(); }
  }
}

#[cfg(test)]
mod tests {
  #[test]
  fn second_launch_signals_primary_and_releases_after_shutdown() {
    let name = format!("Local\\rsclip.test.{}", uuid::Uuid::new_v4());
    let mutex = windows::core::HSTRING::from(name.as_str());
    let event = windows::core::HSTRING::from(format!("{name}.activate"));
    let acquire = || super::Instance::acquire_named(super::PCWSTR(mutex.as_ptr()), super::PCWSTR(event.as_ptr())).unwrap();
    let primary = acquire().unwrap();
    assert!(acquire().is_none());
    assert_eq!(unsafe { super::WaitForSingleObject(primary.handles.activate, 0) }, super::WAIT_OBJECT_0);
    drop(primary);
    assert!(acquire().is_some());
  }
}
