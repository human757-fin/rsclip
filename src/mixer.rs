use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
  collections::BTreeMap,
  fs::{self, File},
  io::{self, Seek, SeekFrom, Write},
  path::{Path, PathBuf},
  sync::{Arc, Mutex},
};

pub const RATE: usize = 48_000;
pub const CHANNELS: usize = 2;
pub const BLOCK: usize = RATE / 100;
pub const SEGMENT_SECONDS: usize = 2;
pub const SEGMENT_BYTES: usize = RATE * CHANNELS * 2 * SEGMENT_SECONDS;
pub type Sources = Arc<Mutex<BTreeMap<String, String>>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct MixerSettings {
  pub microphone: bool,
  pub per_app: bool,
  pub gains: BTreeMap<String, u16>,
  pub presets: BTreeMap<String, BTreeMap<String, u16>>,
  pub names: BTreeMap<String, String>,
}

impl Default for MixerSettings {
  fn default() -> Self {
    Self {
      microphone: false,
      per_app: true,
      gains: BTreeMap::new(),
      presets: BTreeMap::new(),
      names: BTreeMap::new(),
    }
  }
}

impl MixerSettings {
  pub fn clip_mix(&self, body: &serde_json::Value) -> Result<Self> {
    let mut result = self.clone();
    if let Some(gains) = body.get("gains") {
      let gains: BTreeMap<String, u16> = serde_json::from_value(gains.clone())?;
      if gains.len() > 128 || gains.iter().any(|(id, gain)| !valid_id(id) || *gain > 200) {
        bail!("Invalid clip volumes");
      }
      result.gains = gains;
    }
    Ok(result)
  }

  pub fn gain(&self, id: &str) -> f32 {
    f32::from(self.gains.get(id).copied().unwrap_or(100).min(200)) / 100.0
  }

  pub fn apply(&mut self, body: &serde_json::Value) -> Result<()> {
    match body["action"].as_str() {
      Some("microphone") => {
        self.microphone = body["enabled"]
          .as_bool()
          .context("Invalid microphone setting")?
      }
      Some("perApp") => {
        self.per_app = body["enabled"]
          .as_bool()
          .context("Invalid per-app setting")?
      }
      Some("gain") => {
        let id = body["source"]
          .as_str()
          .filter(|id| valid_id(id))
          .context("Invalid audio source")?;
        let gain = body["gain"]
          .as_u64()
          .filter(|gain| *gain <= 200)
          .context("Choose a gain from 0 to 200 percent")?;
        self.gains.insert(id.into(), gain as u16);
      }
      Some("savePreset" | "loadPreset") => {
        let name = body["name"]
          .as_str()
          .map(str::trim)
          .filter(|name| !name.is_empty() && name.len() <= 48)
          .context("Use a preset name up to 48 bytes")?;
        if body["action"] == "savePreset" {
          self.presets.insert(name.into(), self.clip_mix(body)?.gains);
        } else {
          self.gains = self
            .presets
            .get(name)
            .cloned()
            .context("Audio preset not found")?;
        }
      }
      _ => bail!("Unknown audio mixer setting"),
    }
    Ok(())
  }
}

fn valid_id(id: &str) -> bool {
  matches!(id, "desktop" | "microphone")
    || id
      .strip_prefix("app-")
      .is_some_and(|key| key.len() == 16 && key.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

pub fn part_index(path: &Path) -> Option<usize> {
  path
    .file_stem()?
    .to_str()?
    .strip_prefix("part-")?
    .parse()
    .ok()
}

pub fn segment_folder(buffer: &Path, index: usize) -> PathBuf {
  buffer.join("audio").join(format!("{index:09}"))
}

pub struct Stems {
  root: PathBuf,
  segment: usize,
  files: BTreeMap<String, File>,
}

impl Stems {
  pub fn new(root: PathBuf) -> Self {
    Self {
      root,
      segment: 0,
      files: BTreeMap::new(),
    }
  }

  pub fn write(&mut self, id: &str, frame: usize, pcm: &[i16]) -> Result<()> {
    let frames_per_segment = RATE * SEGMENT_SECONDS;
    let segment = frame / frames_per_segment;
    if segment != self.segment {
      self.files.clear();
      self.segment = segment;
    }
    if pcm.iter().all(|&sample| sample == 0) {
      return Ok(());
    }
    if !self.files.contains_key(id) {
      let folder = segment_folder(&self.root, segment);
      fs::create_dir_all(&folder)?;
      self.files.insert(id.into(), File::create(folder.join(id))?);
    }
    let file = self.files.get_mut(id).unwrap();
    file.seek(SeekFrom::Start(
      ((frame % frames_per_segment) * CHANNELS * 2) as u64,
    ))?;
    let mut bytes = [0; BLOCK * CHANNELS * 2];
    for (chunk, sample) in bytes.chunks_exact_mut(2).zip(pcm) {
      chunk.copy_from_slice(&sample.to_le_bytes());
    }
    file.write_all(&bytes[..pcm.len() * 2])?;
    Ok(())
  }
}

pub struct JoinTracks {
  root: PathBuf,
  pub inputs: Vec<(String, String, PathBuf)>,
}

impl JoinTracks {
  pub fn prepare(buffer: &Path, parts: &[PathBuf], sources: &Sources) -> Result<Self> {
    let mut result = Self {
      root: buffer.join(format!("mix-{}", uuid::Uuid::new_v4())),
      inputs: Vec::new(),
    };
    let folders: Vec<_> = parts
      .iter()
      .filter_map(|part| part_index(part))
      .map(|index| segment_folder(buffer, index))
      .collect();
    let sources = sources.lock().unwrap().clone();
    let zeros = vec![0; SEGMENT_BYTES];
    for (id, name) in sources {
      if !folders.iter().any(|folder| folder.join(&id).is_file()) {
        continue;
      }
      fs::create_dir_all(&result.root)?;
      let path = result.root.join(format!("{id}.raw"));
      let mut track = File::create(&path)?;
      for folder in &folders {
        use std::io::Read;
        let written = match File::open(folder.join(&id)) {
          Ok(file) => io::copy(&mut file.take(SEGMENT_BYTES as u64), &mut track)? as usize,
          Err(error) if error.kind() == io::ErrorKind::NotFound => 0,
          Err(error) => return Err(error.into()),
        };
        track.write_all(&zeros[..SEGMENT_BYTES - written])?;
      }
      result.inputs.push((id, name, path));
    }
    Ok(result)
  }
}

impl Drop for JoinTracks {
  fn drop(&mut self) {
    if self.root.is_dir() {
      let _ = fs::remove_dir_all(&self.root);
    }
  }
}

pub fn export_filter(probe: &serde_json::Value, settings: &MixerSettings) -> Option<String> {
  let streams = probe["streams"].as_array()?;
  let mut graph = String::new();
  let mut mix = String::new();
  let mut count = 0;
  for (index, stream) in streams.iter().enumerate() {
    let Some(handler) = stream["tags"]["handler_name"].as_str() else {
      continue;
    };
    let Some((id, _)) = handler
      .strip_prefix("rsclip:")
      .and_then(|name| name.split_once(':'))
    else {
      continue;
    };
    if !valid_id(id) {
      continue;
    }
    graph.push_str(&format!(
      "[0:a:{index}]volume={}[mix{count}];",
      settings.gain(id)
    ));
    mix.push_str(&format!("[mix{count}]"));
    count += 1;
  }
  if count == 0 {
    return None;
  }
  Some(format!(
    "{graph}{mix}amix=inputs={count}:normalize=0,alimiter=limit=0.98:level=0:latency=1[audio]"
  ))
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn clip_mix_and_presets_do_not_change_capture_volumes() -> Result<()> {
    let mut mixer = MixerSettings::default();
    mixer.gains.insert("microphone".into(), 80);
    let body = json!({"action":"savePreset","name":"Clip voice","gains":{"microphone":25}});
    mixer.apply(&body)?;
    assert_eq!(mixer.gains["microphone"], 80);
    assert_eq!(mixer.presets["Clip voice"]["microphone"], 25);
    assert_eq!(mixer.clip_mix(&body)?.gain("microphone"), 0.25);
    assert!(mixer.clip_mix(&json!({"gains":{"microphone":201}})).is_err());
    assert!(mixer.clip_mix(&json!({"gains":{"invalid":100}})).is_err());
    Ok(())
  }

  #[test]
  fn presets_restore_gains_and_export_uses_only_isolated_tracks() -> Result<()> {
    let mut mixer = MixerSettings::default();
    mixer.apply(&json!({"action":"gain","source":"microphone","gain":60}))?;
    mixer.apply(&json!({"action":"savePreset","name":"Voice"}))?;
    mixer.apply(&json!({"action":"gain","source":"microphone","gain":20}))?;
    mixer.apply(&json!({"action":"loadPreset","name":"Voice"}))?;
    let graph = export_filter(
      &json!({"streams":[
        {"tags":{"handler_name":"SoundHandler"}},
        {"tags":{"handler_name":"rsclip:microphone:Microphone"}},
        {"tags":{"handler_name":"rsclip:app-0000000000000001:Game"}}
      ]}),
      &mixer,
    )
    .unwrap();
    assert!(!graph.contains("[0:a:0]"));
    assert!(graph.contains("[0:a:1]volume=0.6"));
    assert!(graph.contains("[0:a:2]volume=1"));
    assert!(graph.contains("amix=inputs=2"));
    Ok(())
  }

  #[test]
  fn saved_stem_includes_silence_before_an_app_joins() -> Result<()> {
    let root = std::env::current_dir()?
      .join("target")
      .join(format!("audio-test-{}", uuid::Uuid::new_v4()));
    let mut stems = Stems::new(root.clone());
    stems.write(
      "microphone",
      RATE * SEGMENT_SECONDS + BLOCK,
      &[123; BLOCK * CHANNELS],
    )?;
    drop(stems);
    let sources = Arc::new(Mutex::new(BTreeMap::from([(
      "microphone".into(),
      "Mic".into(),
    )])));
    let parts = [
      root.join("part-000000000.mp4"),
      root.join("part-000000001.mp4"),
    ];
    let tracks = JoinTracks::prepare(&root, &parts, &sources)?;
    let pcm = fs::read(&tracks.inputs[0].2)?;
    assert_eq!(pcm.len(), 2 * SEGMENT_BYTES);
    let first_sample = SEGMENT_BYTES + BLOCK * CHANNELS * 2;
    assert!(pcm[..first_sample].iter().all(|&byte| byte == 0));
    assert_eq!(&pcm[first_sample..first_sample + 2], &123i16.to_le_bytes());
    drop(tracks);
    fs::remove_dir_all(root)?;
    Ok(())
  }
}
