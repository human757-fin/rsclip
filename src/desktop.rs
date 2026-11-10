use anyhow::{Context, Result};
use std::{
  env,
  path::{Path, PathBuf},
};
use windows::{
  core::PCWSTR,
  Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT},
};
use windows_registry::CURRENT_USER;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const APP: &str = "rsclip";

pub fn launches_on_login() -> bool {
  CURRENT_USER
    .open(RUN_KEY)
    .and_then(|key| key.get_string(APP))
    .is_ok()
}

pub fn set_launch_on_login(enabled: bool) -> Result<()> {
  if enabled {
    let exe = env::current_exe().context("Could not find the rsclip executable")?;
    let command = format!("\"{}\"", exe.display());
    let current = CURRENT_USER
      .open(RUN_KEY)
      .and_then(|key| key.get_string(APP));
    if current.as_deref().ok() != Some(command.as_str()) {
      CURRENT_USER.create(RUN_KEY)?.set_string(APP, command)?;
    }
  } else if launches_on_login() {
    CURRENT_USER
      .options()
      .write()
      .open(RUN_KEY)?
      .remove_value(APP)?;
  }
  Ok(())
}

pub fn choose_folder(current: &Path, title: &str) -> Option<PathBuf> {
  rfd::FileDialog::new()
    .set_title(title)
    .set_directory(current)
    .pick_folder()
}

pub fn play_saved_sound(screenshot: bool) {
  let sound: &'static [u8] = if screenshot {
    include_bytes!("../sounds/screenshot.wav")
  } else {
    include_bytes!("../sounds/clip.wav")
  };
  // The buffer is static because asynchronous playback can outlive this call.
  unsafe {
    let _ = PlaySoundW(
      PCWSTR(sound.as_ptr().cast()),
      None,
      SND_ASYNC | SND_MEMORY | SND_NODEFAULT,
    );
  }
}

pub fn reveal_file(path: &Path) -> Result<()> {
  use std::os::windows::ffi::OsStrExt;
  use windows::Win32::{
    System::Com::{CoInitializeEx, CoTaskMemFree, CoUninitialize, COINIT_APARTMENTTHREADED},
    UI::Shell::{SHOpenFolderAndSelectItems, SHParseDisplayName},
  };
  let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
  unsafe {
    CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
    let mut item = std::ptr::null_mut();
    let result = SHParseDisplayName(PCWSTR(name.as_ptr()), None, &mut item, 0, None)
      .and_then(|()| SHOpenFolderAndSelectItems(item, None, 0));
    CoTaskMemFree(Some(item.cast()));
    CoUninitialize();
    result.context("Could not select the clip in File Explorer")
  }
}
