use crate::monitors;
use crate::recorder::{preview_monitor, valid_media, Recorder};
use crate::{desktop, hotkeys, performance::Performance, playback};
use percent_encoding::percent_decode_str;
use serde_json::{json, Value};
use std::{
  fs::File,
  io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
  net::{TcpListener, TcpStream},
  sync::{mpsc, Arc, Mutex},
  thread,
  time::Duration,
};
use tauri::{AppHandle, Manager};

pub fn serve(
  listener: TcpListener,
  host: String,
  token: String,
  recorder: Arc<Mutex<Recorder>>,
  shortcuts: mpsc::Sender<hotkeys::Command>,
  app: AppHandle,
) {
  let host: Arc<str> = host.into();
  let token: Arc<str> = token.into();
  let performance = Arc::new(Mutex::new(Performance::default()));
  for stream in listener.incoming() {
    let Ok(stream) = stream else { break };
    let (host, token, recorder) = (host.clone(), token.clone(), recorder.clone());
    let shortcuts = shortcuts.clone();
    let app = app.clone();
    let performance = performance.clone();
    thread::spawn(move || {
      let _ = handle(
        stream,
        &host,
        &token,
        &recorder,
        &shortcuts,
        &app,
        &performance,
      );
    });
  }
}

fn handle(
  mut stream: TcpStream,
  host: &str,
  token: &str,
  recorder: &Mutex<Recorder>,
  shortcuts: &mpsc::Sender<hotkeys::Command>,
  app: &AppHandle,
  performance: &Mutex<Performance>,
) -> io::Result<()> {
  stream.set_read_timeout(Some(Duration::from_secs(5)))?;
  stream.set_write_timeout(Some(Duration::from_secs(10)))?;
  let mut reader = BufReader::new(stream.try_clone()?);
  let mut head = Vec::new();
  {
    let mut bounded = (&mut reader).take(16_384);
    loop {
      if bounded.read_until(b'\n', &mut head)? == 0 {
        return Ok(());
      }
      if head.ends_with(b"\r\n\r\n") {
        break;
      }
    }
  }
  let mut headers = [httparse::EMPTY_HEADER; 40];
  let mut request = httparse::Request::new(&mut headers);
  if !matches!(request.parse(&head), Ok(httparse::Status::Complete(_))) {
    return respond(
      &mut stream,
      "400 Bad Request",
      "text/plain",
      b"Invalid request",
    );
  }
  let header = |name: &str| {
    request
      .headers
      .iter()
      .find(|header| header.name.eq_ignore_ascii_case(name))
      .and_then(|header| std::str::from_utf8(header.value).ok())
  };
  if header("Host") != Some(host) {
    return respond(&mut stream, "403 Forbidden", "text/plain", b"Invalid host");
  }
  let path = request
    .path
    .unwrap_or_default()
    .split('?')
    .next()
    .unwrap_or_default();
  match (request.method, path) {
    (Some("GET"), "/" | "/index.html") => respond(
      &mut stream,
      "200 OK",
      "text/html; charset=utf-8",
      include_bytes!("../index.html"),
    ),
    (Some("GET"), "/app.css") => respond(
      &mut stream,
      "200 OK",
      "text/css; charset=utf-8",
      include_bytes!("../app.css"),
    ),
    (Some("GET"), "/overlay.html") => respond(
      &mut stream,
      "200 OK",
      "text/html; charset=utf-8",
      include_bytes!("../overlay.html"),
    ),
    (Some("GET"), "/app.js") => respond(
      &mut stream,
      "200 OK",
      "text/javascript; charset=utf-8",
      include_bytes!("../app.js"),
    ),
    (Some("GET"), "/api/monitors") => match monitors::list() {
      Ok(monitors) => send_json(&mut stream, "200 OK", json!(monitors)),
      Err(error) => send_json(
        &mut stream,
        "500 Internal Server Error",
        json!({ "error": format!("Could not list monitors. {error:#}") }),
      ),
    },
    (Some("GET"), "/api/maintenance") => {
      send_json(&mut stream, "200 OK", crate::maintenance::state())
    }
    (Some("GET"), "/api/performance") => send_json(
      &mut stream,
      "200 OK",
      json!(performance.lock().unwrap().sample()),
    ),
    (Some("GET"), "/api") => {
      let recorder = recorder.lock().unwrap();
      let mut state = recorder.state();
      state["token"] = json!(token);
      match recorder.files() {
        Ok(files) => {
          state["files"] = json!(files);
          send_json(&mut stream, "200 OK", state)
        }
        Err(error) => send_json(
          &mut stream,
          "500 Internal Server Error",
          json!({ "error": error.to_string() }),
        ),
      }
    }
    (Some("GET"), path) if path.starts_with("/mix/") => {
      let name = path.trim_start_matches("/mix/");
      if name.len() == 42
        && name.ends_with(".mix.webm")
        && name
          .bytes()
          .all(|b| b.is_ascii_hexdigit() || b".mixweb".contains(&b))
      {
        let root = recorder.lock().unwrap().playback_cache();
        if let Ok(file) = File::open(root.join(name)) {
          return media(&mut stream, file, "video/webm", header("Range"));
        }
      }
      respond(&mut stream, "404 Not Found", "text/plain", b"Mix not found")
    }
    (Some("GET"), path) if path.starts_with("/thumb/") || path.starts_with("/playback/") => {
      let thumbnail = path.starts_with("/thumb/");
      let name =
        percent_decode_str(path.splitn(3, '/').nth(2).unwrap_or_default()).decode_utf8_lossy();
      if valid_media(&name) && name.ends_with(".mp4") {
        let (root, source) = {
          let recorder = recorder.lock().unwrap();
          (
            recorder.playback_cache(),
            recorder.media_source(name.as_ref()),
          )
        };
        let file = source.and_then(|source| {
          let path = if thumbnail {
            playback::thumbnail(&root, &source)
          } else {
            playback::cached_path(&root, &source, "webm")
          };
          path.ok().and_then(|path| File::open(path).ok())
        });
        if let Some(file) = file {
          return media(
            &mut stream,
            file,
            if thumbnail {
              "image/jpeg"
            } else {
              "video/webm"
            },
            header("Range"),
          );
        }
      }
      respond(
        &mut stream,
        "404 Not Found",
        "text/plain",
        b"Preview not available",
      )
    }
    (Some("GET"), path) if path.starts_with("/media/") => {
      let name = percent_decode_str(&path[7..]).decode_utf8_lossy();
      if valid_media(&name) {
        let path = recorder.lock().unwrap().media_source(name.as_ref());
        if let Some(file) = path.and_then(|path| File::open(path).ok()) {
          return media(
            &mut stream,
            file,
            if name.ends_with(".mp4") {
              "video/mp4"
            } else {
              "image/png"
            },
            header("Range"),
          );
        }
      }
      respond(
        &mut stream,
        "404 Not Found",
        "text/plain",
        b"File not found",
      )
    }
    (Some("POST"), path) if path.starts_with("/api/") => {
      if header("X-Rsclip-Token") != Some(token) {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"Invalid token");
      }
      let size = header("Content-Length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
      if !(1..=4096).contains(&size) {
        return send_json(
          &mut stream,
          "400 Bad Request",
          json!({ "error": "Invalid body size" }),
        );
      }
      let mut bytes = vec![0; size];
      reader.read_exact(&mut bytes)?;
      let body: Value = match serde_json::from_slice::<Value>(&bytes) {
        Ok(value) if value.is_object() => value,
        _ => {
          return send_json(
            &mut stream,
            "400 Bad Request",
            json!({ "error": "Invalid JSON object" }),
          )
        }
      };
      if path == "/api/update-check" {
        thread::spawn(crate::maintenance::check_update);
        return send_json(&mut stream, "200 OK", json!({}));
      }
      if path == "/api/update-install" {
        let mut capture = recorder.lock().unwrap();
        let state = capture.state();
        if state["recording"].as_bool().unwrap_or(false) {
          return send_json(
            &mut stream,
            "409 Conflict",
            json!({"error":"Stop recording before installing an update."}),
          );
        }
        return match crate::maintenance::install() {
          Ok(()) => {
            capture.shutdown();
            app.exit(0);
            send_json(&mut stream, "200 OK", json!({}))
          }
          Err(error) => send_json(
            &mut stream,
            "400 Bad Request",
            json!({"error":format!("{error:#}")}),
          ),
        };
      }
      if path == "/api/monitor-preview" {
        let Some(output) = body["output"].as_u64().filter(|&output| output <= 15) else {
          return send_json(
            &mut stream,
            "400 Bad Request",
            json!({ "error": "Choose a monitor." }),
          );
        };
        return match preview_monitor(output) {
          Ok(png) => respond(&mut stream, "200 OK", "image/png", &png),
          Err(error) => send_json(
            &mut stream,
            "400 Bad Request",
            json!({ "error": format!("{error:#}") }),
          ),
        };
      }
      if path == "/api/prepare-playback" {
        let name = body["name"].as_str().unwrap_or_default();
        let result = if valid_media(name) && name.ends_with(".mp4") {
          let (root, source) = {
            let recorder = recorder.lock().unwrap();
            (recorder.playback_cache(), recorder.media_source(name))
          };
          source
            .ok_or_else(|| anyhow::anyhow!("Clip not found."))
            .and_then(|source| {
              let channels = playback::audio_channels(&playback::probe_audio(&source)?);
              if body.get("gains").is_some() {
                let name = playback::mixed_preview(&root, &source, &body)?;
                Ok(json!({"proxy":true,"url":format!("/mix/{name}"),"channels":channels}))
              } else {
                let proxy =
                  playback::prepare(&root, &source, body["force"].as_bool().unwrap_or(false))?;
                Ok(json!({"proxy":proxy,"channels":channels}))
              }
            })
        } else {
          Err(anyhow::anyhow!("Clip not found."))
        };
        return match result {
          Ok(result) => send_json(&mut stream, "200 OK", result),
          Err(error) => send_json(
            &mut stream,
            "400 Bad Request",
            json!({ "error": format!("{error:#}") }),
          ),
        };
      }
      let result = if path == "/api/hotkey" {
        hotkeys::request(shortcuts, body)
      } else if matches!(path, "/api/trim" | "/api/compatible") {
        let (folder, source, mix) = {
          let recorder = recorder.lock().unwrap();
          (
            recorder.media.clone(),
            recorder.media_source(body["name"].as_str().unwrap_or_default()),
            recorder.mixer_settings(),
          )
        };
        let _job = playback::VIDEO_JOBS.lock().unwrap();
        source
          .ok_or_else(|| anyhow::anyhow!("Clip not found."))
          .and_then(|source| {
            Recorder::export(
              &folder,
              &source,
              &body,
              path == "/api/trim",
              &mix.clip_mix(&body)?,
            )
          })
          .map(|message| {
            let mut recorder = recorder.lock().unwrap();
            recorder.notice = message.into();
            recorder.state()
          })
      } else if path == "/api/fullscreen" {
        match (app.get_webview_window("main"), body["enabled"].as_bool()) {
          (Some(window), Some(enabled)) => window
            .set_fullscreen(enabled)
            .map(|()| json!({ "fullscreen": enabled }))
            .map_err(Into::into),
          _ => Err(anyhow::anyhow!("Fullscreen is unavailable.")),
        }
      } else if path == "/api/reveal-media" {
        let name = body["name"].as_str().unwrap_or_default();
        let source = recorder.lock().unwrap().media_source(name);
        if let Some(source) = source {
          desktop::reveal_file(&source).map(|()| recorder.lock().unwrap().state())
        } else {
          Err(anyhow::anyhow!("File not found."))
        }
      } else if matches!(path, "/api/browse-media" | "/api/add-library-folder") {
        let save_destination = path == "/api/browse-media";
        let current = recorder.lock().unwrap().media.clone();
        let title = if save_destination {
          "Choose where rsclip saves media"
        } else {
          "Choose an older folder to add to the rsclip library"
        };
        let selected = desktop::choose_folder(&current, title);
        let mut recorder = recorder.lock().unwrap();
        match selected {
          Some(folder) if save_destination => {
            recorder.set_media_folder(folder).map(|()| recorder.state())
          }
          Some(folder) => recorder
            .add_library_folder(folder)
            .map(|()| recorder.state()),
          None => Ok(recorder.state()),
        }
      } else {
        recorder.lock().unwrap().act(&path[5..], &body)
      };
      match result {
        Ok(state) => send_json(&mut stream, "200 OK", state),
        Err(error) => send_json(
          &mut stream,
          "400 Bad Request",
          json!({ "error": format!("{error:#}") }),
        ),
      }
    }
    _ => respond(&mut stream, "404 Not Found", "text/plain", b"Not found"),
  }
}

fn response_head(
  stream: &mut impl Write,
  status: &str,
  mime: &str,
  size: u64,
  extra: &str,
) -> io::Result<()> {
  write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {size}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n{extra}\r\n")
}

fn respond(stream: &mut impl Write, status: &str, mime: &str, body: &[u8]) -> io::Result<()> {
  response_head(stream, status, mime, body.len() as u64, "")?;
  stream.write_all(body)
}

fn send_json(stream: &mut impl Write, status: &str, value: Value) -> io::Result<()> {
  respond(
    stream,
    status,
    "application/json",
    value.to_string().as_bytes(),
  )
}

fn media(
  stream: &mut impl Write,
  mut file: File,
  mime: &str,
  header: Option<&str>,
) -> io::Result<()> {
  let size = file.metadata()?.len();
  if let Some(header) = header {
    if let Some((start, end)) = byte_range(header, size) {
      let len = end - start + 1;
      let extra = format!("Accept-Ranges: bytes\r\nContent-Range: bytes {start}-{end}/{size}\r\n");
      response_head(stream, "206 Partial Content", mime, len, &extra)?;
      file.seek(SeekFrom::Start(start))?;
      io::copy(&mut file.take(len), stream)?;
    } else {
      response_head(
        stream,
        "416 Range Not Satisfiable",
        "text/plain",
        0,
        &format!("Content-Range: bytes */{size}\r\n"),
      )?;
    }
  } else {
    response_head(stream, "200 OK", mime, size, "Accept-Ranges: bytes\r\n")?;
    io::copy(&mut file, stream)?;
  }
  Ok(())
}

fn byte_range(header: &str, size: u64) -> Option<(u64, u64)> {
  let last = size.checked_sub(1)?;
  let (start, end) = header.strip_prefix("bytes=")?.split_once('-')?;
  let (start, end) = if start.is_empty() {
    let suffix = end.parse().ok()?;
    (size.saturating_sub(suffix), last)
  } else {
    let start = start.parse().ok()?;
    let end = if end.is_empty() {
      last
    } else {
      last.min(end.parse().ok()?)
    };
    (start, end)
  };
  (start <= end).then_some((start, end))
}
