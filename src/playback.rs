use crate::recorder::ffmpeg;
use anyhow::{bail, Context, Result};
use std::{
  collections::hash_map::DefaultHasher,
  fs,
  hash::{Hash, Hasher},
  os::windows::process::CommandExt,
  path::{Path, PathBuf},
  process::Command,
  sync::Mutex,
};

static THUMBNAILS: Mutex<()> = Mutex::new(());
pub static VIDEO_JOBS: Mutex<()> = Mutex::new(());

pub fn cached_path(root: &Path, source: &Path, extension: &str) -> Result<PathBuf> {
  let info = source.metadata().context("Clip not found")?;
  let mut key = DefaultHasher::new();
  source.hash(&mut key);
  info.len().hash(&mut key);
  info.modified()?.hash(&mut key);
  Ok(root.join(format!("{:016x}.{extension}", key.finish())))
}

fn render(mut command: Command, path: &Path) -> Result<()> {
  fs::create_dir_all(path.parent().context("Missing preview directory")?)?;
  let temp = path.with_file_name(format!(
    ".{}.{}",
    uuid::Uuid::new_v4(),
    path.extension().unwrap().to_string_lossy()
  ));
  let result = (|| -> Result<()> {
    let output = command
      .arg(&temp)
      .output()
      .context("Could not launch FFmpeg")?;
    if !output.status.success() {
      let message = String::from_utf8_lossy(&output.stderr);
      bail!(
        "Could not render preview. {}",
        message.trim().chars().take(450).collect::<String>()
      );
    }
    if temp.metadata()?.len() == 0 {
      bail!("No video frame was decoded.");
    }
    fs::rename(&temp, path)?;
    Ok(())
  })();
  if result.is_err() {
    let _ = fs::remove_file(temp);
  }
  result
}

pub fn thumbnail(root: &Path, source: &Path) -> Result<PathBuf> {
  let path = cached_path(root, source, "jpg")?;
  if !path.is_file() {
    let _guard = THUMBNAILS.lock().unwrap();
    if !path.is_file() {
      let mut command = ffmpeg();
      command
        .args(["-nostdin", "-threads", "1", "-i"])
        .arg(source)
        .args([
          "-map",
          "0:v:0",
          "-frames:v",
          "1",
          "-an",
          "-vf",
          "scale=320:180:force_original_aspect_ratio=decrease",
          "-threads",
          "1",
          "-q:v",
          "4",
          "-update",
          "1",
        ]);
      render(command, &path)?;
    }
  }
  Ok(path)
}

pub fn prepare(root: &Path, source: &Path, force: bool) -> Result<bool> {
  let path = cached_path(root, source, "webm")?;
  if path.is_file() {
    return Ok(true);
  }
  if !force {
    let probe = Command::new("ffprobe")
      .creation_flags(0x0800_0000)
      .args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=codec_name",
        "-of",
        "default=noprint_wrappers=1:nokey=1",
      ])
      .arg(source)
      .output();
    match probe {
      Ok(output) if output.status.success() => {
        if String::from_utf8_lossy(&output.stdout).trim() == "h264" {
          return Ok(false);
        }
      }
      _ => return Ok(false),
    }
  }
  let _guard = VIDEO_JOBS.lock().unwrap();
  if !path.is_file() {
    let mut command = ffmpeg();
    command.args(["-nostdin", "-threads", "2", "-i"]).arg(source).args([
      "-map", "0:v:0", "-map", "0:a:0?",
      "-vf", r"fps=min(source_fps\,30),scale=w=min(854\,iw):h=min(480\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p",
      "-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-threads", "2",
      "-crf", "12", "-b:v", "1400k", "-c:a", "libopus", "-b:a", "96k",
    ]);
    render(command, &path)?;
    // Keep only the latest preview video. Thumbnails are small and stay cached.
    for entry in fs::read_dir(root)?.flatten() {
      let old = entry.path();
      if old != path
        && old.extension().is_some_and(|ext| ext == "webm")
        && old
          .file_stem()
          .and_then(|stem| stem.to_str())
          .is_some_and(|stem| stem.len() == 16 && stem.bytes().all(|byte| byte.is_ascii_hexdigit()))
      {
        let _ = fs::remove_file(old);
      }
    }
  }
  Ok(true)
}
