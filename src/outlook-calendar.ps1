# Read the active Classic Outlook calendar without opening or changing Outlook.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)
$ProgressPreference = 'SilentlyContinue'
try {
    $outlook = [Runtime.InteropServices.Marshal]::GetActiveObject('Outlook.Application')
    $calendar = $outlook.GetNamespace('MAPI').GetDefaultFolder(9)
    $from = [DateTime]::Today
    $until = $from.AddDays(8)
    if ($env:FLODO_CALENDAR_FROM -and $env:FLODO_CALENDAR_UNTIL) {
        $from = [DateTime]::ParseExact($env:FLODO_CALENDAR_FROM, 'yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture)
        $until = [DateTime]::ParseExact($env:FLODO_CALENDAR_UNTIL, 'yyyy-MM-dd', [Globalization.CultureInfo]::InvariantCulture)
        if ($until -le $from -or ($until - $from).TotalDays -gt 45) { throw 'Invalid calendar date range' }
    }
    $items = $calendar.Items
    $items.Sort('[Start]')
    $items.IncludeRecurrences = $true
    # Jet comparisons use local wall-clock times, as do Start and End.
    $filter = "[Start] < '{0}' AND [End] > '{1}'" -f $until.ToString('g'), $from.ToString('g')
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
    [ordered]@{ events = @($events.ToArray()); error = $null } | ConvertTo-Json -Depth 4 -Compress
} catch {
    [ordered]@{
        events = @()
        error = 'Could not read Outlook. Open Classic Outlook in this Windows session, then refresh.'
    } | ConvertTo-Json -Compress
}
