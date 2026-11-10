use anyhow::{Context, Result};
use serde::Serialize;
use std::{collections::HashMap, mem::size_of};
use windows::{
  core::Interface,
  Win32::{
    Devices::Display::{
      DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
      DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
      DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
      DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
    },
    Foundation::ERROR_INSUFFICIENT_BUFFER,
    Graphics::{
      Direct3D::D3D_DRIVER_TYPE_HARDWARE,
      Direct3D11::{D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION},
      Dxgi::{IDXGIDevice, DXGI_ERROR_NOT_FOUND},
    },
  },
};

#[derive(Serialize)]
pub struct Monitor {
  pub output: u32,
  name: String,
  display: String,
  width: i32,
  height: i32,
}

pub fn list() -> Result<Vec<Monitor>> {
  // Use the same default hardware adapter and output order as ddagrab.
  let mut device = None;
  unsafe {
    D3D11CreateDevice(
      None,
      D3D_DRIVER_TYPE_HARDWARE,
      None,
      D3D11_CREATE_DEVICE_BGRA_SUPPORT,
      None,
      D3D11_SDK_VERSION,
      Some(&mut device),
      None,
      None,
    )?;
  }
  let device: IDXGIDevice = device
    .context("Windows did not return a Direct3D device")?
    .cast()?;
  let adapter = unsafe { device.GetAdapter()? };
  let names = friendly_names().unwrap_or_default();
  let mut monitors = Vec::new();
  for output in 0..=15 {
    let item = match unsafe { adapter.EnumOutputs(output) } {
      Ok(item) => item,
      Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
      Err(error) => return Err(error.into()),
    };
    let desc = unsafe { item.GetDesc()? };
    if !desc.AttachedToDesktop.as_bool() {
      continue;
    }
    let gdi = wide(&desc.DeviceName);
    let display = gdi
      .strip_prefix(r"\\.\DISPLAY")
      .filter(|index| !index.is_empty())
      .map(|index| format!("Display {index}"))
      .unwrap_or_else(|| format!("Display {}", output + 1));
    let name = names.get(&gdi).cloned().unwrap_or_else(|| display.clone());
    let rect = desc.DesktopCoordinates;
    monitors.push(Monitor {
      output,
      name,
      display,
      width: rect.right - rect.left,
      height: rect.bottom - rect.top,
    });
  }
  Ok(monitors)
}

fn friendly_names() -> Result<HashMap<String, String>> {
  let flags = QDC_ONLY_ACTIVE_PATHS;
  // A monitor can be connected between sizing the arrays and querying them.
  for _ in 0..3 {
    let (mut path_count, mut mode_count) = (0, 0);
    unsafe {
      GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count).ok()?;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    let status = unsafe {
      QueryDisplayConfig(
        flags,
        &mut path_count,
        paths.as_mut_ptr(),
        &mut mode_count,
        modes.as_mut_ptr(),
        None,
      )
    };
    if status == ERROR_INSUFFICIENT_BUFFER {
      continue;
    }
    status.ok()?;
    let mut names = HashMap::new();
    for path in paths.into_iter().take(path_count as usize) {
      let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
          r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
          size: size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
          adapterId: path.sourceInfo.adapterId,
          id: path.sourceInfo.id,
        },
        ..Default::default()
      };
      let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
          r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
          size: size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
          adapterId: path.targetInfo.adapterId,
          id: path.targetInfo.id,
        },
        ..Default::default()
      };
      if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0
        || unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } != 0
      {
        continue;
      }
      let name = wide(&target.monitorFriendlyDeviceName);
      if !name.is_empty() {
        names.entry(wide(&source.viewGdiDeviceName)).or_insert(name);
      }
    }
    return Ok(names);
  }
  Ok(HashMap::new())
}

fn wide(value: &[u16]) -> String {
  let length = value
    .iter()
    .position(|&unit| unit == 0)
    .unwrap_or(value.len());
  String::from_utf16_lossy(&value[..length])
}
