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
  "preview-1080-v2".hash(&mut key);
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
          "scale=640:360:force_original_aspect_ratio=decrease",
          "-threads",
          "1",
          "-q:v",
          "2",
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
    let probe = Command::new(crate::maintenance::executable("ffprobe"))
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
      "-vf", r"fps=min(source_fps\,60),scale=w=min(1920\,iw):h=min(1080\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p",
      "-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "4", "-threads", "2",
      "-crf", "8", "-b:v", "8000k", "-c:a", "libopus", "-b:a", "96k",
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

pub fn probe_audio(source: &Path) -> Result<serde_json::Value> {
  let output = Command::new(crate::maintenance::executable("ffprobe"))
    .creation_flags(0x0800_0000)
    .args([
      "-v",
      "error",
      "-select_streams",
      "a",
      "-show_entries",
      "stream_tags=handler_name",
      "-of",
      "json",
    ])
    .arg(source)
    .output()?;
  if !output.status.success() {
    bail!("Could not read clip audio tracks");
  }
  Ok(serde_json::from_slice(&output.stdout)?)
}

pub fn audio_channels(probe: &serde_json::Value) -> Vec<(String, String)> {
  probe["streams"]
    .as_array()
    .into_iter()
    .flatten()
    .filter_map(|stream| {
      let (id, name) = stream["tags"]["handler_name"]
        .as_str()?
        .strip_prefix("rsclip:")?
        .split_once(':')?;
      Some((id.to_owned(), name.to_owned()))
    })
    .collect()
}

pub fn mixed_preview(root: &Path, source: &Path, body: &serde_json::Value) -> Result<String> {
  let mix = crate::mixer::MixerSettings::default().clip_mix(body)?;
  let probe = probe_audio(source)?;
  let graph =
    crate::mixer::export_filter(&probe, &mix).context("This clip has no separate audio tracks")?;
  prepare(root, source, true)?;
  let video = cached_path(root, source, "webm")?;
  let mut hash = DefaultHasher::new();
  mix.gains.hash(&mut hash);
  let path = cached_path(root, source, &format!("{:016x}.mix.webm", hash.finish()))?;
  let _guard = VIDEO_JOBS.lock().unwrap();
  if !path.is_file() {
    let mut command = ffmpeg();
    command
      .args(["-nostdin", "-threads", "2", "-i"])
      .arg(source)
      .arg("-i")
      .arg(video)
      .args([
        "-filter_complex",
        &graph,
        "-map",
        "1:v:0",
        "-map",
        "[audio]",
        "-c:v",
        "copy",
        "-c:a",
        "libopus",
        "-b:a",
        "160k",
      ]);
    render(command, &path)?;
  }
  Ok(path.file_name().unwrap().to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  #[ignore = "requires FFmpeg on PATH"]
  fn clip_preview_mixes_isolated_audio_and_reuses_video() -> Result<()> {
    let root = std::env::current_dir()?
      .join("target")
      .join(format!("preview-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root)?;
    let source = root.join("source.mp4");
    let status = ffmpeg()
      .args([
        "-nostdin",
        "-f",
        "lavfi",
        "-i",
        "color=size=160x90:rate=15:duration=0.5",
        "-f",
        "lavfi",
        "-i",
        "sine=frequency=440:duration=0.5",
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-map",
        "1:a",
        "-c:v",
        "libx264",
        "-c:a",
        "aac",
        "-metadata:s:a:1",
        "handler_name=rsclip:microphone:Microphone",
      ])
      .arg(&source)
      .output()?;
    assert!(status.status.success());
    let probe = probe_audio(&source)?;
    assert_eq!(
      audio_channels(&probe),
      vec![("microphone".into(), "Microphone".into())]
    );
    let muted = serde_json::json!({"gains":{"microphone":0}});
    let first = mixed_preview(&root, &source, &muted)?;
    assert_eq!(first.len(), 42);
    assert_eq!(first, mixed_preview(&root, &source, &muted)?);
    let other = mixed_preview(
      &root,
      &source,
      &serde_json::json!({"gains":{"microphone":100}}),
    )?;
    assert_ne!(first, other);
    assert_eq!(
      probe_audio(&root.join(&first))?["streams"]
        .as_array()
        .unwrap()
        .len(),
      1
    );
    let decoded = ffmpeg()
      .arg("-i")
      .arg(root.join(first))
      .args(["-map", "0:a:0", "-f", "s16le", "-"])
      .output()?;
    assert!(decoded.status.success());
    assert!(!decoded.stdout.is_empty());
    assert!(decoded.stdout.iter().all(|&byte| byte == 0));
    fs::remove_dir_all(root)?;
    Ok(())
  }
}
