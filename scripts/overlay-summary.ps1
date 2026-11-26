# Summarizes the overlay's CSV (VICTORY_OVERLAY_CSV): numbers only, safe to share.
#   scripts\overlay-summary.ps1 <run.csv> [<run.csv> ...]
# Rows under 25 fps count as in-world (loading screens sit at 30). Prints the median of the
# frame columns and the peak of the memory columns.
param([Parameter(Mandatory, ValueFromRemainingArguments)][string[]]$Csv)
# Decimal points whatever the system's language, so summaries compare across machines.
[Threading.Thread]::CurrentThread.CurrentCulture = [Globalization.CultureInfo]::InvariantCulture

$medians = 'fps', 'ms', 'preparing_ms', 'copy_ms', 'copy_mb', 'texture_ms', 'texture_mb', 'gpu_wait_ms',
    'back_ms', 'draws', 'dropped', 'cpu_pct', 'gpu_pct'
$peaks = 'pipelines_making', 'textures_waiting', 'ram_process_gb', 'commit_gb', 'ram_used_gb', 'vram_used_gb',
    'shared_used_gb'

function Get-Median([double[]]$v) {
    $s = $v | Sort-Object
    if ($s.Count % 2) { $s[[int][math]::Floor($s.Count / 2)] } else { ($s[$s.Count / 2 - 1] + $s[$s.Count / 2]) / 2 }
}

foreach ($file in $Csv) {
    $rows = @(Import-Csv $file)
    $world = @($rows | Where-Object { $_.fps -ne '' -and [double]$_.fps -gt 0 -and [double]$_.fps -lt 25 })
    if (-not $world.Count) { "${file}: $($rows.Count) rows, none in-world"; continue }
    $col = { param($k) @($world | Where-Object { $_.$k -ne '' } | ForEach-Object { [double]$_.$k }) }
    $fps = @(& $col 'fps' | Sort-Object)
    $p10 = $fps[[math]::Max(0, [int][math]::Floor($fps.Count * 0.1) - 1)]
    $med = ($medians | Where-Object { (& $col $_).Count } | ForEach-Object { '{0}={1:0.0}' -f $_, (Get-Median (& $col $_)) }) -join ' '
    $peak = ($peaks | Where-Object { (& $col $_).Count } | ForEach-Object { '{0}={1:0.00}' -f $_, ((& $col $_) | Measure-Object -Maximum).Maximum }) -join ' '
    '{0}: in-world {1}/{2} s | p10 fps {3:0.00} | median {4} | peak {5}' -f (Split-Path $file -Leaf), $world.Count, $rows.Count, $p10, $med, $peak
}
