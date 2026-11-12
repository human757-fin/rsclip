$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$root = Join-Path $env:LOCALAPPDATA 'rsclip\tools'
New-Item -ItemType Directory -Force -Path $root | Out-Null
$stage = Join-Path $root ([Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
  $zip = Join-Path $stage 'ffmpeg.zip'
  $url = 'https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip'
  Invoke-WebRequest -UseBasicParsing -Uri ($url + '.sha256') -OutFile (Join-Path $stage 'checksum') -TimeoutSec 30
  $expected = ((Get-Content -Raw (Join-Path $stage 'checksum')).Trim() -split '\s+')[0]
  if ($expected -notmatch '^[a-fA-F0-9]{64}$') { throw 'Invalid FFmpeg checksum.' }
  Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $zip -TimeoutSec 300
  if ((Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash -ne $expected) { throw 'FFmpeg checksum mismatch.' }
  Expand-Archive -LiteralPath $zip -DestinationPath (Join-Path $stage 'unpacked')
  $bin = Get-ChildItem -LiteralPath (Join-Path $stage 'unpacked') -Filter ffmpeg.exe -Recurse | Select-Object -First 1
  if (!$bin) { throw 'FFmpeg archive contains no executable.' }
  $filters = & $bin.FullName -hide_banner -filters 2>&1 | Out-String
  if ($LASTEXITCODE -ne 0 -or $filters -notmatch 'ddagrab') { throw 'FFmpeg build lacks desktop capture.' }
  $encoders = & $bin.FullName -hide_banner -encoders 2>&1 | Out-String
  if ($LASTEXITCODE -ne 0 -or $encoders -notmatch 'h264_nvenc') { throw 'FFmpeg build lacks NVENC.' }
  $destination = Join-Path $root 'ffmpeg'
  New-Item -ItemType Directory -Force -Path $destination | Out-Null
  Copy-Item -LiteralPath (Join-Path $bin.DirectoryName 'ffprobe.exe') -Destination $destination -Force
  Copy-Item -LiteralPath $bin.FullName -Destination $destination -Force
} finally {
  # Only remove the uniquely created staging directory beneath the fixed tools root.
  if ([IO.Path]::GetDirectoryName($stage) -eq $root) { Remove-Item -LiteralPath $stage -Recurse -Force }
}
