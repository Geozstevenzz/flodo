
# Flodo's updater. Flodo writes the variables above, starts this script with
# no console window, and quits. This waits for Flodo to exit, installs the
# offered version into the same npm prefix, records the outcome for the next
# launch, starts Flodo again and deletes itself. Every child runs inside this
# script's hidden console, so nothing appears on screen.
$ErrorActionPreference = 'Continue'
$log = Join-Path $StateDir 'update.log'
$resultFile = Join-Path $StateDir 'update-result.json'
$outFile = "$PSCommandPath.out"
$errFile = "$PSCommandPath.err"
$lines = New-Object System.Collections.Generic.List[string]
$lines.Add("Flodo update from $From to $To, started $((Get-Date).ToString('o'))")

# Test-only: install from a local tarball instead of the registry.
if ($env:FLODO_UPDATE_SPEC) { $Spec = $env:FLODO_UPDATE_SPEC }
$env:npm_config_update_notifier = 'false'
$env:npm_config_fund = 'false'
$env:npm_config_audit = 'false'
$env:npm_config_progress = 'false'
$env:npm_config_color = 'false'

function Write-Result($ok, $message) {
    $result = [ordered]@{
        ok          = [bool]$ok
        from        = $From
        to          = $To
        message     = $message
        finished_at = (Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ')
    }
    $json = $result | ConvertTo-Json -Compress
    try { [IO.File]::WriteAllText($resultFile, $json) } catch { $lines.Add("Could not write the result: $_") }
    $lines.Add("Result: $json")
}

# Arguments are double-quoted for the command line. Windows paths cannot
# contain a double quote, and Flodo strips trailing backslashes.
function Invoke-Npm {
    $npmArgs = @('install', '-g', $Spec, '--prefix', $Prefix) | ForEach-Object { '"' + $_ + '"' }
    if ($Node) {
        $file = $Node
        $argLine = ('"' + $NpmCli + '" ') + ($npmArgs -join ' ')
    } else {
        $file = 'cmd.exe'
        $argLine = '/d /c npm ' + ($npmArgs -join ' ')
    }
    $lines.Add("Running: $file $argLine")
    try {
        $p = Start-Process -FilePath $file -ArgumentList $argLine -NoNewWindow -PassThru `
            -WorkingDirectory ([IO.Path]::GetTempPath()) `
            -RedirectStandardOutput $outFile -RedirectStandardError $errFile
    } catch {
        return @{ code = -1; output = @("Could not start npm: $_") }
    }
    $null = $p.Handle # without this Windows PowerShell can lose the exit code
    if ($p.WaitForExit(600000)) { $code = $p.ExitCode } else {
        try { $p.Kill() } catch {}
        $code = -2
    }
    $output = @(Get-Content -LiteralPath $outFile -ErrorAction SilentlyContinue) +
        @(Get-Content -LiteralPath $errFile -ErrorAction SilentlyContinue)
    Remove-Item -LiteralPath $outFile, $errFile -Force -ErrorAction SilentlyContinue
    return @{ code = $code; output = $output }
}

function Get-Reason($run) {
    if ($run.code -eq -1) { return 'npm could not be started' }
    if ($run.code -eq -2) { return 'npm took longer than ten minutes' }
    foreach ($line in $run.output) {
        if ("$line" -match 'npm (error|ERR!) code (\w+)') { return "npm error $($Matches[2])" }
    }
    return "npm exited with code $($run.code)"
}

try {
    $flodo = Get-Process -Id $FlodoPid -ErrorAction SilentlyContinue
    if ($flodo -and -not $flodo.WaitForExit(60000)) {
        # Still open, so there is nothing to restart and the files are locked.
        Write-Result $false 'Flodo did not close within a minute'
        return
    }
    # Give Windows a moment to release the executable.
    Start-Sleep -Milliseconds 500

    $run = Invoke-Npm
    if ($run.code -ne 0) {
        $lines.Add("First attempt failed ($(Get-Reason $run)); retrying in 3 seconds.")
        Start-Sleep -Seconds 3
        $run = Invoke-Npm
    }
    $lines.Add('npm output (last 20 lines):')
    foreach ($line in @($run.output | Select-Object -Last 20)) { $lines.Add("  $line") }

    $installed = $null
    try {
        $installed = (Get-Content -LiteralPath (Join-Path $Package 'package.json') -Raw |
            ConvertFrom-Json).version
    } catch {}
    if ($run.code -ne 0) {
        Write-Result $false (Get-Reason $run)
    } elseif ($installed -ne $To) {
        Write-Result $false "npm finished but version $installed is installed"
    } else {
        Write-Result $true ''
    }

    # Start Flodo again whatever happened, so the app is never left missing.
    try {
        $psi = New-Object System.Diagnostics.ProcessStartInfo
        $psi.FileName = $Exe
        $psi.UseShellExecute = $false
        $psi.CreateNoWindow = $true
        if (Test-Path -LiteralPath $WorkDir -PathType Container) {
            $psi.WorkingDirectory = $WorkDir
        } else {
            $psi.WorkingDirectory = [Environment]::GetFolderPath('UserProfile')
        }
        $started = [System.Diagnostics.Process]::Start($psi)
        $lines.Add("Started Flodo again (process $($started.Id)).")
    } catch {
        $lines.Add("Could not start Flodo again: $_")
    }
} finally {
    $lines.Add("Finished $((Get-Date).ToString('o'))")
    try { [IO.File]::WriteAllLines($log, $lines) } catch {}
    Remove-Item -LiteralPath $outFile, $errFile, $PSCommandPath -Force -ErrorAction SilentlyContinue
}
