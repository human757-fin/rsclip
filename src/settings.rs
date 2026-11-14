use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
  fs, io,
  path::{Path, PathBuf},
  process::Command,
};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
  #[default]
  H264,
  Hevc,
  Av1,
}

impl Codec {
  fn encoder(self) -> &'static str {
    match self {
      Self::H264 => "h264_nvenc",
      Self::Hevc => "hevc_nvenc",
      Self::Av1 => "av1_nvenc",
    }
  }

  pub fn mp4_tag(self) -> &'static str {
    match self {
      Self::H264 => "avc1",
      Self::Hevc => "hvc1",
      Self::Av1 => "av01",
    }
  }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Compression {
  #[default]
  Cbr,
  Vbr,
  Quality,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
  pub seconds: u64,
  pub long_seconds: u64,
  pub encoding: Encoding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Encoding {
  pub output: u64,
  pub fps: u64,
  pub height: u64,
  pub codec: Codec,
  compression: Compression,
  bitrate: u64,
  quality: u64,
  preset: u64,
}

impl Default for Settings {
  fn default() -> Self {
    Self {
      seconds: 30,
      long_seconds: 120,
      encoding: Encoding::default(),
    }
  }
}

impl Default for Encoding {
  fn default() -> Self {
    Self {
      output: 0,
      fps: 60,
      height: 0,
      codec: Codec::default(),
      compression: Compression::default(),
      bitrate: 12,
      quality: 23,
      preset: 1,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct General {
  pub auto_buffer: bool,
  pub start_hidden: bool,
  pub save_sounds: bool,
  pub capture_overlay: bool,
  pub overlay_stream_proof: bool,
  pub error_logging: bool,
  pub error_log_folder: Option<PathBuf>,
  pub media_folder: Option<PathBuf>,
  pub library_folders: Vec<PathBuf>,
  pub launch_on_login: Option<bool>,
}

impl Default for General {
  fn default() -> Self {
    Self {
      auto_buffer: true,
      start_hidden: false,
      save_sounds: true,
      capture_overlay: true,
      overlay_stream_proof: true,
      error_logging: false,
      error_log_folder: None,
      media_folder: None,
      library_folders: Vec::new(),
      launch_on_login: None,
    }
  }
}

impl General {
  pub fn load(path: &Path) -> Result<Self> {
    load_json(path)
  }

  pub fn save(&self, path: &Path) -> Result<()> {
    save_json(path, self).context("Could not save general settings")
  }
}

pub(crate) fn load_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> Result<T> {
  match fs::read(path) {
    Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(T::default()),
    Err(error) => Err(error.into()),
  }
}

pub(crate) fn save_json(path: &Path, value: &impl Serialize) -> Result<()> {
  let temp = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
  let result =
    fs::write(&temp, serde_json::to_vec_pretty(value)?).and_then(|()| fs::rename(&temp, path));
  if result.is_err() {
    let _ = fs::remove_file(temp);
  }
  Ok(result?)
}

impl Settings {
  pub fn validate(self) -> Result<Self> {
    if !(5..=600).contains(&self.seconds)
      || !(5..=600).contains(&self.long_seconds)
      || self.long_seconds < self.seconds
    {
      bail!("Choose lengths from 5 to 600 seconds. Long must be at least as long as Short.");
    }
    let encoding = self.encoding;
    if ![0, 720, 1080, 1440, 2160].contains(&encoding.height) { bail!("Choose a listed capture resolution."); }
    if encoding.output > 15 || !(15..=240).contains(&encoding.fps) {
      bail!("Choose a display index from 0 to 15 and a frame rate from 15 to 240 FPS.");
    }
    if !(2..=80).contains(&encoding.bitrate)
      || !(1..=51).contains(&encoding.quality)
      || ![1, 3, 5].contains(&encoding.preset)
    {
      bail!("Choose a bitrate from 2 to 80 Mbps, a quality from 1 to 51, and a listed preset.");
    }
    Ok(self)
  }

  pub fn load(path: &Path) -> Result<Self> {
    load_json::<Self>(path)?.validate()
  }

  pub fn save(self, path: &Path) -> Result<()> {
    save_json(path, &self).context("Could not save capture settings")
  }
}

impl Encoding {
  pub fn source(self) -> String {
    let mut source = format!("ddagrab=output_idx={}:framerate={}:draw_mouse=0", self.output, self.fps);
    if self.height != 0 {
      source.push_str(&format!(r",hwdownload,format=bgra,scale=w=-2:h=trunc(min({}\,ih)/2)*2:flags=bilinear,format=nv12", self.height));
    }
    source
  }

  pub fn args(self, command: &mut Command, segment: u64) {
    let bitrate = format!("{}M", self.bitrate);
    command.args([
      "-c:v",
      self.codec.encoder(),
      "-preset",
      &format!("p{}", self.preset),
    ]);
    match self.compression {
      Compression::Cbr => {
        command.args([
          "-rc", "cbr", "-b:v", &bitrate, "-maxrate", &bitrate, "-bufsize", &bitrate,
        ]);
      }
      Compression::Vbr => {
        let peak = format!("{}M", self.bitrate * 2);
        command.args([
          "-rc", "vbr", "-b:v", &bitrate, "-maxrate", &peak, "-bufsize", &peak,
        ]);
      }
      Compression::Quality => {
        command.args([
          "-rc",
          "vbr",
          "-cq",
          &self.quality.to_string(),
          "-b:v",
          "0",
          "-maxrate",
          &bitrate,
          "-bufsize",
          &bitrate,
        ]);
      }
    }
    if self.codec == Codec::H264 {
      command.args(["-profile:v", "high"]);
    }
    command.args([
      "-bf",
      "0",
      "-g",
      &(self.fps * segment).to_string(),
      "-forced-idr",
      "1",
      "-force_key_frames",
      &format!("expr:gte(t,n_forced*{segment})"),
      "-tag:v",
      self.codec.mp4_tag(),
    ]);
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn stream_proof_defaults_on_and_persists_when_disabled() {
    let legacy: General = serde_json::from_str(r#"{"captureOverlay":true}"#).unwrap();
    assert!(legacy.overlay_stream_proof);
    assert!(legacy.error_log_folder.is_none());
    let mut config = legacy;
    config.overlay_stream_proof = false;
    config.error_log_folder = Some(std::env::current_dir().unwrap().join("custom-logs"));
    let restored: General = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
    assert!(!restored.overlay_stream_proof);
    assert!(restored.capture_overlay);
    assert_eq!(restored.error_log_folder, config.error_log_folder);
  }

  #[test]
  fn accepts_custom_lengths_and_fps_and_rejects_invalid_settings() {
    let mut settings = Settings {
      seconds: 17,
      long_seconds: 300,
      ..Settings::default()
    };
    settings.encoding.fps = 97;
    assert!(settings.validate().is_ok());
    settings.long_seconds = 16;
    assert!(settings.validate().is_err());
    settings.long_seconds = 300;
    settings.encoding.fps = 241;
    assert!(settings.validate().is_err());
  }

  #[test]
  fn capture_resolution_round_trips_and_old_profiles_remain_native() {
    let legacy: Encoding = serde_json::from_str(r#"{"fps":60}"#).unwrap();
    assert_eq!(legacy.height, 0);
    assert!(!legacy.source().contains("hwdownload"));
    let mut config = Settings::default();
    config.encoding.height = 720;
    let loaded: Settings = serde_json::from_slice(&serde_json::to_vec(&config).unwrap()).unwrap();
    assert_eq!(loaded.encoding.height, 720);
    assert!(loaded.validate().is_ok());
    assert!(loaded.encoding.source().contains(r"trunc(min(720\,ih)/2)*2"));
    config.encoding.height = 721;
    assert!(config.validate().is_err());
  }

  #[test]
  fn every_codec_and_compression_mode_builds_bounded_mp4_arguments() {
    for codec in [Codec::H264, Codec::Hevc, Codec::Av1] {
      for compression in [Compression::Cbr, Compression::Vbr, Compression::Quality] {
        let encoding = Encoding {
          codec,
          compression,
          ..Encoding::default()
        };
        let mut command = Command::new("ffmpeg");
        encoding.args(&mut command, 2);
        let args: Vec<_> = command
          .get_args()
          .map(|arg| arg.to_str().unwrap())
          .collect();
        assert!(args
          .windows(2)
          .any(|args| args == ["-tag:v", codec.mp4_tag()]));
        assert!(args
          .windows(2)
          .any(|args| args == ["-c:v", codec.encoder()]));
        assert!(args.windows(2).any(|args| args
          == [
            "-maxrate",
            if compression == Compression::Vbr {
              "24M"
            } else {
              "12M"
            }
          ]));
      }
    }
  }
}
