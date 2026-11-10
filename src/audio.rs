use anyhow::{anyhow, bail, Result};
use std::{
  collections::VecDeque,
  io::Write,
  process::ChildStdin,
  ptr, slice,
  sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
  },
  thread::{self, JoinHandle},
  time::{Duration, Instant},
};
use windows::Win32::{
  Media::Audio::{
    eMultimedia, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEXTENSIBLE,
  },
  System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
  },
};

#[derive(Clone, Copy)]
pub struct Format {
  pub sample: &'static str,
  pub rate: u32,
  pub channels: u16,
  frame_bytes: usize,
  silence: u8,
}

pub struct Loopback {
  pub format: Format,
  pub input: mpsc::Sender<ChildStdin>,
  pub stop: Arc<AtomicBool>,
  pub worker: JoinHandle<Result<()>>,
}

pub fn open() -> Result<Loopback> {
  let (ready_tx, ready_rx) = mpsc::sync_channel(1);
  let (input, stdin_rx) = mpsc::channel();
  let stop = Arc::new(AtomicBool::new(false));
  let cancel = stop.clone();
  let worker = thread::spawn(move || {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
    let result = capture(ready_tx, stdin_rx, &cancel);
    unsafe { CoUninitialize() };
    result
  });
  match ready_rx.recv() {
    Ok(format) => Ok(Loopback {
      format,
      input,
      stop,
      worker,
    }),
    Err(_) => match worker.join() {
      Ok(Err(error)) => Err(error.context("Could not open default playback audio")),
      _ => Err(anyhow!("Audio capture worker exited during setup")),
    },
  }
}

fn capture(
  ready: mpsc::SyncSender<Format>,
  stdin: mpsc::Receiver<ChildStdin>,
  stop: &AtomicBool,
) -> Result<()> {
  let (client, capture, format) = unsafe {
    let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let device = devices.GetDefaultAudioEndpoint(eRender, eMultimedia)?;
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
    let wave = client.GetMixFormat()?;
    let setup = (|| -> Result<Format> {
      let raw = wave.read_unaligned();
      let tag = if raw.wFormatTag == 0xfffe && raw.cbSize >= 22 {
        let subtype = wave
          .cast::<WAVEFORMATEXTENSIBLE>()
          .read_unaligned()
          .SubFormat;
        if subtype.data2 != 0
          || subtype.data3 != 0x0010
          || subtype.data4 != [0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71]
        {
          bail!("Unsupported WASAPI mix subtype");
        }
        subtype.data1
      } else {
        u32::from(raw.wFormatTag)
      };
      let (sample, silence) = match (tag, raw.wBitsPerSample) {
        (3, 32) => ("f32le", 0),
        (1, 8) => ("u8", 128),
        (1, 16) => ("s16le", 0),
        (1, 24) => ("s24le", 0),
        (1, 32) => ("s32le", 0),
        _ => bail!("Unsupported WASAPI mix format"),
      };
      let frame_bytes = usize::from(raw.nBlockAlign);
      if raw.nSamplesPerSec == 0
        || raw.nChannels == 0
        || frame_bytes != usize::from(raw.nChannels) * usize::from(raw.wBitsPerSample / 8)
      {
        bail!("Invalid WASAPI mix format");
      }
      client.Initialize(
        AUDCLNT_SHAREMODE_SHARED,
        AUDCLNT_STREAMFLAGS_LOOPBACK,
        2_000_000,
        0,
        wave,
        None,
      )?;
      Ok(Format {
        sample,
        rate: raw.nSamplesPerSec,
        channels: raw.nChannels,
        frame_bytes,
        silence,
      })
    })();
    CoTaskMemFree(Some(wave.cast()));
    let format = setup?;
    let capture = client.GetService::<IAudioCaptureClient>()?;
    (client, capture, format)
  };
  ready.send(format)?;
  let mut stdin = stdin.recv()?;
  unsafe { client.Start()? };
  let result = pump(&capture, format, &mut stdin, stop);
  unsafe {
    let _ = client.Stop();
  }
  result
}

fn pump(
  capture: &IAudioCaptureClient,
  format: Format,
  stdin: &mut ChildStdin,
  stop: &AtomicBool,
) -> Result<()> {
  let frame = format.frame_bytes;
  let block_frames = (format.rate / 100).max(1) as usize;
  let mut pending = VecDeque::with_capacity(block_frames * frame * 8);
  let mut block = vec![format.silence; block_frames * frame];
  let start = Instant::now();
  let mut sent = 0;
  while !stop.load(Ordering::Relaxed) {
    unsafe {
      while capture.GetNextPacketSize()? != 0 {
        let mut data = ptr::null_mut();
        let mut frames = 0;
        let mut flags = 0;
        capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
        let len = frames as usize * frame;
        if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
          pending.resize(pending.len() + len, format.silence);
        } else {
          pending.extend(slice::from_raw_parts(data, len));
        }
        capture.ReleaseBuffer(frames)?;
      }
    }
    let max_pending = block.len() * 8;
    if pending.len() > max_pending {
      pending.drain(..pending.len() - max_pending);
    }
    let elapsed_frames = (start.elapsed().as_secs_f64() * f64::from(format.rate)) as usize;
    let target = elapsed_frames.saturating_sub(block_frames * 3);
    while sent + block_frames <= target && !stop.load(Ordering::Relaxed) {
      block.fill(format.silence);
      for (dest, sample) in block
        .iter_mut()
        .zip(pending.drain(..pending.len().min(block_frames * frame)))
      {
        *dest = sample;
      }
      stdin.write_all(&block)?;
      sent += block_frames;
    }
    thread::sleep(Duration::from_millis(5));
  }
  Ok(())
}
