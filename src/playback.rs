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

pub const SECTION_SECONDS: f64 = 12.0;

pub fn section(root: &Path, source: &Path, body: &serde_json::Value) -> Result<serde_json::Value> {
  use serde_json::json;
  let info = Command::new(crate::maintenance::executable("ffprobe"))
    .creation_flags(0x0800_0000)
    .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type,codec_name", "-of", "json"])
    .arg(source).output()?;
  if !info.status.success() { bail!("Could not read clip metadata: {}", String::from_utf8_lossy(&info.stderr)); }
  let info: serde_json::Value = serde_json::from_slice(&info.stdout)?;
  let duration = info["format"]["duration"].as_str().and_then(|v| v.parse::<f64>().ok())
    .filter(|v| v.is_finite() && *v > 0.0).context("Clip duration is unavailable")?;
  let codec = info["streams"].as_array().and_then(|s| s.iter().find(|s| s["codec_type"] == "video"))
    .and_then(|s| s["codec_name"].as_str()).unwrap_or("");
  let channels = audio_channels(&probe_audio(source)?);
  if body.get("gains").is_none() && !body["force"].as_bool().unwrap_or(false)
    && (codec == "h264" || (codec == "hevc" && body["nativeHevc"].as_bool().unwrap_or(false))) {
    return Ok(json!({"proxy":false,"duration":duration,"offset":0,"channels":channels}));
  }
  let position = body["position"].as_f64().unwrap_or(0.0);
  if !position.is_finite() || position < 0.0 { bail!("Choose a valid playback position"); }
  let index = (position.min((duration - 0.001).max(0.0)) / SECTION_SECONDS).floor() as u64;
  let offset = index as f64 * SECTION_SECONDS;
  let mix = crate::mixer::MixerSettings::default().clip_mix(body)?;
  let mut key = DefaultHasher::new();
  body.get("gains").is_some().hash(&mut key);
  mix.gains.hash(&mut key);
  let path = cached_path(root, source, &format!("{:016x}.seg.{index}.mp4", key.finish()))?;
  let _guard = VIDEO_JOBS.lock().unwrap();
  if !path.is_file() {
    let audio = probe_audio(source)?;
    let encode = |hardware| {
      let mut command = ffmpeg();
      command.args(["-nostdin", "-threads", "2", "-filter_threads", "2", "-ss", &offset.to_string(), "-i"])
        .arg(source).args(["-t", &SECTION_SECONDS.to_string(), "-map", "0:v:0"]);
      if let Some(graph) = body.get("gains").and_then(|_| crate::mixer::export_filter(&audio, &mix)) {
        command.args(["-filter_complex", &graph, "-map", "[audio]"]);
      } else { command.args(["-map", "0:a:0?"]); }
      command.args(["-vf", r"fps=min(source_fps\,60),scale=w=min(1920\,iw):h=min(1080\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p"]);
      if hardware { command.args(["-c:v", "h264_nvenc", "-preset", "p1", "-rc", "vbr", "-cq", "20", "-b:v", "0"]); }
      else { command.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-threads", "2"]); }
      command.args(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"]);
      render(command, &path)
    };
    if encode(true).is_err() { encode(false)?; }
    let mut entries: Vec<_> = fs::read_dir(root)?.flatten().filter_map(|e| {
      let p = e.path();
      if !p.file_name()?.to_str()?.contains(".seg.") { return None; }
      let m = p.metadata().ok()?;
      Some((m.modified().ok()?, p, m.len()))
    }).collect();
    entries.sort_by(|a,b| b.0.cmp(&a.0));
    let mut bytes = 0;
    for (index, (_, old, size)) in entries.into_iter().enumerate() {
      bytes += size;
      if old != path && (index >= 48 || bytes > 1024 * 1024 * 1024) { let _ = fs::remove_file(old); }
    }
  }
  Ok(json!({"proxy":true,"section":true,"duration":duration,"offset":offset,
    "url":format!("/part/{}",path.file_name().unwrap().to_string_lossy()),"channels":channels}))
}

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
  if let Err(error) = &result {
    crate::diagnostics::record("preview renderer", &format!("{error:#}"));
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

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  #[ignore = "requires FFmpeg on PATH"]
  fn playback_sections_preserve_timeline_audio_and_cache() -> Result<()> {
    use serde_json::json;
    let root = std::env::current_dir()?.join("target").join(format!("sections-test-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root)?;
    let source = root.join("source.mp4");
    let output = ffmpeg().args(["-nostdin", "-f", "lavfi", "-i", "color=size=160x90:rate=15:duration=30",
      "-f", "lavfi", "-i", "sine=frequency=440:duration=30", "-map", "0:v", "-map", "1:a", "-map", "1:a",
      "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-metadata:s:a:1", "handler_name=rsclip:microphone:Microphone"])
      .arg(&source).output()?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(section(&root, &source, &json!({}))?["proxy"], false);
    let muted = json!({"force":true,"gains":{"microphone":0},"position":0});
    let first = section(&root, &source, &muted)?;
    assert_eq!(first["offset"], 0.0);
    assert!(first["duration"].as_f64().unwrap() >= 30.0);
    let path = root.join(first["url"].as_str().unwrap().trim_start_matches("/part/"));
    let modified = path.metadata()?.modified()?;
    assert_eq!(section(&root, &source, &muted)?["url"], first["url"]);
    assert_eq!(path.metadata()?.modified()?, modified);
    let next = section(&root, &source, &json!({"force":true,"gains":{"microphone":0},"position":25}))?;
    assert_eq!(next["offset"], 24.0);
    assert!(path.is_file());
    assert_ne!(first["url"], next["url"]);
    let full_volume = section(&root, &source, &json!({"force":true,"gains":{"microphone":100},"position":0}))?;
    assert_ne!(first["url"], full_volume["url"]);
    let output = Command::new(crate::maintenance::executable("ffprobe")).creation_flags(0x0800_0000)
      .args(["-v", "error", "-show_entries", "format=duration:stream=codec_name", "-of", "json"]).arg(&path).output()?;
    assert!(output.status.success());
    let info: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let duration: f64 = info["format"]["duration"].as_str().unwrap().parse()?;
    assert!((11.8..12.3).contains(&duration), "Only a 12-second section should be converted: {duration}");
    assert_eq!(info["streams"][0]["codec_name"], "h264");
    assert_eq!(probe_audio(&path)?["streams"].as_array().unwrap().len(), 1);
    let decoded = ffmpeg().arg("-i").arg(&path).args(["-map", "0:a:0", "-f", "s16le", "-"]).output()?;
    assert!(decoded.status.success());
    assert!(!decoded.stdout.is_empty());
    assert!(decoded.stdout.iter().all(|&b| b == 0));
    assert!(section(&root, &source, &json!({"force":true,"position":-1})).is_err());
    fs::remove_dir_all(root)?;
    Ok(())
  }
}
