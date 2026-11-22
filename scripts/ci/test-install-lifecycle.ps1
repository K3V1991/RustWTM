# Load the installer's preflight functions and directory-copy loop.
# Process, service and file operations are doubles.
$ErrorActionPreference = 'Stop'
$installer = Join-Path (Split-Path $PSScriptRoot -Parent) 'install.ps1'
$parseErrors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile($installer, [ref]$null, [ref]$parseErrors)
if ($parseErrors) { throw 'Installer syntax is invalid' }
foreach ($name in 'Resolve-InstallDirectory', 'Get-InstallService', 'Close-TargetGui') {
    $definition = $ast.Find({ param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $true)
    if (-not $definition) { throw "Missing installer function $name" }
    . ([scriptblock]::Create($definition.Extent.Text))
}

function Assert-Rejected([scriptblock]$Action, [string]$ExpectedMessage) {
    try { $null = & $Action }
    catch {
        if ($_.Exception.Message -notlike $ExpectedMessage) { throw }
        return
    }
    throw "Expected refusal: $ExpectedMessage"
}

function Invoke-InstallTreeFixture([scriptblock]$Action, [string[]]$Sources, [string[]]$Existing, [int]$DeleteExit) {
    $source = 'C:\fixture-source'
    $InstallDir = 'C:\fixture-install'
    $trace = [Collections.Generic.List[string]]::new()
    function Test-Path {
        param($LiteralPath)
        if ($LiteralPath -in @($Sources | ForEach-Object { Join-Path $source $_ })) { return $true }
        $LiteralPath -in @($Existing | ForEach-Object { Join-Path $InstallDir $_ })
    }
    function cmd.exe {
        if ($args.Count -ne 5 -or ($args[0..3] -join ' ') -cne '/c rd /s /q' -or
            $args[4] -notin @($Existing | ForEach-Object { Join-Path $InstallDir $_ })) {
            throw 'Unexpected directory deletion request'
        }
        $trace.Add("delete:$(Split-Path $args[4] -Leaf)")
        $global:LASTEXITCODE = $DeleteExit
    }
    function Copy-Item {
        param($LiteralPath, $Destination, [switch]$Recurse, [switch]$Force)
        if ($LiteralPath -notin @($Sources | ForEach-Object { Join-Path $source $_ }) -or
            $Destination -ine $InstallDir -or -not $Recurse -or -not $Force) {
            throw 'Unexpected installation copy request'
        }
        $trace.Add("copy:$(Split-Path $LiteralPath -Leaf)")
    }
    $previousExit = $global:LASTEXITCODE
    $failure = $null
    try { & $Action }
    catch { $failure = $_.Exception.Message }
    finally { $global:LASTEXITCODE = $previousExit }
    [pscustomobject]@{ Trace = $trace -join '|'; Failure = $failure }
}

$copyTree = $ast.Find({
    param($node)
    $node -is [System.Management.Automation.Language.ForEachStatementAst] -and
        $node.Extent.Text.StartsWith("foreach (`$dir in 'hardware', 'docs')")
}, $true)
if (-not $copyTree) { throw 'Missing installation directory copy loop' }
$copyAction = [scriptblock]::Create($copyTree.Extent.Text)
$replaceResult = Invoke-InstallTreeFixture $copyAction @('hardware', 'docs') @('hardware') 5
if ($replaceResult.Trace -cne 'delete:hardware' -or $replaceResult.Failure -notlike '*exit code 5*') {
    throw "Installer copied after deleting the old directory failed: $($replaceResult | ConvertTo-Json -Compress)"
}

$ServiceName = 'RustWTM'
$script:registration = $null
$script:openedServices = 0
$script:processes = @()
$script:closed = [Collections.Generic.List[int]]::new()
function Get-CimInstance { param($ClassName, $Filter)
    if ($ClassName -cne 'Win32_Service' -or $Filter -cne "Name = 'RustWTM'") { throw 'Unexpected service discovery' }
    $script:registration
}
function Get-Service { param($Name)
    if ($Name -cne $ServiceName) { throw 'Wrong service selected' }
    $script:openedServices++
    [pscustomobject]@{ Status = 'Running' }
}
function Get-Process { param($Name, $ErrorAction) $script:processes }
function Stop-Process { param([Parameter(ValueFromPipeline)]$InputObject, [switch]$Force)
    process { $script:closed.Add($InputObject.Id) }
}

$dir = Join-Path $env:ProgramFiles 'RustWTM'
$portableDir = Join-Path $env:LOCALAPPDATA 'RustWTM\portable'
if ((Resolve-InstallDirectory) -ine $dir) { throw 'Unexpected machine install default' }
if ((Resolve-InstallDirectory -Portable) -ine $portableDir) { throw 'Portable install must default to the user profile' }
if ((Resolve-InstallDirectory "$dir\" -Portable) -ine $dir) { throw 'Explicit install directory was ignored' }
Assert-Rejected { Resolve-InstallDirectory ([IO.Path]::GetPathRoot($dir)) } 'Choose a RustWTM directory*'
Assert-Rejected { Resolve-InstallDirectory '\\server\share\' -Portable } 'Choose a RustWTM directory*'

$exe = Join-Path $dir 'wtm-service.exe'
if (Get-InstallService $exe) { throw 'An absent service was selected' }
if (Get-InstallService $exe -WithoutService) { throw 'NoService selected an absent service' }
$script:registration = [pscustomobject]@{ PathName = ('"' + $exe.ToUpperInvariant() + '" service') }
if ((Get-InstallService $exe).Status -ne 'Running') { throw 'Matching service was not found' }
$script:registration.PathName = '"%ProgramFiles%\RustWTM\wtm-service.exe" service'
if ((Get-InstallService $exe).Status -ne 'Running') { throw 'Expanded service path was not recognized' }
$script:registration.PathName = 'C:\RustWTM\wtm-service.exe service'
if ((Get-InstallService 'C:\RustWTM\wtm-service.exe').Status -ne 'Running') { throw 'Unquoted executable path was not recognized' }
Assert-Rejected { Get-InstallService $exe } 'The RustWTM service is registered at a different path*'
Assert-Rejected { Get-InstallService $exe -WithoutService } 'A RustWTM service is already registered*'
$script:registration.PathName = '"C:\missing-end-quote.exe service'
Assert-Rejected { Get-InstallService $exe } 'Could not identify the registered RustWTM service executable*'
if ($script:openedServices -ne 3) { throw 'A refused service reached Get-Service' }

$dir = Join-Path $env:ProgramFiles 'RustWTM [preview]'
$script:processes = @(
    [pscustomobject]@{ Id = 1; Path = "$dir\rustwtm.exe".ToUpperInvariant() },
    [pscustomobject]@{ Id = 2; Path = "$dir-sibling\rustwtm.exe" },
    [pscustomobject]@{ Id = 3; Path = "$dir\nested\rustwtm.exe" },
    [pscustomobject]@{ Id = 4; Path = $null }
)
Close-TargetGui $dir
if ($script:closed.Count -ne 1 -or $script:closed[0] -ne 1) { throw 'Installer closed programs outside the exact target' }
$script:closed.Clear()
Assert-Rejected { Close-TargetGui $dir -Portable } 'Close the RustWTM programs in the portable directory*'
$script:processes += [pscustomobject]@{ Id = 5; Path = "$dir\wtm.exe" }
Assert-Rejected { Close-TargetGui $dir } 'Close the RustWTM CLI in the installation directory*'
$script:processes = @([pscustomobject]@{ Id = 6; Path = "$dir\wtm-service.exe" })
Assert-Rejected { Close-TargetGui $dir -Portable } 'Close the RustWTM programs in the portable directory*'
Close-TargetGui "$dir-sibling" -Portable
if ($script:closed.Count -ne 0) { throw 'Portable extraction or an active CLI closed a program' }
Write-Output 'PASS: installer directory defaults, root rejection, service image ownership, NoService refusal, exact process targets, and portable no-stop behavior'
