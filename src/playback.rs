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
  "preview-h264-v3".hash(&mut key);
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

pub fn prepare(root: &Path, source: &Path, force: bool, native_hevc: bool) -> Result<bool> {
  let path = cached_path(root, source, "mp4")?;
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
        let codec = String::from_utf8_lossy(&output.stdout);
        if codec.trim() == "h264" || (native_hevc && codec.trim() == "hevc") {
          return Ok(false);
        }
      }
      _ => return Ok(false),
    }
  }
  let _guard = VIDEO_JOBS.lock().unwrap();
  if !path.is_file() {
    let encode = |hardware: bool| {
      let mut command = ffmpeg();
      command.args(["-nostdin", "-threads", "2", "-filter_threads", "2", "-i"]).arg(source).args([
        "-map", "0:v:0", "-map", "0:a:0?",
        "-vf", r"fps=min(source_fps\,60),scale=w=min(1920\,iw):h=min(1080\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p",
      ]);
      if hardware {
        command.args(["-c:v", "h264_nvenc", "-preset", "p1", "-rc", "vbr", "-cq", "20", "-b:v", "0"]);
      } else {
        command.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-threads", "2"]);
      }
      command.args(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"]);
      render(command, &path)
    };
    if encode(true).is_err() { encode(false)?; }
    // Retain recent previews rather than re-encoding on every clip switch.
    let mut entries: Vec<_> = fs::read_dir(root)?.flatten().filter_map(|entry| {
      let path = entry.path();
      let stem = path.file_stem()?.to_str()?;
      if path.extension()? != "mp4" || stem.len() != 16 || !stem.bytes().all(|c| c.is_ascii_hexdigit()) { return None; }
      let meta = path.metadata().ok()?;
      Some((meta.modified().ok()?, path, meta.len()))
    }).collect();
    entries.sort_by(|a,b| b.0.cmp(&a.0));
    let mut bytes = 0u64;
    for (index, (_, old, size)) in entries.into_iter().enumerate() {
      bytes += size;
      if old != path && (index >= 8 || bytes > 2 * 1024 * 1024 * 1024) { let _ = fs::remove_file(old); }
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
  prepare(root, source, true, false)?;
  let video = cached_path(root, source, "mp4")?;
  let mut hash = DefaultHasher::new();
  mix.gains.hash(&mut hash);
  let path = cached_path(root, source, &format!("{:016x}.mix.mp4", hash.finish()))?;
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
        "aac",
        "-b:a",
        "192k",
        "-movflags",
        "+faststart",
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
    assert_eq!(first.len(), 41);
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
    let cached = cached_path(&root, &source, "mp4")?;
    let cache_modified = cached.metadata()?.modified()?;
    assert!(prepare(&root, &source, true, false)?);
    assert_eq!(cached.metadata()?.modified()?, cache_modified);
    let second = root.join("second.mp4");
    fs::copy(&source, &second)?;
    prepare(&root, &second, true, false)?;
    assert!(cached.is_file(), "Opening another clip must retain the prior preview");
    let hevc = root.join("hevc.mp4");
    let result = ffmpeg().args(["-nostdin", "-threads", "2", "-i"]).arg(&source)
      .args(["-map", "0:v:0", "-an", "-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "pools=2:frame-threads=1", "-tag:v", "hvc1"])
      .arg(&hevc).output()?;
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    assert!(!prepare(&root, &hevc, false, true)?, "Native HEVC must avoid transcoding");
    assert!(prepare(&root, &hevc, true, false)?);
    let proxy = cached_path(&root, &hevc, "mp4")?;
    let probe = Command::new(crate::maintenance::executable("ffprobe"))
      .creation_flags(0x0800_0000).args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
      .arg(proxy).output()?;
    assert!(probe.status.success());
    assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "h264");
    fs::remove_dir_all(root)?;
    Ok(())
  }
}
