use crate::{
  audio, desktop,
  mixer::{self, JoinTracks, MixerSettings, Sources},
  overlay::{Feedback, OverlayEvent},
  settings::{self, Codec, Encoding, General, Settings},
};
use anyhow::{bail, Context, Result};
use chrono::Local;
use serde_json::{json, Value};
use std::{
  collections::{BTreeMap, HashMap},
  env,
  fs::{self, File},
  os::windows::process::CommandExt,
  path::{Path, PathBuf},
  process::{Child, Command, Stdio},
  sync::{atomic::Ordering, mpsc, Arc, Mutex},
  thread,
  time::{Duration, Instant},
};
use uuid::Uuid;

const SEGMENT: u64 = mixer::SEGMENT_SECONDS as u64;

pub struct Recorder {
  data: PathBuf,
  pub media: PathBuf,
  buffer: Option<PathBuf>,
  buffer_codec: Codec,
  child: Option<Child>,
  audio: Option<audio::Loopback>,
  ffmpeg: bool,
  settings: Settings,
  general: General,
  mixer: Arc<Mutex<MixerSettings>>,
  sources: Sources,
  launch_on_login: bool,
  recording: Option<Recording>,
  feedback: mpsc::Sender<OverlayEvent>,
  pub notice: String,
  error: String,
  pub hotkey_error: String,
  pub hotkeys: Value,
}

struct Recording {
  first_part: Option<PathBuf>,
  resume_buffer: bool,
}

impl Recording {
  fn includes(&self, part: &PathBuf) -> bool {
    self.first_part.as_ref().is_none_or(|first| part >= first)
  }
}

pub fn ffmpeg() -> Command {
  let mut command = Command::new(crate::maintenance::executable("ffmpeg"));
  command
    .creation_flags(0x0800_0000)
    .args(["-hide_banner", "-loglevel", "error"]);
  command
}

fn default_audio_sources() -> Sources {
  Arc::new(Mutex::new(BTreeMap::from([
    ("desktop".into(), "Desktop audio fallback".into()),
    ("microphone".into(), "Microphone".into()),
  ])))
}

fn snapshot_command(output: u64, filters: &str) -> Command {
  let mut command = ffmpeg();
  command.args([
    "-nostdin",
    "-filter_complex",
    &format!("ddagrab=output_idx={output}:draw_mouse=0,hwdownload,format=bgra{filters}[out]"),
    "-map",
    "[out]",
    "-frames:v",
    "1",
  ]);
  command
}

pub fn preview_monitor(output: u64) -> Result<Vec<u8>> {
  let filters = r",scale=w=min(480\,iw):h=min(300\,ih):force_original_aspect_ratio=decrease";
  let result = snapshot_command(output, filters)
    .args(["-c:v", "png", "-f", "image2pipe", "pipe:1"])
    .output()
    .context("Could not run FFmpeg for the monitor preview")?;
  if !result.status.success() || result.stdout.is_empty() {
    let error = String::from_utf8_lossy(&result.stderr);
    bail!(
      "Could not preview this monitor. {}",
      error.trim().chars().take(360).collect::<String>()
    );
  }
  Ok(result.stdout)
}

impl Recorder {
  pub fn new(feedback: mpsc::Sender<OverlayEvent>) -> Result<Self> {
    let app_data = PathBuf::from(env::var_os("LOCALAPPDATA").context("LOCALAPPDATA is missing")?);
    let mut data = app_data.join("rsclip");
    let legacy = app_data.join("Clipline");
    if !data.join("media").exists() && legacy.join("media").is_dir() {
      data = legacy;
    }
    let default_media = data.join("media");
    fs::create_dir_all(&default_media)?;
    let general_path = data.join("general.json");
    let mut general = General::load(&general_path).unwrap_or_default();
    let mut launch_on_login = desktop::launches_on_login();
    let startup_warning = if general.launch_on_login == Some(true) {
      match desktop::set_launch_on_login(true) {
        Ok(()) => {
          launch_on_login = true;
          None
        }
        Err(error) => Some(format!("Could not restore Windows startup. {error:#}")),
      }
    } else if general.launch_on_login.is_none() && launch_on_login {
      general.launch_on_login = Some(true);
      general
        .save(&general_path)
        .err()
        .map(|error| format!("{error:#}"))
    } else {
      None
    };
    let media = general
      .media_folder
      .as_ref()
      .filter(|folder| folder.is_absolute() && folder.is_dir())
      .cloned()
      .unwrap_or_else(|| default_media.clone());
    let (settings, notice) = match Settings::load(&data.join("capture.json")) {
      Ok(settings) => (settings, String::new()),
      Err(_) => (
        Settings::default(),
        "Saved capture settings could not be read. Defaults are active.".into(),
      ),
    };
    let notice = if general
      .media_folder
      .as_ref()
      .is_some_and(|folder| folder != &media)
    {
      "Your selected media folder is unavailable. Using the default folder.".into()
    } else {
      notice
    };
    let mixer: MixerSettings = settings::load_json(&data.join("audio.json")).unwrap_or_default();
    let sources = default_audio_sources();
    sources.lock().unwrap().extend(mixer.names.clone());
    Ok(Self {
      data,
      media,
      buffer: None,
      buffer_codec: Codec::default(),
      child: None,
      audio: None,
      ffmpeg: ffmpeg()
        .arg("-version")
        .output()
        .is_ok_and(|result| result.status.success()),
      settings,
      general,
      mixer: Arc::new(Mutex::new(mixer)),
      sources,
      launch_on_login,
      recording: None,
      feedback,
      notice: startup_warning.unwrap_or(notice),
      error: String::new(),
      hotkey_error: String::new(),
      hotkeys: json!({}),
    })
  }

  pub fn playback_cache(&self) -> PathBuf {
    self.data.join("playback-cache")
  }

  pub fn hotkey_path(&self) -> PathBuf {
    self.data.join("hotkeys.json")
  }

  pub fn refresh_ffmpeg(&mut self) {
    let was_missing = !self.ffmpeg;
    self.ffmpeg = ffmpeg()
      .arg("-version")
      .output()
      .is_ok_and(|result| result.status.success());
    if self.ffmpeg && (was_missing || self.error.contains("nvenc API version")) { self.error.clear(); }
    self.start_automatically();
  }

  pub fn starts_hidden(&self) -> bool {
    self.general.start_hidden
  }

  pub fn start_automatically(&mut self) {
    if self.general.auto_buffer && self.ffmpeg && !self.active() {
      if let Err(error) = self.start() {
        self.error = format!("Could not start the replay buffer. {error:#}");
      }
    }
  }

  pub fn set_media_folder(&mut self, folder: PathBuf) -> Result<()> {
    if !folder.is_absolute() || !folder.is_dir() {
      bail!("Choose an available folder.");
    }
    let test = folder.join(format!(".rsclip-write-test-{}", Uuid::new_v4()));
    let file = fs::OpenOptions::new()
      .write(true)
      .create_new(true)
      .open(&test)
      .context("rsclip cannot write to that folder")?;
    drop(file);
    fs::remove_file(test)?;
    let mut general = self.general.clone();
    general
      .library_folders
      .retain(|known| known != &folder && known != &self.media);
    if self.media != folder {
      general.library_folders.insert(0, self.media.clone());
    }
    general.media_folder = Some(folder.clone());
    general.save(&self.data.join("general.json"))?;
    self.general = general;
    self.media = folder;
    self.notice = "Media folder updated".into();
    Ok(())
  }

  pub fn add_library_folder(&mut self, folder: PathBuf) -> Result<()> {
    if !folder.is_absolute() || !folder.is_dir() {
      bail!("Choose an available folder.");
    }
    if !self.media_folders().contains(&folder) {
      let mut general = self.general.clone();
      general.library_folders.push(folder);
      general.save(&self.data.join("general.json"))?;
      self.general = general;
    }
    self.notice = "Library folder added".into();
    Ok(())
  }

  fn update_general(&mut self, body: &Value) -> Result<()> {
    let enabled = body["enabled"].as_bool().context("Invalid setting value")?;
    let mut general = self.general.clone();
    let setting = body["setting"].as_str();
    match setting {
      Some("launchOnLogin") => general.launch_on_login = Some(enabled),
      Some("autoBuffer") => general.auto_buffer = enabled,
      Some("startHidden") => general.start_hidden = enabled,
      Some("captureOverlay") => general.capture_overlay = enabled,
      Some("saveSounds") => general.save_sounds = enabled,
      _ => bail!("Unknown general setting."),
    }
    let startup_changed = setting == Some("launchOnLogin");
    if startup_changed {
      desktop::set_launch_on_login(enabled).context("Could not update Windows startup")?;
    }
    if let Err(error) = general.save(&self.data.join("general.json")) {
      if startup_changed {
        let _ = desktop::set_launch_on_login(self.launch_on_login);
      }
      return Err(error);
    }
    self.general = general;
    if startup_changed {
      self.launch_on_login = enabled;
    }
    if setting == Some("captureOverlay") && !enabled {
      let _ = self.feedback.send(OverlayEvent::Disable);
    }
    self.notice = "Settings saved".into();
    Ok(())
  }

  pub fn mixer_settings(&self) -> MixerSettings {
    self.mixer.lock().unwrap().clone()
  }

  fn update_mixer(&mut self, body: &Value) -> Result<()> {
    let mut config = self.mixer_settings();
    config.apply(body)?;
    config.names = self.sources.lock().unwrap().clone();
    settings::save_json(&self.data.join("audio.json"), &config)
      .context("Could not save audio settings")?;
    *self.mixer.lock().unwrap() = config;
    self.notice = "Audio settings saved".into();
    Ok(())
  }

  fn active(&self) -> bool {
    self.child.is_some()
  }

  fn start(&mut self) -> Result<()> {
    if !self.ffmpeg {
      bail!("FFmpeg is not ready. Open Settings / General / Software for setup progress, the exact error, and Retry FFmpeg setup.");
    }
    if let Some(previous) = &self.buffer {
      fs::remove_dir_all(previous)?;
      self.buffer = None;
    }
    self.error.clear();
    let buffer = self.data.join(format!("buffer-{}", Uuid::new_v4()));
    fs::create_dir(&buffer)?;
    self.buffer = Some(buffer.clone());
    let audio = audio::open(buffer.clone(), self.mixer.clone(), self.sources.clone())?;
    let encoding = self.settings.encoding;
    let capture = encoding.source();
    let mut command = ffmpeg();
    command.args([
      "-nostdin",
      "-thread_queue_size",
      "64",
      "-probesize",
      "32",
      "-analyzeduration",
      "1",
      "-f",
      "s16le",
      "-ar",
      &mixer::RATE.to_string(),
      "-ac",
      &mixer::CHANNELS.to_string(),
      "-i",
      "pipe:0",
      "-f",
      "lavfi",
      "-i",
      &capture,
      "-map",
      "1:v:0",
      "-map",
      "0:a:0",
    ]);
    encoding.args(&mut command, SEGMENT);
    let mut child = command
      .args([
        "-c:a",
        "aac",
        "-b:a",
        "160k",
        "-ac",
        "2",
        "-ar",
        "48000",
        "-shortest",
        "-f",
        "segment",
        "-segment_time",
        &SEGMENT.to_string(),
        "-reset_timestamps",
        "1",
        "part-%09d.mp4",
      ])
      .current_dir(&buffer)
      .stdin(Stdio::piped())
      .stdout(Stdio::null())
      .stderr(File::create(buffer.join("ffmpeg.log"))?)
      .spawn()
      .context("Could not start FFmpeg")?;
    if audio
      .input
      .send(child.stdin.take().context("Missing FFmpeg audio pipe")?)
      .is_err()
    {
      let _ = child.kill();
      let _ = child.wait();
      bail!("WASAPI stopped before FFmpeg could start");
    }
    self.buffer_codec = encoding.codec;
    self.child = Some(child);
    self.audio = Some(audio);
    self.notice = "Replay buffer started · desktop audio on".into();
    Ok(())
  }

  fn finish_audio(&mut self) -> Result<()> {
    if let Some(audio) = self.audio.take() {
      audio.stop.store(true, Ordering::Relaxed);
      return audio
        .worker
        .join()
        .map_err(|_| anyhow::anyhow!("Audio capture worker panicked"))?;
    }
    Ok(())
  }

  fn stop(&mut self) -> Result<()> {
    let Some(mut child) = self.child.take() else {
      return Ok(());
    };
    if let Some(audio) = &self.audio {
      audio.stop.store(true, Ordering::Relaxed);
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    let status = loop {
      if let Some(status) = child.try_wait()? {
        break status;
      }
      if Instant::now() >= deadline {
        child.kill()?;
        break child.wait()?;
      }
      thread::sleep(Duration::from_millis(30));
    };
    let audio_result = self.finish_audio();
    if !status.success() {
      self.discard_partial()?;
      bail!("Capture did not shut down cleanly. Completed segments are still available.");
    }
    audio_result?;
    self.notice = "Replay buffer stopped".into();
    Ok(())
  }

  fn parts(&self) -> Result<Vec<PathBuf>> {
    let Some(buffer) = &self.buffer else {
      return Ok(Vec::new());
    };
    let mut files: Vec<_> = fs::read_dir(buffer)?
      .filter_map(|entry| entry.ok().map(|entry| entry.path()))
      .filter(|path| {
        path
          .file_name()
          .and_then(|name| name.to_str())
          .is_some_and(|name| name.starts_with("part-") && name.ends_with(".mp4"))
      })
      .collect();
    files.sort_unstable();
    Ok(files)
  }

  fn discard_partial(&self) -> Result<()> {
    if let Some(path) = self.parts()?.last() {
      fs::remove_file(path)?;
    }
    Ok(())
  }

  pub fn maintain(&mut self) -> Result<()> {
    if let Some(child) = &mut self.child {
      if let Some(status) = child.try_wait()? {
        self.child = None;
        let audio_result = self.finish_audio();
        if !status.success() {
          self.discard_partial()?;
          let log = fs::read_to_string(self.buffer.as_ref().unwrap().join("ffmpeg.log"))
            .unwrap_or_default();
          bail!(
            "Capture stopped. {}",
            log.trim().chars().take(900).collect::<String>()
          );
        }
        audio_result.context("Desktop audio stopped unexpectedly")?;
        self.notice = "Replay buffer ended".into();
      }
    }
    let parts = self.parts()?;
    let retain =
      self.settings.long_seconds.div_ceil(SEGMENT) as usize + 1 + usize::from(self.active());
    let discard = parts.len().saturating_sub(retain);
    for path in parts.into_iter().take(discard) {
      if self
        .recording
        .as_ref()
        .is_some_and(|recording| recording.includes(&path))
      {
        break;
      }
      if let (Some(buffer), Some(index)) = (&self.buffer, mixer::part_index(&path)) {
        let folder = mixer::segment_folder(buffer, index);
        if folder.is_dir() {
          fs::remove_dir_all(folder)?;
        }
      }
      fs::remove_file(path)?;
    }
    Ok(())
  }

  pub fn shutdown(&mut self) {
    let _ = self.stop();
    if self.recording.is_some() && self.finish_recording(false).is_err() {
      return;
    }
    if let Some(buffer) = self.buffer.take() {
      let _ = fs::remove_dir_all(buffer);
    }
  }

  pub fn tick(&mut self) {
    if let Err(error) = self.maintain() {
      self.error = format!("{error:#}");
    }
  }

  fn publish(media: &Path, extension: &str, label: &str, mut command: Command) -> Result<PathBuf> {
    let name = format!(
      "{} {}{label}",
      Local::now().format("%Y-%m-%d %H-%M-%S"),
      &Uuid::new_v4().simple().to_string()[..4]
    );
    let destination = media.join(format!("{name}.{extension}"));
    let temp = media.join(format!(".{name}.{extension}"));
    let result = (|| -> Result<()> {
      let output = command
        .arg(&temp)
        .output()
        .context("Could not launch FFmpeg")?;
      if !output.status.success() {
        let log = String::from_utf8_lossy(&output.stderr);
        bail!(
          "FFmpeg failed. {}",
          log.trim().chars().take(900).collect::<String>()
        );
      }
      fs::rename(&temp, &destination)?;
      Ok(())
    })();
    if result.is_err() {
      let _ = fs::remove_file(temp);
    }
    result.map(|()| destination)
  }

  fn feedback(&self, title: &'static str, source: PathBuf, next_segment: Option<PathBuf>) {
    if self.general.capture_overlay {
      let _ = self.feedback.send(OverlayEvent::Show(Feedback {
        title,
        source,
        cache: self.playback_cache(),
        next_segment,
      }));
    }
  }

  fn save(&mut self, long: bool) -> Result<()> {
    let mut parts = self.parts()?;
    if self.active() {
      parts.pop();
    }
    let seconds = if long {
      self.settings.long_seconds
    } else {
      self.settings.seconds
    };
    let count = seconds.div_ceil(SEGMENT) as usize;
    let parts = &parts[parts.len().saturating_sub(count)..];
    if parts.is_empty() {
      bail!("Start the replay buffer and wait for the first segment.");
    }
    let (label, notice) = if long {
      (" long", "Long clip saved")
    } else {
      (" short", "Short clip saved")
    };
    let path = self.remux(parts, label)?;
    self.feedback(notice, path, None);
    self.notice = notice.into();
    if self.general.save_sounds {
      desktop::play_saved_sound(false);
    }
    Ok(())
  }

  fn remux(&self, parts: &[PathBuf], label: &str) -> Result<PathBuf> {
    let buffer = self.buffer.as_ref().unwrap();
    let mut playlist = String::from("ffconcat version 1.0\n");
    for path in parts {
      let name = path.file_name().unwrap().to_string_lossy();
      playlist.push_str(&format!("file '{name}'\n"));
    }
    fs::write(buffer.join("save.ffconcat"), playlist)?;
    let tracks = JoinTracks::prepare(buffer, parts, &self.sources)?;
    let mut command = ffmpeg();
    command.current_dir(buffer).args([
      "-nostdin",
      "-f",
      "concat",
      "-safe",
      "1",
      "-i",
      "save.ffconcat",
    ]);
    for (_, _, path) in &tracks.inputs {
      command
        .args([
          "-f",
          "s16le",
          "-ar",
          &mixer::RATE.to_string(),
          "-ac",
          "2",
          "-i",
        ])
        .arg(path);
    }
    command.args([
      "-map",
      "0",
      "-c",
      "copy",
      "-tag:v",
      self.buffer_codec.mp4_tag(),
      "-movflags",
      "+faststart",
    ]);
    for (index, (id, name, _)) in tracks.inputs.iter().enumerate() {
      let track = index + 1;
      command.args([
        "-map",
        &format!("{track}:a:0"),
        &format!("-c:a:{track}"),
        "aac",
        &format!("-b:a:{track}"),
        "128k",
        &format!("-disposition:a:{track}"),
        "0",
        &format!("-metadata:s:a:{track}"),
        &format!("handler_name=rsclip:{id}:{name}"),
      ]);
    }
    if !tracks.inputs.is_empty() {
      command.args(["-shortest", "-disposition:a:0", "default"]);
    }
    Self::publish(&self.media, "mp4", label, command)
  }

  fn toggle_recording(&mut self) -> Result<()> {
    if self.recording.is_some() {
      return self.finish_recording(true);
    }
    let resume_buffer = self.active();
    let first_part = if resume_buffer {
      self.parts()?.pop()
    } else {
      self.start()?;
      None
    };
    if let Some(buffer) = &self.buffer {
      let source = first_part
        .clone()
        .unwrap_or_else(|| buffer.join("part-000000000.mp4"));
      let number = source
        .file_stem()
        .and_then(|stem| stem.to_str())
        .and_then(|stem| stem.strip_prefix("part-"))
        .and_then(|number| number.parse::<u64>().ok());
      let next = number.map(|number| buffer.join(format!("part-{:09}.mp4", number + 1)));
      self.feedback("Recording started", source, next);
    }
    self.recording = Some(Recording {
      first_part,
      resume_buffer,
    });
    self.notice = "Recording started".into();
    Ok(())
  }

  fn finish_recording(&mut self, resume: bool) -> Result<()> {
    // A failed stop can still leave completed segments available for export.
    let stop_error = self.stop().err();
    let Some(recording) = self.recording.as_ref() else {
      return Ok(());
    };
    let parts: Vec<_> = self
      .parts()?
      .into_iter()
      .filter(|part| recording.includes(part))
      .collect();
    let resume_buffer = resume && recording.resume_buffer;
    if parts.is_empty() {
      self.recording = None;
      if resume_buffer {
        self.start()?;
      }
      bail!("Recording was too short to save.");
    }
    let saved = self.remux(&parts, " recording").context(
      "Could not save the recording. Change the media folder if needed, then retry Save recording.",
    )?;
    self.recording = None;
    if resume {
      self.feedback("Recording stopped", saved, None);
    }
    if resume_buffer {
      if let Err(error) = self.start() {
        self.error = format!("Recording saved. Replay buffer could not restart. {error:#}");
      }
    }
    self.notice = if stop_error.is_some() {
      "Recording saved from completed segments"
    } else {
      "Recording saved"
    }
    .into();
    Ok(())
  }

  fn screenshot(&mut self) -> Result<()> {
    let mut command = snapshot_command(self.settings.encoding.output, "");
    command.args(["-update", "1"]);
    Self::publish(&self.media, "png", "", command)?;
    self.notice = "Screenshot saved".into();
    if self.general.save_sounds {
      desktop::play_saved_sound(true);
    }
    Ok(())
  }

  pub fn export(
    media: &Path,
    source: &Path,
    body: &Value,
    trim: bool,
    mix: &MixerSettings,
  ) -> Result<&'static str> {
    if source
      .extension()
      .is_none_or(|extension| extension != "mp4")
      || !source.is_file()
    {
      bail!("Clip not found.");
    }
    let mut command = ffmpeg();
    command.arg("-nostdin");
    let span = if trim {
      let (Some(start), Some(end)) = (body["start"].as_f64(), body["end"].as_f64()) else {
        bail!("Choose a valid trim range.");
      };
      if !(0.0 <= start && start < end && end.is_finite()) {
        bail!("Choose a valid trim range.");
      }
      command.args(["-ss", &start.to_string()]);
      Some(end - start)
    } else {
      None
    };
    command
      .args(["-threads", "2", "-filter_threads", "2", "-i"])
      .arg(source);
    if let Some(duration) = span {
      command.args(["-t", &duration.to_string()]);
    }
    let probe = Command::new(crate::maintenance::executable("ffprobe"))
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
      .output()
      .ok()
      .filter(|output| output.status.success())
      .and_then(|output| serde_json::from_slice::<Value>(&output.stdout).ok());
    command.args(["-map", "0:v:0"]);
    if let Some(graph) = probe
      .as_ref()
      .and_then(|probe| mixer::export_filter(probe, mix))
    {
      command.args(["-filter_complex", &graph, "-map", "[audio]"]);
    } else {
      command.args(["-map", "0:a:0?"]);
    }
    command.args([
      "-vf", r"fps=min(source_fps\,60),scale=w=min(1920\,iw):h=min(1080\,ih):force_original_aspect_ratio=decrease:force_divisible_by=2,format=yuv420p",
      "-c:v", "h264_nvenc", "-preset", "p1", "-profile:v", "high", "-b:v", "12M",
      "-c:a", "aac", "-b:a", "160k", "-ac", "2", "-ar", "48000", "-movflags", "+faststart",
    ]);
    Self::publish(media, "mp4", "", command)?;
    Ok(if trim {
      "H.264 trim saved"
    } else {
      "H.264 copy saved to Clips"
    })
  }

  fn verify_encoding(&self, encoding: Encoding) -> Result<()> {
    let mut command = ffmpeg();
    command.args([
      "-nostdin",
      "-f",
      "lavfi",
      "-i",
      &encoding.source(),
      "-frames:v",
      "1",
      "-an",
    ]);
    encoding.args(&mut command, SEGMENT);
    let result = command
      .args([
        "-f",
        "mp4",
        "-movflags",
        "+empty_moov+frag_keyframe",
        "pipe:1",
      ])
      .stdout(Stdio::null())
      .output()
      .context("Could not test capture settings with FFmpeg")?;
    if !result.status.success() {
      let error = String::from_utf8_lossy(&result.stderr);
      bail!(
        "This capture profile did not work with your display, GPU, driver, and FFmpeg build. {}",
        error.trim().chars().take(650).collect::<String>()
      );
    }
    Ok(())
  }

  fn update_settings(&mut self, body: &Value) -> Result<()> {
    let settings = serde_json::from_value::<Settings>(body.clone())
      .context("Invalid capture settings")?
      .validate()?;
    if settings.encoding != self.settings.encoding {
      if self.active() {
        bail!("Stop the replay buffer before changing capture settings.");
      }
      if self.ffmpeg {
        self.verify_encoding(settings.encoding)?;
      } else if settings.encoding.codec != Codec::H264 {
        bail!("Install FFmpeg before testing and selecting this codec.");
      }
    }
    settings.save(&self.data.join("capture.json"))?;
    self.settings = settings;
    self.notice = "Settings saved".into();
    Ok(())
  }

  pub fn act(&mut self, action: &str, body: &Value) -> Result<Value> {
    self.tick();
    match action {
      "toggle" if self.recording.is_some() => {
        bail!("Save the current recording before toggling the buffer.")
      }
      "toggle" if self.active() => self.stop()?,
      "toggle" => self.start()?,
      "record" => self.toggle_recording()?,
      "general" => self.update_general(body)?,
      "mixer" => self.update_mixer(body)?,
      "save" => self.save(false)?,
      "save_long" => self.save(true)?,
      "screenshot" => self.screenshot()?,
      "settings" => self.update_settings(body)?,
      _ => bail!("Unknown action."),
    }
    Ok(self.state())
  }

  pub fn state(&self) -> Value {
    json!({
      "active": self.active(), "recording": self.recording.is_some(), "settings": self.settings, "error": self.error,
      "mixer": self.mixer_settings(), "audioSources": *self.sources.lock().unwrap(),
      "general": { "autoBuffer": self.general.auto_buffer, "startHidden": self.general.start_hidden, "saveSounds": self.general.save_sounds, "captureOverlay": self.general.capture_overlay, "launchOnLogin": self.launch_on_login },
      "notice": self.notice, "hotkeyError": self.hotkey_error, "hotkeys": self.hotkeys, "available": self.ffmpeg, "mediaFolder": self.media.to_string_lossy()
    })
  }

  fn media_folders(&self) -> Vec<PathBuf> {
    let mut folders = vec![self.media.clone()];
    let defaults = self
      .data
      .parent()
      .into_iter()
      .flat_map(|parent| ["rsclip", "Clipline"].map(|name| parent.join(name).join("media")));
    for folder in self.general.library_folders.iter().cloned().chain(defaults) {
      if folder.is_absolute() && !folders.contains(&folder) {
        folders.push(folder);
      }
    }
    folders
  }

  pub fn media_source(&self, name: &str) -> Option<PathBuf> {
    if !valid_media(name) {
      return None;
    }
    self
      .media_folders()
      .into_iter()
      .map(|folder| folder.join(name))
      .find(|path| path.is_file())
  }

  pub fn files(&self) -> Result<Vec<Value>> {
    let mut files = HashMap::new();
    for folder in self.media_folders() {
      let entries = match fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) if folder == self.media => return Err(error.into()),
        Err(_) => continue,
      };
      for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str().filter(|name| valid_media(name)) {
          if let Ok(info) = entry.metadata() {
            if info.is_file() {
              files.entry(name.to_owned()).or_insert(info.len());
            }
          }
        }
      }
    }
    let mut files: Vec<_> = files.into_iter().collect();
    files.sort_unstable_by(|left, right| right.0.cmp(&left.0));
    Ok(
      files
        .into_iter()
        .map(|(name, size)| json!({ "name": name, "size": size }))
        .collect(),
    )
  }
}

impl Drop for Recorder {
  fn drop(&mut self) {
    self.shutdown();
  }
}

pub fn valid_media(name: &str) -> bool {
  let Some((stem, extension)) = name.rsplit_once('.') else {
    return false;
  };
  let stem = if extension == "mp4" {
    stem
      .strip_suffix(" recording")
      .or_else(|| stem.strip_suffix(" short"))
      .or_else(|| stem.strip_suffix(" long"))
      .unwrap_or(stem)
  } else {
    stem
  };
  if !matches!(extension, "mp4" | "png") || stem.len() != 24 {
    return false;
  }
  stem.bytes().enumerate().all(|(index, byte)| match index {
    4 | 7 | 13 | 16 => byte == b'-',
    10 | 19 => byte == b' ',
    20..=23 => byte.is_ascii_hexdigit(),
    _ => byte.is_ascii_digit(),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn keeps_previous_folders_and_resolves_old_recordings() -> Result<()> {
    let root = env::current_dir()?
      .join("target")
      .join(format!("media-test-{}", Uuid::new_v4()));
    let data = root.join("rsclip");
    let first = data.join("media");
    let second = root.join("new");
    let third = root.join("import");
    for folder in [&first, &second, &third] {
      fs::create_dir_all(folder)?;
    }
    let (feedback, _events) = mpsc::channel();
    let mut recorder = Recorder {
      data: data.clone(),
      media: first.clone(),
      buffer: None,
      buffer_codec: Codec::H264,
      child: None,
      audio: None,
      ffmpeg: false,
      settings: Settings::default(),
      general: General::default(),
      mixer: Arc::new(Mutex::new(MixerSettings::default())),
      sources: default_audio_sources(),
      launch_on_login: false,
      recording: None,
      feedback,
      notice: String::new(),
      error: String::new(),
      hotkey_error: String::new(),
      hotkeys: Value::Null,
    };
    let recording = "2026-11-10 09-00-00 abcd recording.mp4";
    let imported = "2026-11-10 09-00-01 abcd short.mp4";
    fs::write(first.join(recording), b"old recording")?;
    fs::write(third.join(imported), b"imported clip")?;
    recorder.set_media_folder(second.clone())?;
    recorder.add_library_folder(third.clone())?;
    recorder.general = General::load(&data.join("general.json"))?;
    assert_eq!(
      recorder.media_source(recording),
      Some(first.join(recording))
    );
    assert_eq!(recorder.media_source(imported), Some(third.join(imported)));
    assert_eq!(recorder.files()?.len(), 2);
    assert_eq!(recorder.media, second);
    assert!(recorder.media_source("..\\outside.mp4").is_none());
    fs::remove_dir_all(root)?;
    Ok(())
  }
}
