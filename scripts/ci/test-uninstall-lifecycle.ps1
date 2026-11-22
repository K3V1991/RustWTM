# Load only uninstaller functions and the teardown steps before the PATH
# update. All process, registry, native commands and file operations are fake.
$ErrorActionPreference = 'Stop'
$path = Join-Path (Split-Path $PSScriptRoot -Parent) 'uninstall.ps1'
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($path, [ref]$null, [ref]$parseErrors)
if ($parseErrors) { throw 'Uninstaller syntax is invalid' }
foreach ($name in 'Get-UninstallTarget', 'Invoke-RustWTMMaintenance', 'Remove-RustWTMRegistration', 'Stop-TargetPrograms', 'Invoke-UninstallCopy') {
    $definition = $ast.Find({
        param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $true)
    if (-not $definition) { throw "Missing uninstaller function $name" }
    $body = $definition.Extent.Text
    if ($name -eq 'Invoke-RustWTMMaintenance') {
        if (-not $body.Contains('& $Exe $Command')) { throw 'Native-command seam has changed' }
        $body = $body.Replace('& $Exe $Command', 'Invoke-NativeFixture $Exe $Command')
    }
    . ([scriptblock]::Create($body))
}

# Keep the production ordering around cleanup and recursive removal, while
# leaving the host PATH update out of the fixture.
$statements = @($ast.EndBlock.Statements | ForEach-Object { $_.Extent.Text })
$first = [array]::IndexOf($statements, 'Remove-RustWTMRegistration $target')
$last = [array]::IndexOf($statements, '$entries = @(Get-InstallPathEntries)')
if ($first -lt 0 -or $last -le $first) { throw 'Uninstall teardown fixture cannot locate its boundaries' }
$remove = [scriptblock]::Create(($statements[$first..($last - 1)] -join "`n"))
if ($remove.ToString() -match '(?i)::(?:Delete|SetEnvironmentVariable)\(') {
    throw 'Fixture contains a direct host mutation'
}

function Assert-Rejected([scriptblock]$Action, [string]$Message) {
    try { $null = & $Action }
    catch {
        if ($_.Exception.Message -notlike $Message) { throw }
        return
    }
    throw "Expected refusal: $Message"
}

function Invoke-PartialRemovalFixture($UninstallerAst, [string[]]$FileOrder, [string]$BlockedFile) {
    foreach ($name in 'Get-UninstallTarget', 'Remove-Tree') {
        $definition = $UninstallerAst.Find({
            param($node)
            $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
        }, $true)
        if (-not $definition) { throw "Missing tree fixture definition $name" }
        $body = $definition.Extent.Text
        if ($name -eq 'Remove-Tree') {
            $directoryDelete = '[IO.Directory]::Delete($item.FullName, $false)'
            if (-not $body.Contains($directoryDelete)) { throw 'Directory deletion seam has changed' }
            $body = $body.Replace($directoryDelete, 'Remove-EmptyFixtureDirectory $item.FullName')
        }
        if ($body -match '(?i)::(?:Delete|SetEnvironmentVariable)\(') { throw 'Tree fixture contains a direct host mutation' }
        . ([scriptblock]::Create($body))
    }
    $statement = $UninstallerAst.EndBlock.Statements |
        Where-Object { $_.Extent.Text.StartsWith('if (Test-Path -LiteralPath $InstallDir) {') }
    if (@($statement).Count -ne 1) { throw 'Install directory teardown statement not found' }
    $action = [scriptblock]::Create($statement.Extent.Text)
    if ($action.ToString() -match '(?i)::(?:Delete|SetEnvironmentVariable)\(') { throw 'Teardown fixture contains a direct host mutation' }
    $ServiceName = 'RustWTM'
    $InstallDir = 'C:\fixture-uninstall'
    $state = @{ Files = [Collections.Generic.List[string]]::new(); Blocked = $BlockedFile; Present = $true }
    foreach ($name in $FileOrder) { $state.Files.Add($name) }
    function Get-CimInstance { param($ClassName, $Filter) }
    function Get-InstallPathEntries { $InstallDir }
    function Test-InstalledShortcut { param($Link, $Gui) $true }
    function Test-Path {
        param($LiteralPath, $PathType)
        if ($LiteralPath -ieq $InstallDir) { return $state.Present }
        if ($LiteralPath -like '*.lnk') { return $true }
        $state.Files.Contains((Split-Path $LiteralPath -Leaf))
    }
    function Get-Item {
        param($LiteralPath, [switch]$Force)
        [pscustomobject]@{ FullName = $LiteralPath; PSIsContainer = $LiteralPath -ieq $InstallDir; Attributes = 0 }
    }
    function Get-ChildItem {
        param($LiteralPath, [switch]$Force)
        if ($LiteralPath -ine $InstallDir) { throw 'Unexpected directory enumeration' }
        foreach ($name in $state.Files) { [pscustomobject]@{ FullName = Join-Path $InstallDir $name } }
    }
    function Remove-Item {
        param($LiteralPath, [switch]$Force)
        $name = Split-Path $LiteralPath -Leaf
        if ($name -ieq $state.Blocked) { throw "Fixture locked $name" }
        if (-not $state.Files.Remove($name)) { throw "Unexpected leaf deletion $name" }
    }
    function Remove-EmptyFixtureDirectory {
        param($Path)
        if ($Path -ine $InstallDir -or $state.Files.Count) { throw 'Removed a non-empty fixture directory' }
        $state.Present = $false
    }
    function Write-Step { param($Message) }
    $target = Get-UninstallTarget $InstallDir
    $failure = $null
    try { & $action }
    catch { $failure = $_.Exception.Message }
    $remaining = $state.Files -join '|'
    $retryFailure = $null
    if ($failure) {
        $state.Blocked = ''
        try {
            $target = Get-UninstallTarget $InstallDir
            & $action
        }
        catch { $retryFailure = $_.Exception.Message }
    }
    [pscustomobject]@{ Failure = $failure; Remaining = $remaining; RetryFailure = $retryFailure; Complete = -not $state.Present }
}

$removalResult = Invoke-PartialRemovalFixture $ast @('wtm-service.exe', 'uninstall.ps1', 'rustwtm.exe') 'rustwtm.exe'
if ($removalResult.Failure -cne 'Fixture locked rustwtm.exe' -or
    $removalResult.Remaining -cne 'wtm-service.exe|uninstall.ps1|rustwtm.exe' -or
    $removalResult.RetryFailure -or -not $removalResult.Complete) {
    throw "Uninstall could not resume after partial removal: $($removalResult | ConvertTo-Json -Compress)"
}

$ServiceName = 'RustWTM'
$dir = Join-Path $env:ProgramFiles 'RustWTM [preview]'
$gui = Join-Path $dir 'rustwtm.exe'
$exe = Join-Path $dir 'wtm-service.exe'
$script:registry = $null
$script:linkPresent = $true
$script:linkTarget = $gui
$script:filesPresent = $true
$script:helperPresent = $true
$script:pathEntries = @($dir)
$script:result = 0
$script:deleteServiceBeforeFailure = $false
$script:trace = [Collections.Generic.List[string]]::new()

function Get-CimInstance { param($ClassName, $Filter)
    if ($ClassName -cne 'Win32_Service' -or $Filter -cne "Name = 'RustWTM'") { throw 'Unexpected service lookup' }
    $script:registry
}
function Get-InstallPathEntries { $script:pathEntries }
function Test-InstalledShortcut { param($Link, $Gui) $script:linkPresent -and $script:linkTarget -ieq $Gui }
function Test-Path {
    [CmdletBinding(DefaultParameterSetName='Pattern')]
    param(
        [Parameter(Position=0, ParameterSetName='Pattern')]$Path,
        [Parameter(ParameterSetName='Literal')]$LiteralPath,
        $PathType
    )
    if ($PSCmdlet.ParameterSetName -ne 'Literal') { throw 'Uninstaller used a wildcard path probe' }
    if ($LiteralPath -like '*.lnk') { return $script:linkPresent }
    if ($LiteralPath -ieq $exe) { return $script:helperPresent }
    if ($LiteralPath -ieq $gui -or $LiteralPath -ieq $dir) { return $script:filesPresent }
    $false
}
function Get-Process { param($Name, $ErrorAction)
    [pscustomobject]@{ Id = 1; Path = $gui.ToUpperInvariant() }
    [pscustomobject]@{ Id = 2; Path = "$dir-sibling\rustwtm.exe" }
    [pscustomobject]@{ Id = 3; Path = "$dir\nested\wtm.exe" }
    [pscustomobject]@{ Id = 4; Path = "$dir\wtm.exe" }
    [pscustomobject]@{ Id = 5; Path = $null }
}
function Stop-Process { param([Parameter(ValueFromPipeline)]$InputObject, [switch]$Force)
    process { $script:trace.Add("stop:$($InputObject.Id)") }
}
function Invoke-NativeFixture { param($Path, $Command)
    if ($Path -ine $exe) { throw 'Uninstaller selected another native helper' }
    $script:trace.Add($Command)
    if ($Command -eq 'uninstall' -and $script:deleteServiceBeforeFailure) { $script:registry = $null }
    $global:LASTEXITCODE = $script:result
}
function Remove-Tree { param($Path) $script:trace.Add("remove:$Path") }
function Remove-Item { param($LiteralPath, [switch]$Force) $script:trace.Add('remove:shortcut') }
function Write-Step { param($Message) }
function Write-Host { param($Object, $ForegroundColor) }

Assert-Rejected { Get-UninstallTarget ([IO.Path]::GetPathRoot($dir)) } 'Choose the RustWTM installation directory*'
$script:registry = [pscustomobject]@{ PathName = ('"' + "$dir-sibling\wtm-service.exe" + '" service') }
Assert-Rejected { Get-UninstallTarget $dir } 'The RustWTM service is registered at a different path*'
$script:registry.PathName = '"unclosed.exe'
Assert-Rejected { Get-UninstallTarget $dir } 'Could not identify the registered RustWTM service executable*'
$script:registry.PathName = '"%ProgramFiles%\RustWTM [preview]\wtm-service.exe" service'
$script:helperPresent = $false
Assert-Rejected { Get-UninstallTarget $dir } 'The RustWTM cleanup binary is missing*'
$script:helperPresent = $true

$target = Get-UninstallTarget "$dir\"
if (-not $target.HasService -or $target.Directory -ine $dir) { throw 'Matching registered installation was not resolved' }
$InstallDir = $target.Directory
$RemoveData = $false
$matchingRegistration = $script:registry
$script:deleteServiceBeforeFailure = $true
$script:result = 5
Assert-Rejected { & $remove } '*exit code 5*'
if (($script:trace -join '|') -cne 'uninstall') { throw 'Native uninstall failure reached destructive teardown' }

$script:trace.Clear()
$script:deleteServiceBeforeFailure = $false
$script:result = 0
$target = Get-UninstallTarget $dir
if ($target.HasService) { throw 'Retry ignored the service removal from the first attempt' }
& $remove
if (($script:trace -join '|') -cne "cleanup|stop:1|stop:4|remove:$dir|remove:shortcut") {
    throw 'Retry after service removal did not finish cleanup in order'
}

$script:trace.Clear()
$script:registry = $matchingRegistration
$script:linkTarget = "$dir-sibling\rustwtm.exe"
$target = Get-UninstallTarget $dir
& $remove
if (($script:trace -join '|') -cne "uninstall|stop:1|stop:4|remove:$dir") { throw 'Uninstall touched an unrelated process or shortcut' }

$script:trace.Clear()
$script:registry = $null
Assert-Rejected { Get-UninstallTarget $dir } 'The RustWTM shortcut belongs to another installation*'
$script:linkPresent = $false
$script:pathEntries = @("$dir-sibling")
Assert-Rejected { Get-UninstallTarget $dir } 'RustWTM installation ownership could not be verified*'
$script:pathEntries = @("$dir\")
$target = Get-UninstallTarget $dir
if ($target.HasService) { throw 'Retry selected a service that has already been removed' }
$script:result = 5
Assert-Rejected { & $remove } '*exit code 5*'
if (($script:trace -join '|') -cne 'cleanup') { throw 'Native cleanup failure reached destructive teardown' }

$script:trace.Clear()
$script:result = 0
$script:linkPresent = $true
$script:linkTarget = $gui
$target = Get-UninstallTarget $dir
& $remove
if (($script:trace -join '|') -cne "cleanup|stop:1|stop:4|remove:$dir|remove:shortcut") {
    throw 'Retry ran the wrong teardown stages or ran them out of order'
}

function New-Item { param($ItemType, $Path)
    if ($ItemType -ne 'Directory') { throw 'Wrong kind of temporary item' }
    $script:copyDir = $Path
    $script:trace.Add('temp-created')
}
function Set-Acl { param($LiteralPath, $AclObject)
    if ($LiteralPath -ine $script:copyDir) { throw 'Wrong temporary ACL target' }
    $script:trace.Add('temp-acl')
}
function Copy-Item { param($LiteralPath, $Destination, [switch]$Force)
    if ($LiteralPath -ine (Join-Path $dir 'uninstall.ps1') -or
        $Destination -ine (Join-Path $script:copyDir 'uninstall.ps1')) { throw 'Wrong temporary copy paths' }
    $script:trace.Add('temp-copy')
}
function powershell.exe {
    if ($env:RUSTWTM_UNINSTALL_COPY -ne '1') { throw 'Child launch marker was not set' }
    $script:trace.Add('child')
    $script:childArguments = @($args)
    if ($script:failLaunch) { throw 'Injected child launch failure' }
    $global:LASTEXITCODE = 17
}
$oldMarker = $env:RUSTWTM_UNINSTALL_COPY
try {
    $env:RUSTWTM_UNINSTALL_COPY = 'parent-marker'
    $self = Join-Path $dir 'uninstall.ps1'
    $script:trace.Clear()
    $script:failLaunch = $true
    Assert-Rejected { Invoke-UninstallCopy $self $dir } 'Injected child launch failure'
    if ($env:RUSTWTM_UNINSTALL_COPY -cne 'parent-marker' -or
        ($script:trace -join '|') -cne "temp-created|temp-acl|temp-copy|child|remove:$script:copyDir") {
        throw 'Child launch failure leaked temporary state'
    }
    $script:trace.Clear()
    $script:failLaunch = $false
    if ((Invoke-UninstallCopy $self $dir -RemoveData) -ne 17 -or
        $env:RUSTWTM_UNINSTALL_COPY -cne 'parent-marker' -or
        ($script:trace -join '|') -cne "temp-created|temp-acl|temp-copy|child|remove:$script:copyDir" -or
        '-RemoveData' -cnotin $script:childArguments -or $dir -cnotin $script:childArguments) {
        throw 'Copied uninstaller did not preserve child status, arguments and parent state'
    }
} finally {
    $env:RUSTWTM_UNINSTALL_COPY = $oldMarker
    $global:LASTEXITCODE = 0
}
Write-Output 'PASS: uninstall verifies ownership, stops exact programs, checks native errors, and supports cleanup retries'
