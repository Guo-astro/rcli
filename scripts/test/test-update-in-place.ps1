param([string]$Binary = '')
$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$SavedCI = $env:CI
$env:CI = '1'

# Load only the transaction functions, never the installer's network/PATH code.
$Root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Tokens = $null
$Errors = $null
$Ast = [Management.Automation.Language.Parser]::ParseFile((Join-Path $Root 'install.ps1'), [ref]$Tokens, [ref]$Errors)
if ($Errors.Count) { throw ($Errors | Out-String) }
foreach ($Definition in $Ast.FindAll({ param($Node)
    $Node -is [Management.Automation.Language.FunctionDefinitionAst] -and $Node.Name -in @('Fail', 'Update-InPlace')
}, $false)) { . ([ScriptBlock]::Create($Definition.Extent.Text)) }

$Work = Join-Path ([IO.Path]::GetTempPath()) ('wally-update-test-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Work | Out-Null
function Assert([bool]$Condition, [string]$Message) { if (-not $Condition) { throw $Message }; Write-Host "ok $Message" }
<#
.SYNOPSIS
Creates an isolated update fixture with release files and unrelated files.
.PARAMETER Name
Name of the fixture directory under this test run's temporary root.
#>
function Fixture([string]$Name) {
    $Dir = Join-Path $Work $Name
    $Source = Join-Path $Dir 'source'
    $Target = Join-Path $Dir 'target'
    New-Item -ItemType Directory -Path $Source, $Target | Out-Null
    foreach ($File in 'wally.exe', 'runtime.dll') {
        [IO.File]::WriteAllText((Join-Path $Source $File), "new $File")
        [IO.File]::WriteAllText((Join-Path $Target $File), "old $File")
    }
    foreach ($File in 'notes.old', 'wally.exe.old', 'runtime.dll.123.old', 'notes.txt') {
        [IO.File]::WriteAllText((Join-Path $Target $File), "keep $File")
    }
    return @{ Source = $Source; Target = $Target }
}

# Inject a placement failure or hold a retired file open while retaining the
# real filesystem operations. These exercise the production rollback/receipt.
$script:FailPlacement = $false
$script:FailRestore = $false
$script:HoldBackup = $false
$script:Held = $null
$Running = $null
function Move-Item {
    param([string]$LiteralPath, [string]$Destination, [string]$ErrorAction)
    if ($script:FailPlacement -and (Split-Path $LiteralPath -Leaf) -eq 'wally.exe') { throw 'injected placement failure' }
    Microsoft.PowerShell.Management\Move-Item -LiteralPath $LiteralPath -Destination $Destination -ErrorAction Stop
    if ($script:HoldBackup -and $null -eq $script:Held) {
        $Backup = Get-ChildItem -LiteralPath (Split-Path $Destination -Parent) -Filter '*.wally-backup-*' | Select-Object -First 1
        $script:Held = [IO.File]::Open($Backup.FullName, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
    }
}
function Rename-Item {
    param([string]$LiteralPath, [string]$NewName, [string]$ErrorAction)
    if ($script:FailRestore -and $LiteralPath -like '*.wally-backup-*') { throw 'injected restoration failure' }
    Microsoft.PowerShell.Management\Rename-Item -LiteralPath $LiteralPath -NewName $NewName -ErrorAction Stop
}

try {
    $F = Fixture 'success'
    Update-InPlace $F.Source $F.Target
    Assert (([IO.File]::ReadAllText((Join-Path $F.Target 'wally.exe'))) -eq 'new wally.exe') 'new executable installed'
    foreach ($File in 'notes.old', 'wally.exe.old', 'runtime.dll.123.old', 'notes.txt') {
        Assert (([IO.File]::ReadAllText((Join-Path $F.Target $File))) -eq "keep $File") "unrelated $File preserved"
    }
    Assert (@(Get-ChildItem -LiteralPath $F.Target -Filter '*.wally-backup-*').Count -eq 0) 'unlocked backups removed'

    $F = Fixture 'interrupted-recovery'
    $RecoveryId = [Guid]::NewGuid().ToString('N')
    $RecoveryBackup = Join-Path $F.Target "wally.exe.wally-backup-$RecoveryId"
    $RecoveryPending = Join-Path $F.Target ".wally-update-$RecoveryId.pending"
    [IO.File]::WriteAllText($RecoveryBackup, 'recovery executable')
    [IO.File]::WriteAllText($RecoveryPending, 'incomplete receipt')
    $Warnings = @(& { Update-InPlace $F.Source $F.Target } 3>&1)
    Assert (@($Warnings | Where-Object { $_ -is [Management.Automation.WarningRecord] -and $_.Message -match 'interrupted' }).Count -eq 1) 'interrupted recovery warning surfaced'
    Assert (([IO.File]::ReadAllText($RecoveryBackup)) -eq 'recovery executable') 'uncommitted recovery backup preserved'
    Assert (([IO.File]::ReadAllText($RecoveryPending)) -eq 'incomplete receipt') 'uncommitted pending receipt preserved'
    Assert (([IO.File]::ReadAllText((Join-Path $F.Target 'wally.exe'))) -eq 'new wally.exe') 'warning does not prevent a new update'

    $F = Fixture 'rollback'
    $script:FailPlacement = $true
    $Failed = $false
    try { Update-InPlace $F.Source $F.Target } catch { $Failed = $true }
    $script:FailPlacement = $false
    Assert $Failed 'placement failure reported'
    foreach ($File in 'wally.exe', 'runtime.dll') {
        Assert (([IO.File]::ReadAllText((Join-Path $F.Target $File))) -eq "old $File") "$File restored on rollback"
    }
    Assert (@(Get-ChildItem -LiteralPath $F.Target -Filter '.wally-update-*.json').Count -eq 0) 'rollback publishes no cleanup receipt'

    $F = Fixture 'failed-restoration'
    $script:FailPlacement = $true
    $script:FailRestore = $true
    $Reason = ''
    try { Update-InPlace $F.Source $F.Target } catch { $Reason = $_.Exception.Message }
    $script:FailPlacement = $false
    $script:FailRestore = $false
    Assert ($Reason -match 'Recovery backups remain') 'failed restoration reports recovery backups'
    Assert (@(Get-ChildItem -LiteralPath $F.Target -Filter '*.wally-backup-*').Count -gt 0) 'failed restoration retains recovery files'
    Assert (@(Get-ChildItem -LiteralPath $F.Target -Filter '.wally-update-*.json').Count -eq 0) 'failed restoration authorizes no cleanup'

    $F = Fixture 'concurrent-update'
    $HeldLock = [IO.File]::Open((Join-Path $F.Target '.wally-update.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
    $Failed = $false
    try { Update-InPlace $F.Source $F.Target } catch { $Failed = $true } finally { $HeldLock.Dispose() }
    Assert $Failed 'second installer cannot enter an active transaction'
    Assert (([IO.File]::ReadAllText((Join-Path $F.Target 'wally.exe'))) -eq 'old wally.exe') 'active transaction leaves original untouched'

    $F = Fixture 'directory-collision'
    Remove-Item -LiteralPath (Join-Path $F.Target 'runtime.dll')
    New-Item -ItemType Directory -Path (Join-Path $F.Target 'runtime.dll') | Out-Null
    $Failed = $false
    try { Update-InPlace $F.Source $F.Target } catch { $Failed = $true }
    Assert $Failed 'directory collision refused before replacement'
    Assert (([IO.File]::ReadAllText((Join-Path $F.Target 'wally.exe'))) -eq 'old wally.exe') 'preflight leaves executable unchanged'

    # A mapped executable is Windows' real rename-allowed/delete-denied case.
    $F = Fixture 'running-executable'
    $Cs = Join-Path $Work 'Waiting.cs'
    [IO.File]::WriteAllText($Cs, 'class Waiting { static void Main() { System.Threading.Thread.Sleep(300000); } }')
    $Exe = Join-Path $F.Target 'wally.exe'
    Remove-Item -LiteralPath $Exe
    $Compile = "Add-Type -OutputType ConsoleApplication -OutputAssembly '" + $Exe.Replace("'", "''") +
        "' -TypeDefinition (Get-Content -Raw -LiteralPath '" + $Cs.Replace("'", "''") + "')"
    powershell.exe -NoProfile -NonInteractive -Command $Compile
    Assert ($LASTEXITCODE -eq 0 -and (Test-Path -LiteralPath $Exe)) 'running executable fixture compiled'
    $Running = Start-Process -FilePath $Exe -PassThru -WindowStyle Hidden
    # Wait for its CLR startup before replacing the executable mapping.
    Start-Sleep -Milliseconds 500
    Update-InPlace $F.Source $F.Target
    Assert (([IO.File]::ReadAllText($Exe)) -eq 'new wally.exe') 'running executable can be replaced in place'
    Assert (@(Get-ChildItem -LiteralPath $F.Target -Filter 'wally.exe.wally-backup-*').Count -eq 1) 'mapped executable backup remains for later startup'
    Stop-Process -Id $Running.Id
    $Running.WaitForExit()
    $Running.Dispose()
    $Running = $null

    $F = Fixture 'locked'
    $script:HoldBackup = $true
    Update-InPlace $F.Source $F.Target
    $script:HoldBackup = $false
    $Receipts = @(Get-ChildItem -LiteralPath $F.Target -Filter '.wally-update-*.json')
    Assert ($Receipts.Count -eq 1) 'successful update publishes one receipt'
    $Receipt = Get-Content -Raw -LiteralPath $Receipts[0].FullName | ConvertFrom-Json
    Assert ($Receipt.version -eq 1 -and $Receipt.backups.Count -eq 2) 'receipt records exact retired files'
    $Remaining = @(Get-ChildItem -LiteralPath $F.Target -Filter '*.wally-backup-*')
    Assert ($Remaining.Count -eq 1) 'locked backup retained for later cleanup'
    $Hash = (Get-FileHash -LiteralPath $Remaining[0].FullName -Algorithm SHA256).Hash
    Assert (@($Receipt.backups | Where-Object { $_.name -eq $Remaining[0].Name -and $_.sha256 -eq $Hash }).Count -eq 1) 'receipt hash identifies retained backup'
    $script:Held.Dispose()
    $script:Held = $null

    if ($Binary) {
        # The real built binary consumes the receipt generated by PowerShell.
        # Copy its runtime DLLs too; this fixture never becomes an installation.
        Copy-Item -LiteralPath $Binary -Destination (Join-Path $F.Target 'wally.exe') -Force
        Get-ChildItem -LiteralPath (Split-Path $Binary -Parent) -Filter '*.dll' -File | Copy-Item -Destination $F.Target -Force
        $Probe = Start-Process -FilePath (Join-Path $F.Target 'wally.exe') -ArgumentList '--version' -WorkingDirectory $F.Target -WindowStyle Hidden -Wait -PassThru
        try { Assert ($Probe.ExitCode -eq 0) 'built executable starts in disposable fixture' } finally { $Probe.Dispose() }
        Assert (-not (Test-Path -LiteralPath $Remaining[0].FullName)) 'startup consumes installer-owned backup'
        Assert (-not (Test-Path -LiteralPath $Receipts[0].FullName)) 'startup removes completed receipt'
        foreach ($File in 'notes.old', 'wally.exe.old', 'runtime.dll.123.old', 'notes.txt') {
            Assert (([IO.File]::ReadAllText((Join-Path $F.Target $File))) -eq "keep $File") "startup preserves $File"
        }
    }
} finally {
    if ($null -ne $script:Held) { $script:Held.Dispose() }
    if ($null -ne $Running) {
        if (-not $Running.HasExited) { Stop-Process -Id $Running.Id; $Running.WaitForExit() }
        $Running.Dispose()
    }
    $env:CI = $SavedCI
    # Work was created by this script; no installation, registry, or PATH writes.
    if ((Resolve-Path -LiteralPath $Work).Path -ne [IO.Path]::GetFullPath($Work)) { throw 'Unexpected fixture cleanup path' }
    # Windows can briefly retain an executable mapping after process exit.
    for ($Attempt = 0; $Attempt -lt 10; $Attempt++) {
        try { Remove-Item -LiteralPath $Work -Recurse -Force -ErrorAction Stop; break } catch {
            if ($Attempt -eq 9) { throw }
            Start-Sleep -Milliseconds 200
        }
    }
}
