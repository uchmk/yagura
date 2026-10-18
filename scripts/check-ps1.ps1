# Source is uchmk/ito scripts/lanes/; an app's copy is written by its scripts/lanes.sh sync, so edit ito's.
#
# Parse every PowerShell script under scripts/ and fail on the first syntax
# error, then check lane-marks.ps1's and lane-status.ps1's functions on small
# samples (each when the app takes it), then the app's own checks when it has
# scripts/check-ps1-app.ps1 (dot-sourced: Expect and $script:bad are shared).
# Nothing else is run.
#
# The Windows lanes' scheduled task runs scripts/auto-wintest.ps1 from a
# worktree that the script itself moves to origin/main. A copy that does not
# parse never gets as far as moving it, so one syntax error on `main` stopped
# both lanes for three days, long after the fix had landed (v0.78.167 to
# v0.80.13). CI runs this on every change to scripts/, and scripts/verify.sh
# does when pwsh is installed.
#
#   pwsh -NoProfile -File scripts/check-ps1.ps1

$bad = 0
foreach ($f in Get-ChildItem -Path (Join-Path $PSScriptRoot '*.ps1') | Sort-Object Name) {
    $errors = $null
    [void][System.Management.Automation.Language.Parser]::ParseFile($f.FullName, [ref]$null, [ref]$errors)
    foreach ($e in $errors) {
        Write-Output ("{0}:{1}: {2}" -f $f.Name, $e.Extent.StartLineNumber, $e.Message)
        $bad++
    }
}
if ($bad) { Write-Output "$bad PowerShell syntax error(s)."; exit 1 }
Write-Output 'PowerShell scripts parse.'

function Expect([string]$What, $Got, $Want) {
    if ("$Got" -ne "$Want") { Write-Output "check-ps1: $What is '$Got', not '$Want'"; $script:bad++ }
}

# auto-wintest.ps1's marks made again on a main that moved (lane-marks.ps1):
# 2.1 ticked (main left it alone), 2.2 ticked (main reworded it: dropped),
# 2.3 `[~]` (main ticked it too: kept, not dropped), a key ticked; main added
# 2.5 and has CRLF.
if (Test-Path (Join-Path $PSScriptRoot 'lane-marks.ps1')) {
    . (Join-Path $PSScriptRoot 'lane-marks.ps1')
    $base = "## 2. Panes`n`n- [ ] **2.1** a split`n- [ ] **2.2** another`n- [ ] **2.3** a third`n- [x] **2.4** done`n- [ ] ``F2`` rename"
    $ran = "## 2. Panes`n`n- [x] **2.1** a split`n- [x] **2.2** another`n- [~] **2.3** a third`n- [x] **2.4** done`n- [x] ``F2`` rename"
    $marks = @(Get-LaneMarks -Base $base -Branch $ran)
    Expect 'the marks a run set' (($marks | ForEach-Object { "$($_.Mark) $($_.Rest)" }) -join '|') 'x **2.1** a split|x **2.2** another|~ **2.3** a third|x `F2` rename'
    $main = "## 2. Panes`r`n`r`n- [ ] **2.1** a split`r`n- [ ] **2.2** another, reworded`r`n- [~] **2.3** a third`r`n- [x] **2.4** done`r`n- [ ] **2.5** new`r`n- [ ] ``F2`` rename`r`n"
    $r = Set-LaneMarks -Main $main -Marks $marks
    Expect 'the marks on main' $r.Text "## 2. Panes`r`n`r`n- [x] **2.1** a split`r`n- [ ] **2.2** another, reworded`r`n- [~] **2.3** a third`r`n- [x] **2.4** done`r`n- [ ] **2.5** new`r`n- [x] ``F2`` rename`r`n"
    Expect 'the marks dropped' ($r.Dropped -join ',') '2.2'
    $r = Set-LaneMarks -Main "- [ ] ``F3`` other`n" -Marks @(Get-LaneMarks -Base '- [ ] `F2` rename' -Branch '- [x] `F2` rename')
    Expect 'a key dropped' "$($r.Dropped)|$($r.Text)" "``F2`` rename|- [ ] ``F3`` other`n"
    Expect 'no marks' (Set-LaneMarks -Main "a`nb" -Marks @()).Text "a`nb"
    if ($bad) { exit 1 }
    Write-Output 'Lane marks are made again as they should be.'
}

# The lane's status issue (lane-status.ps1): the user's folder written as ~,
# no @ that mentions anyone, the log and the dirty paths folded away, and the
# history kept to its length.
if (Test-Path (Join-Path $PSScriptRoot 'lane-status.ps1')) {
    . (Join-Path $PSScriptRoot 'lane-status.ps1')
    $when = [datetimeoffset]::Parse('2026-10-10T21:20:00+09:00').LocalDateTime
    $b = Format-LaneStatus -Lane win -When $when -Script 'v1.2.3 (abc1234)' -Outcome 'Done: WINTEST_DONE @claude' -Task t `
        -Said @('[x] Script: C:\Users\me\dev\a.ps1') -Dirty @(' M TESTING-CHECKS.md') -Failures 2 -UserDir 'C:\Users\me'
    Expect 'the user folder' ($b -match [regex]::Escape('C:\Users\me')) $false
    Expect 'the folder as ~' ($b -match [regex]::Escape('[x] Script: ~\dev\a.ps1')) $true
    Expect 'a mention' ($b -match '@claude') $false
    Expect 'the dirty worktree' ($b -match '\*\*Worktree:\*\* \*\*dirty\*\* \(1 changed') $true
    Expect 'the failures' ($b -match 'Failed runs in a row:\*\* 2') $true
    Expect 'the dirty paths' ($b -match '(?s)git status --porcelain</summary>.* M TESTING-CHECKS\.md') $true
    $b = Format-LaneStatus -Lane arm -When $when -Script 'v1' -Outcome 'Nothing new.' -Task t
    Expect 'a clean worktree' ($b -match '\*\*Worktree:\*\* clean') $true
    Expect 'no log' ($b -match '<details>') $false
    $h = Join-Path ([IO.Path]::GetTempPath()) "lane-history-$PID.txt"
    Remove-Item -LiteralPath $h -ErrorAction SilentlyContinue
    foreach ($i in 1..5) { $kept = Add-LaneHistory $h "firing $i" 3 }
    Remove-Item -LiteralPath $h -ErrorAction SilentlyContinue
    Expect 'the history' ($kept -join ',') 'firing 5,firing 4,firing 3'
    if ($bad) { exit 1 }
    Write-Output 'The lane status reads as it should.'
}

# The scratch folder's trust (lane-trust.ps1): written once, the rest of the
# config kept, and nothing written when the key is already there.
if (Test-Path (Join-Path $PSScriptRoot 'lane-trust.ps1')) {
    . (Join-Path $PSScriptRoot 'lane-trust.ps1')
    $c = Join-Path ([IO.Path]::GetTempPath()) "lane-trust-$PID.json"
    [IO.File]::WriteAllText($c, '{"numStartups":3,"projects":{"C:/dev/a":{"hasTrustDialogAccepted":false}}}')
    Expect 'trust written' (Set-ClaudeFolderTrust 'C:\scratch\Temp\' $c) $true
    $t = Get-Content -LiteralPath $c -Raw | ConvertFrom-Json -AsHashTable
    Expect 'the folder trusted' $t.projects['C:/scratch/Temp'].hasTrustDialogAccepted $true
    Expect 'the other config kept' "$($t.numStartups)|$($t.projects['C:/dev/a'].hasTrustDialogAccepted)" '3|False'
    Expect 'trust already there' (Set-ClaudeFolderTrust 'C:\scratch\Temp' $c) $false
    Expect 'no config' (Set-ClaudeFolderTrust 'C:\scratch\Temp' "$c.missing") $false
    Remove-Item -LiteralPath $c -ErrorAction SilentlyContinue
    if ($bad) { exit 1 }
    Write-Output 'The scratch folder is trusted as it should be.'
}

$app = Join-Path $PSScriptRoot 'check-ps1-app.ps1'
if (Test-Path $app) {
    . $app
    if ($bad) { exit 1 }
}
