use crate::recorder::Recorder;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
  fs, io,
  path::Path,
  sync::{mpsc, Arc, Mutex},
  time::{Duration, Instant},
};
use windows::Win32::UI::{
  Input::KeyboardAndMouse::{RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT},
  WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE, WM_HOTKEY},
};

const ACTIONS: [&str; 5] = ["save", "toggle", "screenshot", "save_long", "record"];
const ALT: u32 = 1;
const CTRL: u32 = 2;
const SHIFT: u32 = 4;
const WIN: u32 = 8;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Binding {
  modifiers: u32,
  key: String,
}

impl Binding {
  fn vk(&self) -> Result<u32> {
    if self.modifiers & !(ALT | CTRL | SHIFT | WIN) != 0 {
      bail!("Invalid shortcut modifiers.");
    }
    if let [key @ (b'A'..=b'Z' | b'0'..=b'9')] = self.key.as_bytes() {
      return Ok(u32::from(*key));
    }
    let named = match self.key.as_str() {
      "Backspace" => Some(0x08), "Tab" => Some(0x09), "Enter" => Some(0x0d),
      "Pause" => Some(0x13), "CapsLock" => Some(0x14),
      "Space" => Some(0x20), "PageUp" => Some(0x21), "PageDown" => Some(0x22),
      "End" => Some(0x23), "Home" => Some(0x24), "ArrowLeft" => Some(0x25),
      "ArrowUp" => Some(0x26), "ArrowRight" => Some(0x27), "ArrowDown" => Some(0x28),
      "PrintScreen" => Some(0x2c), "Insert" => Some(0x2d), "Delete" => Some(0x2e),
      "NumpadMultiply" => Some(0x6a), "NumpadAdd" => Some(0x6b),
      "NumpadSubtract" => Some(0x6d), "NumpadDecimal" => Some(0x6e), "NumpadDivide" => Some(0x6f),
      "NumLock" => Some(0x90), "ScrollLock" => Some(0x91),
      "Semicolon" => Some(0xba), "Equal" => Some(0xbb), "Comma" => Some(0xbc),
      "Minus" => Some(0xbd), "Period" => Some(0xbe), "Slash" => Some(0xbf),
      "Backquote" => Some(0xc0), "BracketLeft" => Some(0xdb), "Backslash" => Some(0xdc),
      "BracketRight" => Some(0xdd), "Quote" => Some(0xde), "IntlBackslash" => Some(0xe2),
      _ => None,
    };
    if let Some(vk) = named { return Ok(vk); }
    if let Some(digit) = self.key.strip_prefix("Numpad") {
      if let [digit @ b'0'..=b'9'] = digit.as_bytes() { return Ok(0x60 + u32::from(*digit - b'0')); }
    }
    let Some(number) = self
      .key
      .strip_prefix('F')
      .and_then(|key| key.parse::<u32>().ok())
    else {
      bail!("This key is not supported by Windows global shortcuts.");
    };
    if !(1..=24).contains(&number) || self.key != format!("F{number}") {
      bail!("This key is not supported by Windows global shortcuts.");
    }
    if number == 12 {
      bail!("Windows reserves F12. Choose a different key.");
    }
    Ok(0x70 + number - 1)
  }

  fn label(&self) -> String {
    let mut parts = Vec::new();
    for (modifier, label) in [(CTRL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")] {
      if self.modifiers & modifier != 0 {
        parts.push(label);
      }
    }
    parts.push(&self.key);
    parts.join(" + ")
  }

  fn register(&self, id: i32) -> Result<()> {
    let modifiers = HOT_KEY_MODIFIERS(self.modifiers) | MOD_NOREPEAT;
    unsafe { RegisterHotKey(None, id, modifiers, self.vk()?) }
      .map_err(|_| anyhow!("{} is unavailable. Try a different shortcut.", self.label()))
  }
}

fn defaults() -> [Binding; ACTIONS.len()] {
  ["F10", "F9", "F8", "F11", "F7"].map(|key| Binding {
    modifiers: ALT,
    key: key.into(),
  })
}

fn load(path: &Path) -> Result<[Binding; ACTIONS.len()]> {
  let bytes = match fs::read(path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(defaults()),
    Err(error) => return Err(error.into()),
  };
  let mut bindings: Vec<Binding> = serde_json::from_slice(&bytes)?;
  if !(3..=ACTIONS.len()).contains(&bindings.len()) {
    bail!("Invalid shortcut count");
  }
  let added = [
    ["F11", "F7", "F6", "F5", "F3"],
    ["F7", "F6", "F5", "F3", "F2"],
  ];
  for candidates in added.into_iter().skip(bindings.len() - 3) {
    let binding = candidates
      .into_iter()
      .map(|key| Binding {
        modifiers: ALT,
        key: key.into(),
      })
      .find(|binding| !bindings.contains(binding))
      .context("Could not assign a default shortcut")?;
    bindings.push(binding);
  }
  let bindings: [Binding; ACTIONS.len()] = bindings
    .try_into()
    .map_err(|_| anyhow!("Invalid shortcut count"))?;
  for (index, binding) in bindings.iter().enumerate() {
    binding.vk()?;
    if bindings[..index].contains(binding) {
      bail!("Duplicate saved shortcut");
    }
  }
  Ok(bindings)
}

fn save(path: &Path, bindings: &[Binding; ACTIONS.len()]) -> Result<()> {
  let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
  fs::write(&temp, serde_json::to_vec_pretty(bindings)?)?;
  let result = fs::rename(&temp, path);
  if result.is_err() {
    let _ = fs::remove_file(temp);
  }
  result.context("Could not save keybinds")
}

pub enum Command {
  Rebind {
    body: Value,
    reply: mpsc::Sender<Result<Value>>,
  },
  Stop,
}

pub fn request(tx: &mpsc::Sender<Command>, body: Value) -> Result<Value> {
  let (reply, rx) = mpsc::channel();
  tx.send(Command::Rebind { body, reply })
    .map_err(|_| anyhow!("Shortcut service is unavailable."))?;
  rx.recv()
    .map_err(|_| anyhow!("Shortcut service is unavailable."))?
}

struct Hotkeys {
  bindings: [Binding; ACTIONS.len()],
  ids: [Option<i32>; ACTIONS.len()],
}

impl Hotkeys {
  fn rebind(&mut self, path: &Path, body: Value) -> Result<()> {
    let action = body["action"].as_str().unwrap_or_default();
    let index = ACTIONS
      .iter()
      .position(|name| *name == action)
      .context("Unknown shortcut action.")?;
    let binding: Binding =
      serde_json::from_value(body["binding"].clone()).context("Invalid shortcut.")?;
    binding.vk()?;
    if self
      .bindings
      .iter()
      .enumerate()
      .any(|(other, current)| other != index && *current == binding)
    {
      bail!("{} is already assigned in rsclip.", binding.label());
    }
    if self.bindings[index] == binding && self.ids[index].is_some() {
      return Ok(());
    }

    // A spare ID lets Windows check the new binding while the old one remains active.
    let primary = (index as i32) * 2 + 1;
    let id = if self.ids[index] == Some(primary) {
      primary + 1
    } else {
      primary
    };
    binding.register(id)?;
    let mut proposed = self.bindings.clone();
    proposed[index] = binding;
    if let Err(error) = save(path, &proposed) {
      unsafe {
        let _ = UnregisterHotKey(None, id);
      }
      return Err(error);
    }
    if let Some(previous) = self.ids[index].replace(id) {
      unsafe {
        let _ = UnregisterHotKey(None, previous);
      }
    }
    self.bindings = proposed;
    Ok(())
  }

  fn publish(&self, recorder: &mut Recorder) {
    recorder.hotkeys = Value::Object(
      ACTIONS
        .into_iter()
        .zip(self.bindings.iter())
        .enumerate()
        .map(|(index, (action, binding))| {
          (
            action.into(),
            json!({
              "label": binding.label(), "registered": self.ids[index].is_some(),
            }),
          )
        })
        .collect(),
    );
  }
}

pub fn monitor(recorder: Arc<Mutex<Recorder>>, commands: mpsc::Receiver<Command>) {
  let path = recorder.lock().unwrap().hotkey_path();
  let (bindings, error) = match load(&path) {
    Ok(bindings) => (bindings, String::new()),
    Err(_) => (
      defaults(),
      "Could not read saved shortcuts. Defaults are shown.".into(),
    ),
  };
  let ids = std::array::from_fn(|index| {
    let id = (index as i32) * 2 + 1;
    bindings[index].register(id).ok().map(|_| id)
  });
  let mut hotkeys = Hotkeys { bindings, ids };
  {
    let mut recorder = recorder.lock().unwrap();
    recorder.hotkey_error = error;
    hotkeys.publish(&mut recorder);
    recorder.start_automatically();
  }
  let mut next_tick = Instant::now();
  let mut editing_until = None;
  loop {
    match commands.recv_timeout(Duration::from_millis(40)) {
      Ok(Command::Rebind { body, reply }) => {
        let result = if body["action"] == "editing" {
          let editing = body["enabled"].as_bool().unwrap_or(false);
          if editing {
            for id in hotkeys.ids.iter_mut() {
              if let Some(id) = id.take() {
                unsafe {
                  let _ = UnregisterHotKey(None, id);
                }
              }
            }
            editing_until = Some(Instant::now() + Duration::from_secs(15));
          } else {
            editing_until = Some(Instant::now());
          }
          Ok(())
        } else {
          hotkeys.rebind(&path, body)
        }
        .map(|()| {
          let mut recorder = recorder.lock().unwrap();
          recorder.hotkey_error.clear();
          hotkeys.publish(&mut recorder);
          recorder.state()
        });
        let _ = reply.send(result);
      }
      Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
      Err(mpsc::RecvTimeoutError::Timeout) => {}
    }
    if editing_until.is_some_and(|deadline| Instant::now() >= deadline) {
      editing_until = None;
      for (index, id) in hotkeys.ids.iter_mut().enumerate() {
        if id.is_none() {
          let next = index as i32 * 2 + 1;
          *id = hotkeys.bindings[index].register(next).ok().map(|()| next);
        }
      }
      hotkeys.publish(&mut recorder.lock().unwrap());
    }
    let mut message = MSG::default();
    while unsafe { PeekMessageW(&mut message, None, WM_HOTKEY, WM_HOTKEY, PM_REMOVE) }.as_bool() {
      if let Some(index) = hotkeys
        .ids
        .iter()
        .position(|id| *id == Some(message.wParam.0 as i32))
      {
        let mut recorder = recorder.lock().unwrap();
        if let Err(error) = recorder.act(ACTIONS[index], &Value::Null) {
          recorder.notice = format!("{error:#}");
        }
      }
    }
    if Instant::now() >= next_tick {
      if let Ok(mut recorder) = recorder.try_lock() {
        recorder.tick();
        next_tick = Instant::now() + Duration::from_secs(1);
      }
    }
  }
  for id in hotkeys.ids.into_iter().flatten() {
    unsafe {
      let _ = UnregisterHotKey(None, id);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn validates_windows_key_codes_and_labels() {
    let binding = Binding {
      modifiers: CTRL | SHIFT,
      key: "K".into(),
    };
    assert_eq!(binding.vk().unwrap(), 0x4b);
    assert_eq!(binding.label(), "Ctrl + Shift + K");
    assert_eq!(defaults()[0].vk().unwrap(), 0x79);
    for key in ["F12", "F25", "F01", "Numpad10", "Unknown", "Escape"] {
      assert!(Binding {
        key: key.into(),
        ..binding.clone()
      }
      .vk()
      .is_err());
    }
    for (key, vk) in [("K", 0x4b), ("F6", 0x75), ("Space", 0x20), ("ArrowLeft", 0x25), ("Numpad3", 0x63), ("Semicolon", 0xba)] {
      let plain = Binding { modifiers: 0, key: key.into() };
      assert_eq!(plain.vk().unwrap(), vk);
      assert_eq!(plain.label(), key);
    }
    assert!(Binding { modifiers: 16, ..binding }.vk().is_err());
  }

  #[test]
  fn replaces_saved_bindings_and_rejects_duplicates() {
    let path = std::env::current_dir()
      .unwrap()
      .join("target")
      .join(format!("hotkeys-test-{}.json", uuid::Uuid::new_v4()));
    let mut bindings = defaults();
    assert_eq!(load(&path).unwrap(), bindings);
    fs::write(&path, serde_json::to_vec(&bindings[..3]).unwrap()).unwrap();
    assert_eq!(load(&path).unwrap(), bindings);
    bindings[0].key = "F7".into();
    fs::write(&path, serde_json::to_vec(&bindings[..4]).unwrap()).unwrap();
    let migrated = load(&path).unwrap();
    assert_eq!(migrated[..4], bindings[..4]);
    assert_eq!(migrated[4].label(), "Alt + F6");
    bindings[0].key = "Space".into();
    bindings[0].modifiers = 0;
    save(&path, &bindings).unwrap();
    assert_eq!(load(&path).unwrap(), bindings);
    bindings[1] = bindings[0].clone();
    save(&path, &bindings).unwrap();
    let result = load(&path);
    fs::remove_file(path).unwrap();
    assert!(result.is_err());
  }
}
