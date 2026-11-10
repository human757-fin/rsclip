use serde::Serialize;
use std::{
  collections::{HashMap, HashSet},
  mem::size_of,
  process,
  time::Instant,
};
use windows::{
  core::w,
  Wdk::Graphics::Direct3D::{
    D3DKMTQueryStatistics, D3DKMT_QUERYSTATISTICS, D3DKMT_QUERYSTATISTICS_0,
    D3DKMT_QUERYSTATISTICS_ADAPTER, D3DKMT_QUERYSTATISTICS_PROCESS_NODE,
    D3DKMT_QUERYSTATISTICS_QUERY_NODE,
  },
  Win32::{
    Foundation::{CloseHandle, FILETIME, HANDLE, LUID},
    Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE},
    System::{
      Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
      },
      Performance::{
        PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW,
        PdhOpenQueryW, PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
      },
      ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS},
      Threading::{GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION},
    },
  },
};

#[derive(Default)]
pub struct Performance {
  previous: Option<(Instant, HashMap<u32, ProcessUsage>)>,
  gpu: Option<GpuCounter>,
  gpu_attempted: bool,
  gpu_fallback: Option<GraphicsCounter>,
}

#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
  cpu: Option<f64>,
  ram_bytes: Option<u64>,
  gpu: Option<f64>,
}

impl Performance {
  pub fn sample(&mut self) -> Usage {
    let Some(pids) = process_tree() else {
      return Usage::default();
    };
    let current: HashMap<_, _> = pids
      .iter()
      .filter_map(|&pid| Some((pid, process_usage(pid)?)))
      .collect();
    let now = Instant::now();
    let mut usage = Usage::default();
    if !current.is_empty() {
      usage.ram_bytes = current
        .values()
        .filter_map(|item| item.ram_bytes)
        .reduce(u64::saturating_add);
      if let Some((previous_time, previous)) = &self.previous {
        let elapsed = now.duration_since(*previous_time).as_secs_f64();
        if (0.1..=10.0).contains(&elapsed) {
          let ticks: u64 = current
            .iter()
            .filter_map(|(pid, item)| {
              let old = previous.get(pid)?;
              (item.created == old.created).then(|| item.cpu_ticks.saturating_sub(old.cpu_ticks))
            })
            .sum();
          let cores = std::thread::available_parallelism().map_or(1, |cores| cores.get()) as f64;
          usage.cpu = Some((ticks as f64 / (100_000.0 * elapsed * cores)).clamp(0.0, 100.0));
        }
      }
    }
    self.previous = Some((now, current));
    if !self.gpu_attempted {
      self.gpu = GpuCounter::open();
      if self.gpu.is_none() {
        self.gpu_fallback = GraphicsCounter::open();
      }
      self.gpu_attempted = true;
    }
    usage.gpu = self
      .gpu
      .as_mut()
      .and_then(|gpu| gpu.sample(&pids))
      .or_else(|| {
        self
          .gpu_fallback
          .as_mut()
          .and_then(|gpu| gpu.sample(self.previous.as_ref().unwrap()))
      });
    usage
  }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
  fn drop(&mut self) {
    unsafe {
      let _ = CloseHandle(self.0);
    }
  }
}

fn process_tree() -> Option<HashSet<u32>> {
  let snapshot = OwnedHandle(unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()? });
  let mut entry = PROCESSENTRY32W {
    dwSize: size_of::<PROCESSENTRY32W>() as u32,
    ..Default::default()
  };
  let mut children = HashMap::<u32, Vec<u32>>::new();
  unsafe {
    Process32FirstW(snapshot.0, &mut entry).ok()?;
    loop {
      children
        .entry(entry.th32ParentProcessID)
        .or_default()
        .push(entry.th32ProcessID);
      if Process32NextW(snapshot.0, &mut entry).is_err() {
        break;
      }
    }
  }
  let mut pending = vec![process::id()];
  let mut pids = HashSet::new();
  while let Some(pid) = pending.pop() {
    if pids.insert(pid) {
      if let Some(children) = children.get(&pid) {
        pending.extend(children);
      }
    }
  }
  Some(pids)
}

struct ProcessUsage {
  created: u64,
  cpu_ticks: u64,
  ram_bytes: Option<u64>,
}

fn process_usage(pid: u32) -> Option<ProcessUsage> {
  let handle =
    OwnedHandle(unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()? });
  let [mut created, mut exited, mut kernel, mut user] = [FILETIME::default(); 4];
  let mut memory = PROCESS_MEMORY_COUNTERS {
    cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
    ..Default::default()
  };
  unsafe {
    GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user).ok()?;
    let ram_bytes = K32GetProcessMemoryInfo(
      handle.0,
      &mut memory,
      size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
    )
    .as_bool()
    .then_some(memory.WorkingSetSize as u64);
    Some(ProcessUsage {
      created: ticks(created),
      cpu_ticks: ticks(kernel) + ticks(user),
      ram_bytes,
    })
  }
}

fn ticks(time: FILETIME) -> u64 {
  (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

struct GpuCounter {
  query: isize,
  counter: isize,
  primed: bool,
}

impl GpuCounter {
  fn open() -> Option<Self> {
    let mut query = 0;
    unsafe {
      if PdhOpenQueryW(None, 0, &mut query) != 0 {
        return None;
      }
      let mut result = Self {
        query,
        counter: 0,
        primed: false,
      };
      if PdhAddEnglishCounterW(
        query,
        w!("\\GPU Engine(*)\\Utilization Percentage"),
        0,
        &mut result.counter,
      ) != 0
      {
        return None;
      }
      Some(result)
    }
  }

  fn sample(&mut self, pids: &HashSet<u32>) -> Option<f64> {
    let (mut bytes, mut count) = (0, 0);
    unsafe {
      if PdhCollectQueryData(self.query) != 0 {
        return None;
      }
      if !self.primed {
        self.primed = true;
        return None;
      }
      if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut bytes, &mut count, None)
        != PDH_MORE_DATA
      {
        return None;
      }
      // The packed PDH array includes strings after the items, and must be aligned for both.
      let mut buffer = vec![
        PDH_FMT_COUNTERVALUE_ITEM_W::default();
        (bytes as usize).div_ceil(size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>())
      ];
      if PdhGetFormattedCounterArrayW(
        self.counter,
        PDH_FMT_DOUBLE,
        &mut bytes,
        &mut count,
        Some(buffer.as_mut_ptr()),
      ) != 0
      {
        return None;
      }
      let mut engines = HashMap::<String, f64>::new();
      for item in buffer.get(..count as usize)? {
        if item.FmtValue.CStatus > 1 {
          continue;
        }
        let name = item.szName.to_string().ok()?;
        let Some((pid, engine)) = engine_instance(&name) else {
          continue;
        };
        let value = item.FmtValue.Anonymous.doubleValue;
        if pids.contains(&pid) && value.is_finite() && value >= 0.0 {
          *engines.entry(engine.to_owned()).or_default() += value;
        }
      }
      Some(engines.into_values().fold(0.0, f64::max).min(100.0))
    }
  }
}

impl Drop for GpuCounter {
  fn drop(&mut self) {
    unsafe {
      PdhCloseQuery(self.query);
    }
  }
}

type EngineTimes = HashMap<(u32, u64, usize, u32), i64>;

struct GraphicsCounter {
  adapters: Vec<(LUID, u32)>,
  previous: Option<(Instant, EngineTimes)>,
}

impl GraphicsCounter {
  fn open() -> Option<Self> {
    unsafe {
      let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
      let mut adapters = Vec::new();
      for index in 0.. {
        let Ok(adapter) = factory.EnumAdapters1(index) else {
          break;
        };
        let Ok(desc) = adapter.GetDesc1() else {
          continue;
        };
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
          continue;
        }
        let mut query = D3DKMT_QUERYSTATISTICS {
          Type: D3DKMT_QUERYSTATISTICS_ADAPTER,
          AdapterLuid: desc.AdapterLuid,
          ..Default::default()
        };
        if D3DKMTQueryStatistics(&mut query).is_ok() {
          adapters.push((
            desc.AdapterLuid,
            query.QueryResult.AdapterInformation.NodeCount,
          ));
        }
      }
      (!adapters.is_empty()).then_some(Self {
        adapters,
        previous: None,
      })
    }
  }

  fn sample(&mut self, processes: &(Instant, HashMap<u32, ProcessUsage>)) -> Option<f64> {
    let (now, processes) = processes;
    let mut current = EngineTimes::new();
    for (&pid, process) in processes {
      let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
      else {
        continue;
      };
      let handle = OwnedHandle(handle);
      for (index, &(adapter, count)) in self.adapters.iter().enumerate() {
        for node in 0..count {
          let mut query = D3DKMT_QUERYSTATISTICS {
            Type: D3DKMT_QUERYSTATISTICS_PROCESS_NODE,
            AdapterLuid: adapter,
            hProcess: handle.0,
            Anonymous: D3DKMT_QUERYSTATISTICS_0 {
              QueryProcessNode: D3DKMT_QUERYSTATISTICS_QUERY_NODE { NodeId: node },
            },
            ..Default::default()
          };
          unsafe {
            if D3DKMTQueryStatistics(&mut query).is_ok() {
              current.insert(
                (pid, process.created, index, node),
                query.QueryResult.ProcessNodeInformation.RunningTime,
              );
            }
          }
        }
      }
    }
    let result = self.previous.as_ref().and_then(|(then, previous)| {
      let elapsed = now.duration_since(*then).as_secs_f64();
      if !(0.1..=10.0).contains(&elapsed) || current.is_empty() || previous.is_empty() {
        return None;
      }
      let mut engines = HashMap::<(usize, u32), i64>::new();
      for (&(pid, created, index, node), &ticks) in &current {
        if let Some(old) = previous.get(&(pid, created, index, node)) {
          *engines.entry((index, node)).or_default() += ticks.saturating_sub(*old).max(0);
        }
      }
      let busiest = engines.into_values().max().unwrap_or_default() as f64;
      Some((busiest / (100_000.0 * elapsed)).min(100.0))
    });
    self.previous = Some((*now, current));
    result
  }
}

fn engine_instance(name: &str) -> Option<(u32, &str)> {
  let (pid, engine) = name.strip_prefix("pid_")?.split_once('_')?;
  Some((pid.parse().ok()?, engine.split('#').next()?))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn gpu_instances_separate_process_and_engine() {
    assert_eq!(
      engine_instance("pid_42_luid_0x0000_0x1234_phys_0_eng_3_engtype_VideoEncode#1"),
      Some((42, "luid_0x0000_0x1234_phys_0_eng_3_engtype_VideoEncode"))
    );
    assert_eq!(engine_instance("unknown"), None);
  }

  #[test]
  fn samples_current_process_resources() {
    let mut performance = Performance::default();
    let first = performance.sample();
    assert!(first.ram_bytes.is_some_and(|ram| ram > 0));
    std::thread::sleep(std::time::Duration::from_millis(150));
    let second = performance.sample();
    assert!(second.cpu.is_some_and(|cpu| (0.0..=100.0).contains(&cpu)));
    if let Some(gpu) = second.gpu {
      assert!((0.0..=100.0).contains(&gpu));
    }
    println!(
      "Resource sample: CPU {:?}, RAM {:?}, GPU {:?}",
      second.cpu, second.ram_bytes, second.gpu
    );
  }
}
