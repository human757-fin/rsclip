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
    IMMDeviceEnumerator, MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
  },
  System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
  },
};

pub struct Loopback {
  pub input: mpsc::Sender<ChildStdin>,
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
      let mut mixer = CaptureMixer::new(devices, desktop, settings, sources, buffer);
      mixer.pump(&mut stdin, &cancel)
    })();
    unsafe { CoUninitialize() };
    result
  });
  match ready_rx.recv() {
    Ok(()) => Ok(Loopback {
      input,
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
}

impl Stream {
  fn endpoint(devices: &IMMDeviceEnumerator, flow: EDataFlow) -> Result<Self> {
    let client = unsafe {
      devices
        .GetDefaultAudioEndpoint(flow, eMultimedia)?
        .Activate(CLSCTX_ALL, None)?
    };
    Self::start(client, flow == eRender)
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

struct CaptureMixer {
  devices: IMMDeviceEnumerator,
  desktop: Option<Stream>,
  microphone: Option<Stream>,
  apps: HashMap<u32, (app_audio::AppSource, Stream)>,
  pending: HashMap<u32, (app_audio::AppSource, app_audio::Pending)>,
  failed: HashSet<u32>,
  settings: Arc<Mutex<MixerSettings>>,
  config: MixerSettings,
  sources: Sources,
  stems: Stems,
}

impl CaptureMixer {
  fn new(
    devices: IMMDeviceEnumerator,
    desktop: Option<Stream>,
    settings: Arc<Mutex<MixerSettings>>,
    sources: Sources,
    buffer: PathBuf,
  ) -> Self {
    Self {
      devices,
      desktop,
      microphone: None,
      apps: HashMap::new(),
      pending: HashMap::new(),
      failed: HashSet::new(),
      settings,
      config: MixerSettings::default(),
      sources,
      stems: Stems::new(buffer),
    }
  }

  fn discover(&mut self) {
    self.config = self.settings.lock().unwrap().clone();
    if self.config.microphone && self.microphone.is_none() {
      self.microphone = Stream::endpoint(&self.devices, eCapture).ok();
    } else if !self.config.microphone {
      self.microphone = None;
    }
    if self.desktop.is_none() {
      self.desktop = Stream::endpoint(&self.devices, eRender).ok();
    }
    let apps = if self.config.per_app {
      app_audio::sessions(&self.devices).unwrap_or_default()
    } else {
      Vec::new()
    };
    let active: HashSet<_> = apps.iter().map(|app| app.pid).collect();
    self.apps.retain(|pid, _| active.contains(pid));
    self.pending.retain(|pid, _| active.contains(pid));
    self.failed.retain(|pid| active.contains(pid));
    for app in apps {
      if self.apps.contains_key(&app.pid)
        || self.pending.contains_key(&app.pid)
        || self.failed.contains(&app.pid)
      {
        continue;
      }
      match app_audio::activate(app.pid) {
        Ok(pending) => {
          self.pending.insert(app.pid, (app, pending));
        }
        Err(_) => {
          self.failed.insert(app.pid);
        }
      }
    }
  }

  fn collect(&mut self) {
    let ready: Vec<_> = self
      .pending
      .iter()
      .filter_map(|(&pid, (_, pending))| Some((pid, pending.poll()?)))
      .collect();
    for (pid, result) in ready {
      let (app, _) = self.pending.remove(&pid).unwrap();
      match result.and_then(|client| Stream::start(client, true)) {
        Ok(stream) => {
          self
            .sources
            .lock()
            .unwrap()
            .insert(app.id.clone(), app.name.clone());
          self.apps.insert(pid, (app, stream));
        }
        Err(_) => {
          self.failed.insert(pid);
        }
      }
    }
    self.apps.retain(|_, (_, stream)| stream.collect().is_ok());
    if self
      .desktop
      .as_mut()
      .is_some_and(|stream| stream.collect().is_err())
    {
      self.desktop = None;
    }
    if self
      .microphone
      .as_mut()
      .is_some_and(|stream| stream.collect().is_err())
    {
      self.microphone = None;
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
    let mut mix = [0.0; BLOCK * CHANNELS];
    for (id, pcm) in tracks {
      self.stems.write(id, frame, &pcm)?;
      let gain = self.config.gain(id);
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
