# rsclip

A Windows replay clipper with a Rust backend and a Tauri desktop UI powered by WebView2. It captures the selected display through FFmpeg Desktop Duplication, encodes video with NVIDIA NVENC, and records desktop audio through Windows WASAPI loopback.

## Run

1. Install Rust, Microsoft Edge WebView2 Runtime, and a Windows FFmpeg build that includes `ddagrab` and `h264_nvenc`. Put FFmpeg on PATH and make sure the NVIDIA driver is installed.
2. In this folder, run `cargo run --release`. rsclip opens in its own desktop window. The release executable is `target\release\rsclip.exe`.
3. The replay buffer starts when rsclip opens by default. Minimize to the taskbar or close the window to hide rsclip in the system tray. Left-click its tray icon to restore the window. Right-click the icon and choose **Quit rsclip** to stop capture and exit.

Check your FFmpeg build with `ffmpeg -hide_banner -h filter=ddagrab` and `ffmpeg -hide_banner -h encoder=h264_nvenc`.

| Action | Shortcut |
| --- | --- |
| Start or stop buffering | Alt + F9 |
| Start or stop manual recording | Alt + F7 |
| Save short clip | Alt + F10 |
| Save long clip | Alt + F11 |
| Take a screenshot | Alt + F8 |

Open **Keybinds**, select a shortcut, then press its replacement. Single keys or combinations with Ctrl, Alt, Shift, or Win are accepted. Press Esc to cancel. Shortcuts are saved across restarts.

## Capture settings

- **Clip length** accepts separate short and long lengths from 5 to 600 seconds. Defaults are 30 and 120 seconds. Both save actions use one buffer sized for the long clip. Allow the buffer to fill after increasing the long length.
- **Settings → Capture** lists connected monitors on the capture GPU by their Windows names. Selecting a monitor shows one preview frame. Use **Refresh** for a new frame and **Apply capture settings** to switch the recorder. The preview stays in memory. The same menu accepts 15–240 FPS at the display's native resolution. Pick H.264, H.265/HEVC, or AV1 hardware encoding. Changing the capture profile requires stopping the buffer. rsclip tests the new profile against the selected display, GPU, driver, and FFmpeg build before saving it.
- Choose constant bitrate, variable bitrate, or target quality. Set a bitrate from 2 to 80 Mb/s and an encoder preset from the menu. Variable bitrate can peak at twice the target. In target-quality mode the bitrate is a cap and lower quality numbers request higher visual quality. Settings persist across restarts.
- The buffer stores two-second MP4 segments. The default 120-second long buffer at 12 Mb/s needs roughly 190 MB of temporary storage. Saved clips remux existing packets. While buffering, saves use completed segments, so they can omit roughly the latest two seconds and round to a segment boundary.

## Capture and playback

- **Settings → Storage** shows the current folder. Clips and screenshots go to `%LOCALAPPDATA%
sclip\media`. Existing installations can continue using their original media folder. Short and long saves are labeled in the library.
- Audio is the default Windows playback device's system mix, saved as stereo AAC. If you switch the default device, restart the replay buffer. Microphone input is outside this build.
- H.264 is the default. The library shows a still frame from each clip. The editor prepares a low-resolution WebM preview for HEVC and AV1 playback. Select **Make H.264 copy** for a shareable MP4, including while the replay buffer is running. A new MP4 appears in Clips and the original remains available. Trim exports also use H.264. Both export options cap output at 1080p and 60 FPS for wider playback support. Legacy silent clips can still be played and trimmed.

## Recording and general settings

- Select **Record** in the top bar to start a manual recording. Select **Stop recording** to save an MP4. Recording uses the same encoder and audio source as the replay buffer, and can include up to one two-second segment before you pressed Record. If the replay buffer was already running, it restarts after the recording is saved and needs time to refill.
- **Settings → General → Browse** changes the folder used for new clips, recordings, screenshots, and exports. Existing files stay where they are and remain visible in the library. Use **Add older folder** to include a save folder from an earlier release.
- **Settings → General** also controls starting the replay buffer on launch, launching rsclip at Windows sign-in, and playing distinct sounds after clip and screenshot saves. The Windows sign-in toggle is off until enabled. rsclip restores a saved sign-in preference when it first opens after reinstalling.

## Windows installers

On a Windows machine with Tauri's packaging prerequisites, run:

```powershell
pnpm dlx @tauri-apps/cli@2 build --bundles nsis,msi
```

The NSIS setup executable and WiX MSI are written to `target/release/bundle`. Artwork for setup and uninstall is in `installer/`. FFmpeg is still required on PATH for capture.

### Automatic GitHub releases

Push the project to a GitHub repository with Actions enabled. A push to `main` runs the Windows release workflow for a new version. For later releases, update the version in `Cargo.toml` and `tauri.conf.json`, run `cargo check` to refresh `Cargo.lock`, then commit and push all three files. After tests and both installer builds pass, the workflow creates the matching `vMAJOR.MINOR.PATCH` tag and attaches the NSIS and MSI installers to the GitHub release. You can rerun a failed workflow from the Actions tab.

### Audio channels and presets

In **Settings → Audio**, turn on **Microphone** to capture the default Windows input. Separate app tracks use Windows process loopback for discoverable playback sessions. Apps are grouped by executable, and new playback sessions are detected while the buffer runs. Desktop audio is used when separate app capture is off or no app stream is available. Process loopback requires Windows build 20348 or newer.

Saved clips and recordings include a stereo playback mix and the isolated tracks that contributed audio. Adjust source volumes and save named presets in Audio settings. H.264 exports use the current volumes to combine isolated tracks into one stereo track. Older clips with a single audio track keep that mix. Keep both `ffmpeg` and `ffprobe` on PATH.


### Automatic setup and updates (0.3.7)

When FFmpeg is missing, rsclip downloads the Gyan essentials Windows build over HTTPS, verifies its publisher SHA256 checksum, and installs ffmpeg/ffprobe under `%LOCALAPPDATA%\rsclip\tools\ffmpeg`. No administrator rights or PATH changes are needed. The download runs in the background. Desktop capture and NVENC support are checked before installation; GPU compatibility checks still apply. The replay buffer starts when ready if enabled.

At launch, rsclip checks the latest stable GitHub Release from `human757-fin/rsclip`. A newer x64 NSIS installer downloads in the background and is checked against GitHub's SHA256 asset digest. General / Software offers Check for updates and Install update and restart. Stop manual recording before installing, rsclip reopens automatically after successful installation. Network failures leave the installed app usable. Updates require a public repository with a published NSIS asset and GitHub digest; no GitHub credentials are stored.

Global shortcuts are suspended while the shortcut editor listens. Escape, closing the menu, or losing focus restores them; a timeout also restores them if the editor disconnects.


### NVIDIA driver API compatibility (0.3.9)

Setup now probes H.264 NVENC using one synthetic 1280x720 frame. When FFmpeg reports an unsupported NVENC API version, rsclip downloads Gyan FFmpeg 8.0.1 full from its versioned GitHub release and checks the pinned SHA256 from Microsoft's winget-pkgs manifest. It then repeats feature and hardware encoding checks. This replaces the app-managed FFmpeg only; a PATH installation is unchanged. The compatibility pairing still needs a live test on the affected GPU. Other GPU failures are reported directly. On existing installs, stop capture and use General / Software / Retry FFmpeg setup, or restart rsclip.


### Desktop fixes (0.3.10)

The NVENC probe now uses 720p YUV420 video instead of a 64x64 frame, avoiding hardware minimum-size failures. A Windows session-scoped instance guard runs before recorder initialization: another launch signals the running app to show and focus its window, then exits without starting capture or setup workers. Slider styling now includes filled tracks, defined handles, and keyboard focus for audio, trim, and playback controls.


### Flexible keybinds (0.3.12)

Keybinds can use single letters, numbers, function keys, Space, arrows, navigation keys, numpad keys, and punctuation, with optional modifiers. Escape remains reserved for cancelling shortcut editing. Windows-reserved keys and shortcuts already registered by another app are rejected. Duplicate rsclip bindings are also rejected; accepted bindings persist across restarts.


### Overlay capture visibility (0.3.13)

General / Capture feedback / Stream-proof overlay is enabled by default. Turn it off to allow display-capture tools to include the feedback overlay. Capture methods that only record a game's window may still omit it. Changing this setting dismisses any current notification and applies to the next one. The preference survives restarts; older settings retain protected overlays. Disabling Capture overlay still destroys the overlay window.


### Resolution and export quality (0.3.14)

Video settings offer Native, 720p, 1080p, 1440p, and 2160p capture. These are maximum heights: aspect ratio is retained and smaller screens are not upscaled. Resolution applies to both short/long clips and recordings, and requires stopping capture before applying. Native keeps the original GPU capture path; resizing downloads frames for a CPU bilinear resize before NVENC encoding, so it adds CPU work.

The export split button selects H.264, H.265 / HEVC, or AV1 for trimmed exports. Exports retain source dimensions/frame rate and mix the current audio channel volumes into AAC stereo. Unsupported encoders report an error without modifying the source. Make H.264 copy remains capped at 1080p/60 for compatibility. Compatibility playback previews now use up to 1080p/60, VP8 at 8 Mbps/CRF 8, and larger thumbnails. Old preview caches are invalidated. Preview generation remains on demand with two encoding threads.


### Reopen after updates (0.3.15)

In-app NSIS updates now request restart on successful installation. The installer launches the new executable from its installed location with `--show-window`, so an update opens the main window even when normal startup is configured to use the tray. The saved Start hidden setting still applies to normal launches. This takes effect for updates initiated by 0.3.15 or newer; upgrading from an older updater may require reopening once manually.


### Replay buffer recovery (0.3.16)

Unexpected replay-process exits trigger up to three retries, delayed by 2, 5, and 15 seconds. The retry budget resets after 30 seconds of stable capture. A restart is announced only after a completed video segment is available. The current error stays visible until recovery succeeds. Manual stop, cancelling a pending restart, and quitting cancel recovery. Failed manual recordings retain their segments for Save recording and are not replaced automatically.

Stopped/retrying, restarted, and exhausted-retry notifications use the existing feedback overlay and respect its stream-proof setting. When Capture overlay is off, notifications update the app status and tray tooltip without creating an overlay WebView.


### Audio recovery and faster playback (0.3.17)

Desktop/microphone streams are reopened on capture failure and follow changes to Windows' default device. Failed app loopbacks are retried after 3 seconds instead of being excluded until the app closes. Asynchronous activation has a 5-second timeout. Missing sources contribute silence, keeping the existing audio pipe, video encoder, replay history, and recordings running. Isolated-track write failures also retry without terminating the mixed audio feed. Existing status/overlay notifications report audio loss and recovery. Device interruptions still leave gaps in captured sound; live headset/device switching needs verification.

Playback starts reading the original immediately. When WebView2 reports native HEVC support, rsclip tries it without conversion and falls back if no frame arrives within 4 seconds. Compatibility previews now use H.264/AAC MP4 with NVENC, falling back to a two-thread x264 encode if needed, retaining up to 1080p/60 quality. Eight recent base previews are cached (2 GiB budget, except the active preview). Switching clips no longer discards the previous preview. Per-clip audio previews reuse this video and encode only the mixed audio. Old WebM previews are unused and can be removed from the playback-cache folder when rsclip is closed.
