use anyhow::{Context, Result};
use std::{
  collections::{hash_map::DefaultHasher, HashMap, HashSet},
  hash::{Hash, Hasher},
  mem::ManuallyDrop,
  path::Path,
  sync::mpsc,
};
use windows::{
  core::{imp, implement, IUnknown, Interface, HRESULT, PROPVARIANT, PWSTR},
  Win32::{
    Foundation::CloseHandle,
    Media::Audio::{
      eRender, ActivateAudioInterfaceAsync, AudioSessionStateActive,
      IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
      IActivateAudioInterfaceCompletionHandler_Impl, IAudioClient, IAudioSessionControl2,
      IAudioSessionManager2, IMMDeviceEnumerator, AUDIOCLIENT_ACTIVATION_PARAMS,
      AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
      AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, DEVICE_STATE_ACTIVE,
      PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    },
    System::{
      Com::{IAgileObject, IAgileObject_Impl, CLSCTX_ALL},
      Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
      },
      Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
      },
    },
  },
};

#[derive(Clone)]
pub struct AppSource {
  pub pid: u32,
  pub id: String,
  pub name: String,
}

pub fn sessions(devices: &IMMDeviceEnumerator) -> Result<Vec<AppSource>> {
  let mut pids = HashSet::new();
  unsafe {
    let endpoints = devices.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
    for index in 0..endpoints.GetCount()? {
      let result = (|| -> windows::core::Result<_> {
        let manager: IAudioSessionManager2 = endpoints.Item(index)?.Activate(CLSCTX_ALL, None)?;
        let sessions = manager.GetSessionEnumerator()?;
        Ok((sessions.GetCount()?, sessions))
      })();
      let Ok((count, sessions)) = result else {
        continue;
      };
      for index in 0..count {
        let Ok(session) = sessions.GetSession(index) else {
          continue;
        };
        if session.GetState().ok() != Some(AudioSessionStateActive) {
          continue;
        }
        let Ok(session) = session.cast::<IAudioSessionControl2>() else {
          continue;
        };
        if let Ok(pid) = session.GetProcessId() {
          if pid != 0 {
            pids.insert(pid);
          }
        }
      }
    }
  }
  let parents = parents();
  let mut sources = Vec::new();
  for &pid in &pids {
    let mut ancestor = pid;
    let mut nested = false;
    for _ in 0..64 {
      let Some(&parent) = parents.get(&ancestor) else {
        break;
      };
      if parent == 0 || parent == ancestor {
        break;
      }
      if pids.contains(&parent) {
        nested = true;
        break;
      }
      ancestor = parent;
    }
    if nested {
      continue;
    }
    if let Some(path) = process_path(pid) {
      let Some(name) = Path::new(&path).file_stem().and_then(|name| name.to_str()) else {
        continue;
      };
      let mut hash = DefaultHasher::new();
      path.to_lowercase().hash(&mut hash);
      sources.push(AppSource {
        pid,
        id: format!("app-{:016x}", hash.finish()),
        name: name.to_owned(),
      });
    }
  }
  Ok(sources)
}

fn parents() -> HashMap<u32, u32> {
  let mut result = HashMap::new();
  unsafe {
    let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
      return result;
    };
    let mut entry = PROCESSENTRY32W {
      dwSize: size_of::<PROCESSENTRY32W>() as u32,
      ..Default::default()
    };
    if Process32FirstW(snapshot, &mut entry).is_ok() {
      loop {
        result.insert(entry.th32ProcessID, entry.th32ParentProcessID);
        if Process32NextW(snapshot, &mut entry).is_err() {
          break;
        }
      }
    }
    let _ = CloseHandle(snapshot);
  }
  result
}

fn process_path(pid: u32) -> Option<String> {
  unsafe {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let mut path = [0u16; 4096];
    let mut len = path.len() as u32;
    let result = QueryFullProcessImageNameW(
      handle,
      PROCESS_NAME_WIN32,
      PWSTR(path.as_mut_ptr()),
      &mut len,
    );
    let _ = CloseHandle(handle);
    result.ok()?;
    Some(String::from_utf16_lossy(&path[..len as usize]))
  }
}

struct Activated(usize);

impl Activated {
  fn into_client(self) -> IAudioClient {
    let pointer = ManuallyDrop::new(self).0;
    unsafe { IAudioClient::from_raw(pointer as *mut _) }
  }
}

impl Drop for Activated {
  fn drop(&mut self) {
    unsafe {
      drop(IAudioClient::from_raw(self.0 as *mut _));
    }
  }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Completion(mpsc::Sender<Result<Activated>>);

impl IAgileObject_Impl for Completion_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
  fn ActivateCompleted(
    &self,
    operation: Option<&IActivateAudioInterfaceAsyncOperation>,
  ) -> windows::core::Result<()> {
    let result = (|| -> Result<_> {
      let operation = operation.context("No audio activation result")?;
      let mut status = HRESULT(0);
      let mut object: Option<IUnknown> = None;
      unsafe {
        operation.GetActivateResult(&mut status, &mut object)?;
      }
      status.ok()?;
      let client: IAudioClient = object.context("No activated audio client")?.cast()?;
      Ok(Activated(client.into_raw() as usize))
    })();
    let _ = self.0.send(result);
    Ok(())
  }
}

pub struct Pending {
  receiver: mpsc::Receiver<Result<Activated>>,
  _operation: IActivateAudioInterfaceAsyncOperation,
}

impl Pending {
  pub fn poll(&self) -> Option<Result<IAudioClient>> {
    match self.receiver.try_recv() {
      Ok(result) => Some(result.map(Activated::into_client)),
      Err(mpsc::TryRecvError::Disconnected) => {
        Some(Err(anyhow::anyhow!("Audio activation disconnected")))
      }
      Err(mpsc::TryRecvError::Empty) => None,
    }
  }
}

pub fn activate(pid: u32) -> Result<Pending> {
  let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
    ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
      ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
        TargetProcessId: pid,
        ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
      },
    },
  };
  let (sender, receiver) = mpsc::channel();
  let handler: IActivateAudioInterfaceCompletionHandler = Completion(sender).into();
  // The blob borrows the stack activation parameters for the call. PROPVARIANT must not free it.
  let variant = ManuallyDrop::new(unsafe {
    PROPVARIANT::from_raw(imp::PROPVARIANT {
      Anonymous: imp::PROPVARIANT_0 {
        Anonymous: imp::PROPVARIANT_0_0 {
          vt: 65,
          wReserved1: 0,
          wReserved2: 0,
          wReserved3: 0,
          Anonymous: imp::PROPVARIANT_0_0_0 {
            blob: imp::BLOB {
              cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
              pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast(),
            },
          },
        },
      },
    })
  });
  let operation = unsafe {
    ActivateAudioInterfaceAsync(
      VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
      &IAudioClient::IID,
      Some(&*variant),
      &handler,
    )?
  };
  Ok(Pending {
    receiver,
    _operation: operation,
  })
}
