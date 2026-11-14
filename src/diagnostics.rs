use anyhow::Result;
use std::{fs::{self, OpenOptions}, io::Write, path::PathBuf, sync::{Mutex, OnceLock}, time::{Duration, Instant}};

const LIMIT: u64 = 1024 * 1024;
struct Log { path: PathBuf, enabled: bool, last: String, since: Option<Instant> }
fn state() -> &'static Mutex<Log> {
  static STATE: OnceLock<Mutex<Log>> = OnceLock::new();
  STATE.get_or_init(|| Mutex::new(Log { path: PathBuf::new(), enabled: false, last: String::new(), since: None }))
}
pub fn configure(path: PathBuf, enabled: bool) -> Result<()> {
  let mut log = state().lock().unwrap_or_else(|e| e.into_inner());
  if enabled {
    fs::create_dir_all(path.parent().unwrap())?;
    OpenOptions::new().create(true).append(true).open(&path)?;
  }
  log.path = path;
  log.enabled = enabled;
  log.last.clear();
  Ok(())
}
pub fn record(context: &str, message: &str) {
  let mut log = state().lock().unwrap_or_else(|e| e.into_inner());
  if !log.enabled || message.is_empty() { return; }
  let message: String = format!("{context}: {message}").chars().take(8192).collect();
  if log.last == message && log.since.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) { return; }
  let result = (|| -> std::io::Result<()> {
    if log.path.metadata().is_ok_and(|m| m.len() >= LIMIT) {
      let first = log.path.with_extension("log.1");
      let second = log.path.with_extension("log.2");
      if second.exists() { fs::remove_file(&second)?; }
      if first.exists() { fs::rename(&first, &second)?; }
      fs::rename(&log.path, &first)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(&log.path)?;
    writeln!(file, "{} [rsclip {}] {}", chrono::Utc::now().to_rfc3339(), env!("CARGO_PKG_VERSION"), message)
  })();
  // Logging failures must never interrupt capture or recursively log themselves.
  if result.is_ok() { log.last = message; log.since = Some(Instant::now()); }
}

#[cfg(test)]
mod tests {
  #[test]
  fn log_is_opt_in_and_rotates() -> anyhow::Result<()> {
    let root = std::env::current_dir()?.join("target").join(format!("log-test-{}", uuid::Uuid::new_v4()));
    let path = root.join("errors.log");
    super::configure(path.clone(), false)?;
    super::record("test", "disabled");
    assert!(!path.exists());
    super::configure(path.clone(), true)?;
    super::record("test", "debug marker");
    assert!(std::fs::read_to_string(&path)?.contains("debug marker"));
    for index in 0..150 { super::record("test", &format!("{index} {}", "x".repeat(8000))); }
    assert!(path.with_extension("log.1").exists());
    super::configure(path.clone(), false)?;
    let size = path.metadata()?.len();
    super::record("test", "disabled again");
    assert_eq!(size, path.metadata()?.len());
    std::fs::remove_dir_all(root)?;
    Ok(())
  }
}
