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
static FFMPEG_JOB: Mutex<()> = Mutex::new(());
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
fn validate_ffmpeg() -> Result<()> {
  for name in ["ffmpeg", "ffprobe"] {
    let output = Command::new(executable(name)).creation_flags(0x0800_0000)
      .arg("-version").output().with_context(|| format!("Could not launch {name}"))?;
    if !output.status.success() {
      bail!("{name} exited with {}. {}", output.status, String::from_utf8_lossy(&output.stderr).trim());
    }
  }
  for (argument, required) in [("-filters", "ddagrab"), ("-encoders", "h264_nvenc")] {
    let output = Command::new(executable("ffmpeg")).creation_flags(0x0800_0000)
      .args(["-hide_banner", argument]).output()?;
    if !output.status.success() || !String::from_utf8_lossy(&output.stdout).contains(required) {
      bail!("The FFmpeg build does not provide {required}.");
    }
  }
  Ok(())
}
pub fn ensure_ffmpeg() -> Result<()> {
  if validate_ffmpeg().is_ok() { return Ok(()); }
  status().lock().unwrap()["ffmpeg"] = json!("Downloading and verifying FFmpeg. This can take several minutes…");
  powershell(include_str!("../installer/install-ffmpeg.ps1"))?;
  validate_ffmpeg().context("FFmpeg was downloaded but could not run")
}
pub fn setup_ffmpeg(recorder: &Mutex<crate::recorder::Recorder>) {
  let Ok(_job) = FFMPEG_JOB.try_lock() else { return };
  status().lock().unwrap()["ffmpegBusy"] = json!(true);
  match ensure_ffmpeg() {
    Ok(()) => {
      status().lock().unwrap()["ffmpeg"] = json!("FFmpeg is ready.");
      recorder.lock().unwrap().refresh_ffmpeg();
    }
    Err(error) => {
      status().lock().unwrap()["ffmpeg"] = json!(format!("FFmpeg setup failed: {error:#}"));
    }
  }
  status().lock().unwrap()["ffmpegBusy"] = json!(false);
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
  std::thread::spawn(move || setup_ffmpeg(&recorder));
  std::thread::spawn(check_update);
}
