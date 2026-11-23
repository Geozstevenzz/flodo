# Read the active Classic Outlook calendar without opening or changing Outlook.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$ProgressPreference = 'SilentlyContinue'

$reader = {
    param([DateTime]$From, [DateTime]$Until)
    $ErrorActionPreference = 'Stop'
    try {
        $outlook = [Runtime.InteropServices.Marshal]::GetActiveObject('Outlook.Application')
    } catch {
        # The running object table can hide a running Outlook from another
        # process in the same session; COM activation then attaches to it.
        # Only try that when Outlook is already running, so it never opens one.
        if (-not (Get-Process -Name OUTLOOK -ErrorAction SilentlyContinue)) { throw }
        $outlook = New-Object -ComObject Outlook.Application
    }
    $calendar = $outlook.GetNamespace('MAPI').GetDefaultFolder(9)
    $items = $calendar.Items
    $items.Sort('[Start]')
    $items.IncludeRecurrences = $true
    # Jet comparisons use local wall-clock times, as do Start and End.
    $filter = "[Start] < '{0}' AND [End] > '{1}'" -f $Until.ToString('g'), $From.ToString('g')
    $matches = $items.Restrict($filter)
    $events = New-Object 'System.Collections.Generic.List[object]'
    $item = $matches.GetFirst()
    $count = 0
    while ($null -ne $item) {
        $count++
        if ($count -gt 2000) { throw 'Calendar result limit exceeded' }
        if ($item.Class -eq 26 -and $item.MeetingStatus -lt 5 -and $item.ResponseStatus -ne 4) {
            $events.Add([ordered]@{
                id = [string]$item.EntryID + ':' + $item.Start.ToString('yyyyMMddHHmm')
                title = [string]$item.Subject
                start = $item.Start.ToString('yyyy-MM-ddTHH:mm:ss')
                end = $item.End.ToString('yyyy-MM-ddTHH:mm:ss')
                location = [string]$item.Location
                all_day = [bool]$item.AllDayEvent
                is_meeting = [bool]($item.MeetingStatus -ne 0)
            })
        }
        $item = $matches.GetNext()
    }
    [ordered]@{ events = @($events.ToArray()); error = $null }
}

# An elevated process cannot reach the user's non-elevated Outlook over COM,
# and activating one would start a second, elevated Outlook. Run the reader
# instead as a hidden interactive-token task at normal integrity in this
# session, and read its result back from a file.
function Invoke-ReaderUnelevated([DateTime]$From, [DateTime]$Until) {
    $tag = 'Flodo-Outlook-' + [guid]::NewGuid().ToString('N')
    $temp = [IO.Path]::GetTempPath()
    $dir = Join-Path $temp $tag
    # Written by the unelevated task, so it must not sit in a folder this
    # elevated process created.
    $out = Join-Path $temp ($tag + '.json')
    $folder = $null
    $registered = $false
    New-Item -ItemType Directory -Path $dir | Out-Null
    try {
        $ps1 = Join-Path $dir 'read.ps1'
        $header = @'
param([string]$From, [string]$Until, [string]$Out)
$ErrorActionPreference = 'Stop'
$inv = [Globalization.CultureInfo]::InvariantCulture
$reader = {
'@
        $footer = @'
}
try {
    $result = & $reader ([DateTime]::ParseExact($From, 'yyyy-MM-dd', $inv)) ([DateTime]::ParseExact($Until, 'yyyy-MM-dd', $inv))
} catch {
    $result = [ordered]@{ events = @(); error = 'unavailable' }
}
[IO.File]::WriteAllText($Out, ($result | ConvertTo-Json -Depth 4 -Compress), (New-Object Text.UTF8Encoding($false)))
'@
        $utf8 = New-Object Text.UTF8Encoding($true)
        [IO.File]::WriteAllText($ps1, $header + "`n" + $reader.ToString() + "`n" + $footer, $utf8)

        # wscript with window style 0 keeps powershell.exe from opening a console.
        $command = 'powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -STA -File "{0}" -From {1} -Until {2} -Out "{3}"' -f $ps1, $From.ToString('yyyy-MM-dd'), $Until.ToString('yyyy-MM-dd'), $out
        $vbs = Join-Path $dir 'run.vbs'
        [IO.File]::WriteAllText($vbs, 'CreateObject("WScript.Shell").Run "' + $command.Replace('"', '""') + '", 0, True', (New-Object Text.ASCIIEncoding))

        $identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name
        $scheduler = New-Object -ComObject 'Schedule.Service'
        $scheduler.Connect()
        $folder = $scheduler.GetFolder('\')
        $definition = $scheduler.NewTask(0)
        $definition.Principal.UserId = $identity
        $definition.Principal.LogonType = 3  # interactive token: this user's session
        $definition.Principal.RunLevel = 0   # least privilege, not elevated
        $definition.Settings.Hidden = $true
        $definition.Settings.ExecutionTimeLimit = 'PT1M'
        $definition.Settings.DisallowStartIfOnBatteries = $false
        $definition.Settings.StopIfGoingOnBatteries = $false
        $action = $definition.Actions.Create(0)
        $action.Path = 'wscript.exe'
        $action.Arguments = '//B //Nologo "' + $vbs + '"'
        $task = $folder.RegisterTaskDefinition($tag, $definition, 2, $identity, $null, 3, $null)
        $registered = $true
        $null = $task.Run($null)

        $deadline = (Get-Date).AddSeconds(25)
        while (-not (Test-Path -LiteralPath $out)) {
            if ((Get-Date) -gt $deadline) { throw 'Outlook reader task timed out' }
            Start-Sleep -Milliseconds 200
        }
        # The file can exist before its writer has closed it.
        $raw = $null
        while ($null -eq $raw) {
            try { $raw = [IO.File]::ReadAllText($out) } catch {
                if ((Get-Date) -gt $deadline) { throw }
                Start-Sleep -Milliseconds 100
            }
        }
        $result = $raw | ConvertFrom-Json
        if ($result.error) { throw 'Outlook reader task failed' }
        $raw
    } finally {
        if ($registered) { try { $folder.DeleteTask($tag, 0) } catch {} }
        Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $out -Force -ErrorAction SilentlyContinue
    }
}

$elevated = $false
try {
    $from = [DateTime]::Today
    $until = $from.AddDays(8)
    if ($env:FLODO_CALENDAR_FROM -and $env:FLODO_CALENDAR_UNTIL) {
        $from = [DateTime]::ParseExact($env:FLODO_CALENDAR_FROM, 'yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture)
        $until = [DateTime]::ParseExact($env:FLODO_CALENDAR_UNTIL, 'yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture)
        if ($until -le $from -or ($until - $from).TotalDays -gt 45) { throw 'Invalid calendar date range' }
    }
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    $elevated = $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
    if ($elevated) {
        Invoke-ReaderUnelevated $from $until
    } else {
        & $reader $from $until | ConvertTo-Json -Depth 4 -Compress
    }
} catch {
    $message = 'Could not read Outlook. Open Classic Outlook in this Windows session, then refresh.'
    if ($elevated) {
        $message = 'Could not read Outlook while Flodo runs as administrator. Open Classic Outlook, or restart Flodo without administrator rights, then refresh.'
    }
    [ordered]@{ events = @(); error = $message } | ConvertTo-Json -Compress
}
