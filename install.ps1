# Installs newsreel for this user and starts its engine:
#   irm https://raw.githubusercontent.com/nexxeln/torrential/main/install.ps1 | iex
# NEWSREEL_VERSION picks a release, such as v0.6.0. NEWSREEL_INSTALL_DIR picks the folder.
# NEWSREEL_DOWNLOAD_URL takes the place of the release's download folder, for a test.

& {
  $ErrorActionPreference = 'Stop'
  # Invoke-WebRequest downloads far faster without its progress bar.
  $ProgressPreference = 'SilentlyContinue'

  $repo = 'nexxeln/torrential'
  $target = 'x86_64-pc-windows-msvc'
  $dir = if ($env:NEWSREEL_INSTALL_DIR) { $env:NEWSREEL_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\newsreel' }
  $version = if ($env:NEWSREEL_VERSION) { $env:NEWSREEL_VERSION } else { 'latest' }

  # Windows on Arm runs the x64 build.
  if ($env:PROCESSOR_ARCHITECTURE -notin 'AMD64', 'ARM64') {
    throw "newsreel has no prebuilt binary for $env:PROCESSOR_ARCHITECTURE Windows. Build it from source: https://github.com/$repo#build-from-source"
  }

  $url = if ($env:NEWSREEL_DOWNLOAD_URL) { $env:NEWSREEL_DOWNLOAD_URL }
         elseif ($version -eq 'latest') { "https://github.com/$repo/releases/latest/download" }
         else { "https://github.com/$repo/releases/download/$version" }
  $asset = "newsreel-$target.zip"
  $tmp = Join-Path ([IO.Path]::GetTempPath()) "newsreel-install-$([guid]::NewGuid())"
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    Write-Host "Downloading $asset"
    foreach ($file in $asset, "$asset.sha256") {
      try {
        Invoke-WebRequest -UseBasicParsing -Uri "$url/$file" -OutFile (Join-Path $tmp $file)
      } catch {
        throw "Couldn't download $url/$file"
      }
    }
    $published = ((Get-Content -Raw (Join-Path $tmp "$asset.sha256")).Trim() -split '\s+')[0]
    $actual = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash
    if ($actual -ne $published) {
      throw "$asset doesn't match its published checksum, so nothing was installed."
    }
    Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath (Join-Path $tmp 'unpacked')

    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $exe = Join-Path $dir 'newsreel.exe'
    # Windows replaces no program while it runs, but renames one, so a running newsreel moves
    # aside and its engine deletes it the next time it starts.
    if (Test-Path $exe) {
      $old = Join-Path $dir 'newsreel.old'
      Remove-Item -Force $old -ErrorAction SilentlyContinue
      if (Test-Path $old) { $old = Join-Path $dir "newsreel.$PID.old" }
      Move-Item -Force $exe $old
    }
    Copy-Item (Join-Path $tmp 'unpacked\newsreel.exe') $exe
    Write-Host "Installed $(& $exe --version) to $exe"

    # The registry keeps the user's PATH with %VARIABLES% unexpanded, and so does this.
    $environment = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
    $path = $environment.GetValue('Path', '', 'DoNotExpandEnvironmentNames')
    if (($path -split ';') -notcontains $dir) {
      $parts = @($path.TrimEnd(';'), $dir) | Where-Object { $_ }
      $environment.SetValue('Path', ($parts -join ';'), 'ExpandString')
      # Setting a variable through .NET tells open programs that the environment changed.
      [Environment]::SetEnvironmentVariable('NEWSREEL_INSTALLED', '1', 'User')
      [Environment]::SetEnvironmentVariable('NEWSREEL_INSTALLED', $null, 'User')
      Write-Host "Added $dir to your PATH. Open a new terminal to run newsreel there."
    }
    $env:Path = "$dir;$env:Path"
    $environment.Close()

    & $exe status | Out-Null
    if ($LASTEXITCODE -ne 0) {
      throw "newsreel is installed, but its engine did not start. Run newsreel status to see why."
    }
    Write-Host 'The engine is running. Run newsreel to get started.'
  } finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }
}
