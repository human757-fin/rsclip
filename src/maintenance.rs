use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
  env, fs,
  os::windows::process::CommandExt,
  path::PathBuf,
  process::Command,
  sync::{Mutex, OnceLock},
};

static STATUS: OnceLock<Mutex<Value>> = OnceLock::new();
static JOB: Mutex<()> = Mutex::new(());
fn status() -> &'static Mutex<Value> {
  STATUS.get_or_init(|| {
    Mutex::new(json!({"ffmpeg":"Checking FFmpeg…", "update":"Checking GitHub…", "ready":false}))
  })
}
pub fn state() -> Value {
  status().lock().unwrap().clone()
}
fn root() -> PathBuf {
  PathBuf::from(env::var_os("LOCALAPPDATA").unwrap_or_default())
    .join("rsclip")
    .join("tools")
}
pub fn executable(name: &str) -> PathBuf {
  let managed = root().join("ffmpeg").join(format!("{name}.exe"));
  if managed.is_file() {
    managed
  } else {
    PathBuf::from(name)
  }
}
fn powershell(script: &str) -> Result<String> {
  let output = Command::new("powershell.exe")
    .creation_flags(0x0800_0000)
    .args([
      "-NoLogo",
      "-NoProfile",
      "-NonInteractive",
      "-Command",
      script,
    ])
    .output()
    .context("Could not start Windows download service")?;
  if !output.status.success() {
    bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
  }
  Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}
pub fn ensure_ffmpeg() -> Result<()> {
  if ["ffmpeg", "ffprobe"].iter().all(|name| Command::new(executable(name))
    .creation_flags(0x0800_0000)
    .arg("-version")
    .output()
    .is_ok_and(|o| o.status.success()))
  {
    return Ok(());
  }
  powershell(include_str!("../installer/install-ffmpeg.ps1"))?;
  Ok(())
}
pub fn check_update() {
  let Ok(_job) = JOB.try_lock() else { return };
  status().lock().unwrap()["update"] = json!("Checking GitHub…");
  let script =
    include_str!("../installer/check-update.ps1").replace("__VERSION__", env!("CARGO_PKG_VERSION"));
  match powershell(&script) {
    Ok(version) => {
      let ready = !version.is_empty();
      let mut state = status().lock().unwrap();
      state["ready"] = json!(ready);
      state["update"] = json!(if ready {
        format!("rsclip {version} is ready to install.")
      } else {
        "You're up to date.".into()
      });
    }
    Err(error) => {
      status().lock().unwrap()["update"] = json!(format!("Update check failed: {error}"));
    }
  }
}
pub fn install() -> Result<()> {
  let _job = JOB.try_lock().map_err(|_| anyhow::anyhow!("Update download is still running"))?;
  if state()["ready"] != true {
    bail!("No verified update is ready.");
  }
  let setup = root().join("update-setup.exe");
  let expected = fs::read_to_string(root().join("update.sha256"))?;
  // Verify again immediately before execution, including across app restarts.
  let actual = powershell("(Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $env:LOCALAPPDATA 'rsclip\\tools\\update-setup.exe')).Hash")?;
  if !actual.eq_ignore_ascii_case(expected.trim()) {
    bail!("Update checksum changed. Check for updates again.");
  }
  Command::new(setup)
    .arg("/S")
    .spawn()
    .context("Could not launch update installer")?;
  Ok(())
}
pub fn start(recorder: std::sync::Arc<Mutex<crate::recorder::Recorder>>) {
  std::thread::spawn(move || match ensure_ffmpeg() {
    Ok(()) => {
      status().lock().unwrap()["ffmpeg"] = json!("FFmpeg is ready.");
      recorder.lock().unwrap().refresh_ffmpeg();
    }
    Err(error) => {
      status().lock().unwrap()["ffmpeg"] = json!(format!("FFmpeg setup failed: {error}"));
    }
  });
  std::thread::spawn(check_update);
}
