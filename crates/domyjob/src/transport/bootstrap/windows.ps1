$ErrorActionPreference="Stop"
$ProgressPreference="SilentlyContinue"
$base=[IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA "domyjob\bootstrap"))
$null=New-Item -ItemType Directory -Force -Path $base
$work=Join-Path $base "@TICKET@"
$null=New-Item -ItemType Directory -Path $work
$install=Join-Path $work "install"
$result=1
$child=$null
try {
    Add-Type -TypeDefinition @'
@INPUT_READER@
'@
    $source=[DomyjobBootstrapInput]::ReadBytes(@SOURCE_BYTES@)
    if ($source.Length -ne @SOURCE_BYTES@) { throw "truncated bootstrap source" }
    $archive=Join-Path $work "source.tar"
    [IO.File]::WriteAllBytes($archive,$source)
    [Console]::InputEncoding=New-Object Text.UTF8Encoding($false)
    $start=New-Object Diagnostics.ProcessStartInfo
    $start.FileName="tar.exe"
    $start.Arguments='-xmf "'+$archive+'" -C "'+$work+'"'
    $start.WorkingDirectory=$work
    $start.UseShellExecute=$false
    $start.RedirectStandardInput=$true
    $start.RedirectStandardOutput=$true
    $child=[Diagnostics.Process]::Start($start)
    $child.StandardInput.Close()
    $child.StandardOutput.BaseStream.CopyTo([Console]::OpenStandardError())
    $child.WaitForExit()
    $result=$child.ExitCode
    $child.Dispose()
    $child=$null
    if ($result -ne 0) { throw "source extraction failed" }
    $result=1
    $env:CARGO_TARGET_DIR=Join-Path $work "target"
    $null=New-Item -ItemType Directory -Force -Path $env:CARGO_TARGET_DIR
    [IO.File]::WriteAllText((Join-Path $env:CARGO_TARGET_DIR "CACHEDIR.TAG"), "@CACHE_TAG@`n")
    $env:MISE_TRUSTED_CONFIG_PATHS=$work
    $start=New-Object Diagnostics.ProcessStartInfo
    $start.FileName="mise.exe"
    $start.Arguments='x -- cargo install --jobs @BUILD_JOBS@ --debug --locked --path crates/domyjob --bin domyjob --root "'+$install+'" --force'
    $start.WorkingDirectory=$work
    $start.UseShellExecute=$false
    $start.RedirectStandardInput=$true
    $start.RedirectStandardOutput=$true
    $child=[Diagnostics.Process]::Start($start)
    $child.StandardInput.Close()
    $child.StandardOutput.BaseStream.CopyTo([Console]::OpenStandardError())
    $child.WaitForExit()
    $result=$child.ExitCode
    $child.Dispose()
    $child=$null
    if ($result -eq 0) {
        $start=New-Object Diagnostics.ProcessStartInfo
        $start.FileName=Join-Path $install "bin\domyjob.exe"
        $start.Arguments="publish-node"
        $start.WorkingDirectory=$env:USERPROFILE
        $start.UseShellExecute=$false
        $child=[Diagnostics.Process]::Start($start)
        $child.WaitForExit()
        $result=$child.ExitCode
        $child.Dispose()
        $child=$null
    }
} catch {
    [Console]::Error.WriteLine("domyjob: bootstrap failed: $_")
    if ($result -eq 0) { $result=1 }
} finally {
    if ($null -ne $child) {
        try { if (-not $child.HasExited) { $child.Kill() }; $child.WaitForExit() } catch { [Console]::Error.WriteLine("domyjob: bootstrap child cleanup failed: $_") }
        $child.Dispose()
    }
    try { Set-Location $env:USERPROFILE } catch { [Console]::Error.WriteLine("domyjob: bootstrap cwd recovery failed: $_") }
    try { Remove-Item -LiteralPath $work -Recurse -Force } catch { [Console]::Error.WriteLine("domyjob: bootstrap cleanup failed: $_"); if ($result -eq 0) { $result=1 } }
}
exit $result
