# broom-stub v1 -- baked into the golden by the prep, never changes: runs the current broom\__SCRIPT__ that the stage
# puts on BROOMWIN (checked against the server's sha256 every boot), so the script follows the server's version
# without a new golden. BROOMWIN has no drive letter -> volume path. ASCII only.
$v = Get-Volume -FileSystemLabel BROOMWIN -ErrorAction SilentlyContinue
if (-not $v) { exit }
$src = $v.Path + 'broom\__SCRIPT__'
if (-not [IO.File]::Exists($src)) { exit }
$run = "$env:SystemRoot\Temp\__SCRIPT__.run.ps1"
[IO.File]::Copy($src, $run, $true)
& powershell -NoProfile -ExecutionPolicy Bypass -File $run
