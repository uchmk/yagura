# Source is uchmk/ito scripts/lanes/; an app's copy is written by its scripts/lanes.sh sync, so edit ito's.
#
# Claude Code asks "Is this a project you trust?" the first time it starts in
# a folder, and a Windows run starts it in a new folder every time
# (<scratch>\run-<time>\projN), so the run stopped at the question until
# someone pressed Enter. Claude Code takes a folder as trusted when it or a
# parent has projects["<folder>"].hasTrustDialogAccepted in ~/.claude.json, so
# the lane trusts its scratch folder once and every run folder under it is
# trusted. Only the lane's own scratch folder goes through this, never a
# folder the owner works in.
# Dot-sourced by auto-wintest.ps1 and checked by check-ps1.ps1.

# Writes hasTrustDialogAccepted for $Folder into Claude Code's config (a
# folder with the key already true is left alone, and nothing else in the file
# is touched). Returns $true when it wrote, $false when the folder was already
# trusted or the config could not be read or written (a failed write must not
# stop a run: the question then waits for Enter as before).
function Set-ClaudeFolderTrust([string]$Folder, [string]$Config = (Join-Path $HOME '.claude.json')) {
    # A drive-letter path is already full; GetFullPath on a Linux runner would
    # take `C:\x` for a relative name and put the working folder in front.
    $full = if ($Folder -match '^[A-Za-z]:[\\/]') { $Folder } else { [IO.Path]::GetFullPath($Folder) }
    $key = $full.TrimEnd('\', '/') -replace '\\', '/'
    try {
        if (-not (Test-Path -LiteralPath $Config)) { return $false }
        $json = Get-Content -LiteralPath $Config -Raw | ConvertFrom-Json -AsHashTable
        if ($null -eq $json.projects) { $json.projects = @{} }
        if ($json.projects[$key].hasTrustDialogAccepted -eq $true) { return $false }
        if ($null -eq $json.projects[$key]) { $json.projects[$key] = @{} }
        $json.projects[$key].hasTrustDialogAccepted = $true
        $tmp = "$Config.$PID.tmp"
        [IO.File]::WriteAllText($tmp, ($json | ConvertTo-Json -Depth 100), [Text.UTF8Encoding]::new($false))
        Move-Item -LiteralPath $tmp -Destination $Config -Force
        return $true
    } catch {
        return $false
    }
}
