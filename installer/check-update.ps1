$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$headers = @{ 'User-Agent' = 'rsclip'; Accept = 'application/vnd.github+json' }
$release = Invoke-RestMethod -Headers $headers -Uri 'https://api.github.com/repos/human757-fin/rsclip/releases/latest' -TimeoutSec 30
if ($release.draft -or $release.prerelease) { throw 'No stable release available.' }
if ($release.tag_name -notmatch '^v(\d+\.\d+\.\d+)$') { throw 'Invalid release version.' }
$version = $Matches[1]
if ([version]$version -le [version]'__VERSION__') { exit 0 }
$name = "rsclip_${version}_x64-setup.exe"
$assets = @($release.assets | Where-Object name -eq $name)
if ($assets.Count -ne 1) { throw 'The release has no Windows setup installer.' }
$asset = $assets[0]
$url = "https://github.com/human757-fin/rsclip/releases/download/v$version/$name"
if ($asset.browser_download_url -cne $url) { throw 'Unexpected installer source.' }
if ($asset.digest -notmatch '^sha256:([a-fA-F0-9]{64})$') { throw 'GitHub did not provide a SHA256 digest. Update manually.' }
$expected = $Matches[1]
$root = Join-Path $env:LOCALAPPDATA 'rsclip\tools'
New-Item -ItemType Directory -Force -Path $root | Out-Null
$temp = Join-Path $root 'update.download'
Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $temp -TimeoutSec 300
if ((Get-FileHash -LiteralPath $temp -Algorithm SHA256).Hash -ne $expected) { throw 'Installer checksum mismatch.' }
Move-Item -LiteralPath $temp -Destination (Join-Path $root 'update-setup.exe') -Force
Set-Content -LiteralPath (Join-Path $root 'update.sha256') -Value $expected
Write-Output $version
