use crate::{
  app_audio,
  mixer::{MixerSettings, Sources, Stems, BLOCK, CHANNELS, RATE},
};
use anyhow::{anyhow, Result};
use std::{
  collections::{BTreeMap, HashMap, HashSet, VecDeque},
  io::Write,
  path::PathBuf,
  process::ChildStdin,
  ptr, slice,
  sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
  },
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};
use windows::Win32::{
  Media::Audio::{
    eCapture, eMultimedia, eRender, EDataFlow, IAudioCaptureClient, IAudioClient,
    IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
  },
  System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
  },
};

#[derive(Debug, PartialEq, Eq)]
pub enum AudioEvent { Interrupted, Recovered, TracksLost, TracksRecovered }

struct Health { lost: HashSet<String>, events: mpsc::Sender<AudioEvent> }
impl Health {
  fn set(&mut self, id: &str, unavailable: bool) {
    let was_lost = !self.lost.is_empty();
    if unavailable { self.lost.insert(id.into()); } else { self.lost.remove(id); }
    self.announce(was_lost);
  }
  fn announce(&self, was_lost: bool) {
    if was_lost != !self.lost.is_empty() {
      let _ = self.events.send(if self.lost.is_empty() { AudioEvent::Recovered } else { AudioEvent::Interrupted });
    }
  }
}

pub struct Loopback {
  pub input: mpsc::Sender<ChildStdin>,
  pub events: mpsc::Receiver<AudioEvent>,
  pub stop: Arc<AtomicBool>,
  pub worker: JoinHandle<Result<()>>,
}

pub fn open(
  buffer: PathBuf,
  settings: Arc<Mutex<MixerSettings>>,
  sources: Sources,
) -> Result<Loopback> {
  let (ready_tx, ready_rx) = mpsc::sync_channel(1);
  let (input, stdin_rx) = mpsc::channel();
  let (events_tx, events) = mpsc::channel();
  let stop = Arc::new(AtomicBool::new(false));
  let cancel = stop.clone();
  let worker = thread::spawn(move || {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let result = (|| -> Result<()> {
      let devices: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
      let desktop = Stream::endpoint(&devices, eRender).ok();
      ready_tx.send(())?;
      let mut stdin = stdin_rx.recv()?;
      let mut mixer = CaptureMixer::new(devices, desktop, settings, sources, buffer, events_tx);
      mixer.pump(&mut stdin, &cancel)
    })();
    unsafe { CoUninitialize() };
    result
  });
  match ready_rx.recv() {
    Ok(()) => Ok(Loopback {
      input,
      events,
      stop,
      worker,
    }),
    Err(_) => match worker.join() {
      Ok(Err(error)) => Err(error.context("Could not initialize Windows audio capture")),
      _ => Err(anyhow!("Audio capture worker exited during setup")),
    },
  }
}

type Block = [i16; BLOCK * CHANNELS];

struct Stream {
  client: IAudioClient,
  capture: IAudioCaptureClient,
  pending: VecDeque<i16>,
  endpoint_id: Option<String>,
}

impl Stream {
  fn endpoint(devices: &IMMDeviceEnumerator, flow: EDataFlow) -> Result<Self> {
    let device = unsafe { devices.GetDefaultAudioEndpoint(flow, eMultimedia)? };
    let id = device_id(&device)?;
    let client = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let mut stream = Self::start(client, flow == eRender)?;
    stream.endpoint_id = Some(id);
    Ok(stream)
  }

  fn start(client: IAudioClient, loopback: bool) -> Result<Self> {
    let format = WAVEFORMATEX {
      wFormatTag: 1,
      nChannels: CHANNELS as u16,
      nSamplesPerSec: RATE as u32,
      nAvgBytesPerSec: (RATE * CHANNELS * 2) as u32,
      nBlockAlign: (CHANNELS * 2) as u16,
      wBitsPerSample: 16,
      cbSize: 0,
    };
    let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
      | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY
      | if loopback {
        AUDCLNT_STREAMFLAGS_LOOPBACK
      } else {
        0
      };
    let capture = unsafe {
      client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 2_000_000, 0, &format, None)?;
      let capture = client.GetService::<IAudioCaptureClient>()?;
      client.Start()?;
      capture
    };
    Ok(Self {
      client,
      capture,
      pending: VecDeque::new(),
      endpoint_id: None,
    })
  }

  fn collect(&mut self) -> Result<()> {
    unsafe {
      while self.capture.GetNextPacketSize()? != 0 {
        let (mut data, mut frames, mut flags) = (ptr::null_mut(), 0, 0);
        self
          .capture
          .GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
        let samples = frames as usize * CHANNELS;
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
          self.pending.resize(self.pending.len() + samples, 0);
        } else {
          let bytes = slice::from_raw_parts(data, samples * 2);
          self.pending.extend(
            bytes
              .chunks_exact(2)
              .map(|sample| i16::from_le_bytes([sample[0], sample[1]])),
          );
        }
        self.capture.ReleaseBuffer(frames)?;
      }
    }
    let keep = BLOCK * CHANNELS * 8;
    if self.pending.len() > keep {
      self.pending.drain(..self.pending.len() - keep);
    }
    Ok(())
  }

  fn block(&mut self) -> Block {
    let mut samples = [0; BLOCK * CHANNELS];
    for (sample, received) in samples.iter_mut().zip(
      self
        .pending
        .drain(..self.pending.len().min(BLOCK * CHANNELS)),
    ) {
      *sample = received;
    }
    samples
  }
}

impl Drop for Stream {
  fn drop(&mut self) {
    unsafe {
      let _ = self.client.Stop();
    }
  }
}

fn device_id(device: &IMMDevice) -> Result<String> {
  unsafe {
    let id = device.GetId()?;
    let result = id.to_string();
    CoTaskMemFree(Some(id.0.cast()));
    Ok(result?)
  }
}

fn refresh_endpoint(devices: &IMMDeviceEnumerator, flow: EDataFlow, stream: &mut Option<Stream>) {
  let current = unsafe { devices.GetDefaultAudioEndpoint(flow, eMultimedia) }.ok().and_then(|d| device_id(&d).ok());
  if stream.as_ref().and_then(|s| s.endpoint_id.as_ref()) != current.as_ref() { *stream = None; }
  if stream.is_none() {
    match Stream::endpoint(devices, flow) {
      Ok(reopened) => *stream = Some(reopened),
      Err(error) => crate::diagnostics::record("audio endpoint", &format!("flow {}: {error:#}", flow.0)),
    }
  }
}

struct AudioOutput {
  stems: Stems,
  retries: HashMap<String, Instant>,
  events: mpsc::Sender<AudioEvent>,
}
impl AudioOutput {
  fn new(root: PathBuf, events: mpsc::Sender<AudioEvent>) -> Self {
    Self { stems: Stems::new(root), retries: HashMap::new(), events }
  }
  fn write(&mut self, tracks: BTreeMap<&str, Block>, config: &MixerSettings, stdin: &mut impl Write, frame: usize) -> Result<()> {
    let mut mix = [0.0; BLOCK * CHANNELS];
    for (id, pcm) in tracks {
      if pcm.iter().any(|&sample| sample != 0) && self.retries.get(id).is_none_or(|deadline| Instant::now() >= *deadline) {
        match self.stems.write(id, frame, &pcm) {
          Ok(()) => {
            if self.retries.remove(id).is_some() && self.retries.is_empty() { let _ = self.events.send(AudioEvent::TracksRecovered); }
          }
          Err(error) => {
            crate::diagnostics::record("audio track", &format!("{id}: {error:#}"));
            if self.retries.is_empty() { let _ = self.events.send(AudioEvent::TracksLost); }
            self.retries.insert(id.into(), Instant::now() + Duration::from_secs(1));
          }
        }
      }
      let gain = config.gain(id);
      for (sum, sample) in mix.iter_mut().zip(pcm) {
        *sum += f32::from(sample) * gain;
      }
    }
    let mut bytes = [0; BLOCK * CHANNELS * 2];
    for (chunk, sample) in bytes.chunks_exact_mut(2).zip(mix) {
      chunk.copy_from_slice(
        &(sample.clamp(f32::from(i16::MIN), f32::from(i16::MAX)) as i16).to_le_bytes(),
      );
    }
    stdin.write_all(&bytes)?;
    Ok(())
  }
}

struct CaptureMixer {
  devices: IMMDeviceEnumerator,
  desktop: Option<Stream>,
  microphone: Option<Stream>,
  apps: HashMap<u32, (app_audio::AppSource, Stream)>,
  pending: HashMap<u32, (app_audio::AppSource, app_audio::Pending, Instant)>,
  failed: HashMap<u32, Instant>,
  health: Health,

  settings: Arc<Mutex<MixerSettings>>,
  config: MixerSettings,
  sources: Sources,
  output: AudioOutput,
}

impl CaptureMixer {
  fn new(
    devices: IMMDeviceEnumerator,
    desktop: Option<Stream>,
    settings: Arc<Mutex<MixerSettings>>,
    sources: Sources,
    buffer: PathBuf,
    events: mpsc::Sender<AudioEvent>,
  ) -> Self {
    Self {
      devices,
      desktop,
      microphone: None,
      apps: HashMap::new(),
      pending: HashMap::new(),
      failed: HashMap::new(),
      health: Health { lost: HashSet::new(), events: events.clone() },
      settings,
      config: MixerSettings::default(),
      sources,
      output: AudioOutput::new(buffer, events),
    }
  }

  fn discover(&mut self) {
    self.config = self.settings.lock().unwrap().clone();
    // The audio service can restart underneath the enumerator as well as its streams.
    if self.desktop.is_none() || (self.config.microphone && self.microphone.is_none()) {
      if let Ok(devices) = unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) } { self.devices = devices; }
    }
    if self.config.microphone {
      refresh_endpoint(&self.devices, eCapture, &mut self.microphone);
    } else { self.microphone = None; }
    refresh_endpoint(&self.devices, eRender, &mut self.desktop);
    self.health.set("microphone", self.config.microphone && self.microphone.is_none());
    self.health.set("desktop", self.desktop.is_none());
    let apps = if self.config.per_app {
      app_audio::sessions(&self.devices).unwrap_or_default()
    } else {
      Vec::new()
    };
    let active: HashSet<_> = apps.iter().map(|app| app.pid).collect();
    self.apps.retain(|pid, _| active.contains(pid));
    self.pending.retain(|pid, _| active.contains(pid));
    let now = Instant::now();
    self.failed.retain(|pid, deadline| active.contains(pid) && *deadline > now);
    let expired: Vec<_> = self.pending.iter().filter_map(|(&pid, (_, _, since))| (now.duration_since(*since) >= Duration::from_secs(5)).then_some(pid)).collect();
    for pid in expired { self.pending.remove(&pid); self.failed.insert(pid, now + Duration::from_secs(3)); }
    let ids: HashSet<_> = apps.iter().map(|app| app.id.clone()).collect();
    let was_lost = !self.health.lost.is_empty();
    self.health.lost.retain(|id| id == "desktop" || id == "microphone" || ids.contains(id));
    self.health.announce(was_lost);
    for app in apps {
      if self.apps.contains_key(&app.pid)
        || self.pending.contains_key(&app.pid)
        || self.failed.contains_key(&app.pid)
      {
        continue;
      }
      match app_audio::activate(app.pid) {
        Ok(pending) => {
          self.pending.insert(app.pid, (app, pending, Instant::now()));
        }
        Err(error) => {
          crate::diagnostics::record("audio activation", &format!("{}: {error:#}", app.name));
          self.failed.insert(app.pid, Instant::now() + Duration::from_secs(3));
        }
      }
    }
  }

  fn collect(&mut self) {
    let ready: Vec<_> = self
      .pending
      .iter()
      .filter_map(|(&pid, (_, pending, _))| Some((pid, pending.poll()?)))
      .collect();
    for (pid, result) in ready {
      let (app, _, _) = self.pending.remove(&pid).unwrap();
      match result.and_then(|client| Stream::start(client, true)) {
        Ok(stream) => {
          self
            .sources
            .lock()
            .unwrap()
            .insert(app.id.clone(), app.name.clone());
          self.health.set(&app.id, false);
          self.apps.insert(pid, (app, stream));
        }
        Err(error) => {
          crate::diagnostics::record("audio activation", &format!("{}: {error:#}", app.name));
          self.failed.insert(pid, Instant::now() + Duration::from_secs(3));
        }
      }
    }
    let health = &mut self.health;
    self.apps.retain(|_, (source, stream)| {
      let result = stream.collect();
      if let Err(error) = &result { crate::diagnostics::record("app audio", &format!("{}: {error:#}", source.name)); }
      let ok = result.is_ok();
      if !ok { health.set(&source.id, true); }
      ok
    });
    if self
      .desktop
      .as_mut()
      .is_some_and(|stream| match stream.collect() { Ok(()) => false, Err(error) => { crate::diagnostics::record("audio stream", &format!("{error:#}")); true } })
    {
      self.desktop = None;
      self.health.set("desktop", true);
    }
    if self
      .microphone
      .as_mut()
      .is_some_and(|stream| match stream.collect() { Ok(()) => false, Err(error) => { crate::diagnostics::record("audio stream", &format!("{error:#}")); true } })
    {
      self.microphone = None;
      self.health.set("microphone", true);
    }
  }

  fn write_block(&mut self, stdin: &mut ChildStdin, frame: usize) -> Result<()> {
    let mut tracks = BTreeMap::<&str, Block>::new();
    if let Some(mic) = self.microphone.as_mut() {
      tracks.insert("microphone", mic.block());
    }
    let desktop = self.desktop.as_mut().map(Stream::block);
    if self.apps.is_empty() {
      if let Some(desktop) = desktop {
        tracks.insert("desktop", desktop);
      }
    } else {
      for (source, stream) in self.apps.values_mut() {
        let track = tracks.entry(&source.id).or_insert([0; BLOCK * CHANNELS]);
        for (target, sample) in track.iter_mut().zip(stream.block()) {
          *target = target.saturating_add(sample);
        }
      }
    }
    self.output.write(tracks, &self.config, stdin, frame)
  }

  fn pump(&mut self, stdin: &mut ChildStdin, stop: &AtomicBool) -> Result<()> {
    let start = Instant::now();
    let mut next_discovery = start;
    let mut sent = 0;
    while !stop.load(Ordering::Relaxed) {
      if Instant::now() >= next_discovery {
        self.discover();
        next_discovery = Instant::now() + Duration::from_secs(1);
      }
      self.collect();
      let elapsed = (start.elapsed().as_secs_f64() * RATE as f64) as usize;
      let target = elapsed.saturating_sub(BLOCK * 3);
      while sent + BLOCK <= target && !stop.load(Ordering::Relaxed) {
        self.write_block(stdin, sent)?;
        sent += BLOCK;
      }
      thread::sleep(Duration::from_millis(5));
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn audio_health_notifies_once_and_recovers_after_all_sources_return() {
    let (events, rx) = mpsc::channel();
    let mut health = Health { lost: HashSet::new(), events };
    health.set("desktop", true);
    health.set("desktop", true);
    health.set("microphone", true);
    assert_eq!(rx.try_recv().unwrap(), AudioEvent::Interrupted);
    assert!(rx.try_recv().is_err());
    health.set("desktop", false);
    assert!(rx.try_recv().is_err());
    health.set("microphone", false);
    assert_eq!(rx.try_recv().unwrap(), AudioEvent::Recovered);
  }

  #[test]
  fn track_write_failure_keeps_pcm_flowing_and_retries_without_losing_sync() -> Result<()> {
    let root = std::env::current_dir()?.join("target").join(format!("audio-fault-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("audio"), b"block directory creation")?;
    let (events, rx) = mpsc::channel();
    let mut output = AudioOutput::new(root.clone(), events);
    let config = MixerSettings::default();
    let mut pcm = Vec::new();
    output.write(BTreeMap::from([("desktop", [123; BLOCK * CHANNELS])]), &config, &mut pcm, 0)?;
    assert_eq!(pcm.len(), BLOCK * CHANNELS * 2);
    assert_eq!(rx.try_recv()?, AudioEvent::TracksLost);
    output.write(BTreeMap::new(), &config, &mut pcm, BLOCK)?;
    assert!(pcm[BLOCK * CHANNELS * 2..].iter().all(|&b| b == 0));
    assert!(rx.try_recv().is_err());
    std::fs::remove_file(root.join("audio"))?;
    output.retries.insert("desktop".into(), Instant::now());
    output.write(BTreeMap::from([("desktop", [321; BLOCK * CHANNELS])]), &config, &mut pcm, BLOCK * 2)?;
    assert_eq!(rx.try_recv()?, AudioEvent::TracksRecovered);
    assert!(output.retries.is_empty());
    drop(output);
    let raw = std::fs::read(root.join("audio/000000000/desktop"))?;
    assert!(raw[..BLOCK * CHANNELS * 4].iter().all(|&b| b == 0));
    assert_eq!(i16::from_le_bytes([raw[BLOCK * CHANNELS * 4], raw[BLOCK * CHANNELS * 4 + 1]]), 321);
    std::fs::remove_dir_all(root)?;
    Ok(())
  }

  #[test]
  #[ignore = "requires a Windows process-loopback audio service"]
  fn starts_process_loopback_at_mixer_format() -> Result<()> {
    unsafe {
      CoInitializeEx(None, COINIT_MULTITHREADED).ok()?;
    }
    let result = (|| -> Result<()> {
      let pending = app_audio::activate(std::process::id())?;
      let deadline = Instant::now() + Duration::from_secs(3);
      loop {
        if let Some(client) = pending.poll() {
          let mut stream = Stream::start(client?, true)?;
          return stream.collect();
        }
        anyhow::ensure!(
          Instant::now() < deadline,
          "Timed out waiting for process-loopback activation"
        );
        thread::sleep(Duration::from_millis(20));
      }
    })();
    unsafe {
      CoUninitialize();
    }
    result
  }
}
