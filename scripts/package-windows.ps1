[CmdletBinding()]
param(
    [switch]$PrintBackendRevision,
    [switch]$ValidateOnly,
    [switch]$SkipBuild,
    [string]$BackendDirectory,
    [ValidateSet("x86_64-pc-windows-msvc")]
    [string]$Target = "x86_64-pc-windows-msvc",
    [string]$Version
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-package-lib.ps1")

if (($PrintBackendRevision -and $ValidateOnly) -or
    ($PrintBackendRevision -and $SkipBuild) -or
    ($ValidateOnly -and $SkipBuild)) {
    throw "PrintBackendRevision, ValidateOnly, and SkipBuild cannot be combined."
}

$RepositoryRoot = [System.IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$ManifestPath = Join-Path $RepositoryRoot "Cargo.toml"
$LockPath = Join-Path $RepositoryRoot "Cargo.lock"
$Pin = Get-OxideBackendPin -ManifestPath $ManifestPath

if ($PrintBackendRevision) {
    Write-Output $Pin.Revision
    return
}

if ([string]::IsNullOrWhiteSpace($BackendDirectory)) {
    $BackendDirectory = Join-Path $RepositoryRoot "_backend/rlox"
}
$BackendDirectory = [System.IO.Path]::GetFullPath($BackendDirectory)

Assert-OxideLockProvenance -LockPath $LockPath -Pin $Pin
Assert-OxideBackendCheckout `
    -BackendDirectory $BackendDirectory `
    -Pin $Pin `
    -RepositoryRoot $RepositoryRoot

$OxideLicense = Join-Path $RepositoryRoot "LICENSE"
if (-not (Test-Path -LiteralPath $OxideLicense -PathType Leaf)) {
    throw "Oxide LICENSE is missing: $OxideLicense"
}

if ($ValidateOnly) {
    Write-Output "Validated RLox revision $($Pin.Revision) and Cargo.lock provenance."
    return
}

$ManifestVersion = Get-OxidePackageVersion -ManifestPath $ManifestPath
if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = $ManifestVersion
}
elseif (-not [string]::Equals(
        $Version,
        $ManifestVersion,
        [System.StringComparison]::Ordinal
    )) {
    throw "Requested package version $Version does not match Cargo.toml version $ManifestVersion."
}
$SemverPattern = '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?(?:\+[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?\z'
if ($Version -notmatch $SemverPattern) {
    throw "Version is not a supported semantic version: $Version"
}

if (-not $SkipBuild) {
    Invoke-OxideCargo -Arguments @(
        'build',
        '--locked',
        '--release',
        '--manifest-path',
        $ManifestPath,
        '--target',
        $Target
    )
    Invoke-OxideCargo -Arguments @(
        'build',
        '--locked',
        '--release',
        '--manifest-path',
        (Join-Path $BackendDirectory 'Cargo.toml'),
        '--target',
        $Target,
        '-p',
        'rlox',
        '-p',
        'rlox-lsp'
    )
}

$OxideReleaseDirectory = Join-Path $RepositoryRoot "target/$Target/release"
$BackendReleaseDirectory = Join-Path $BackendDirectory "target/$Target/release"
$PackageName = "oxide-ide-v$Version-windows-x86_64"
$DistributionDirectory = [System.IO.Path]::GetFullPath((Join-Path $RepositoryRoot "dist"))
New-Item -ItemType Directory -Path $DistributionDirectory -Force | Out-Null

$ArchivePath = Resolve-OxideDirectChildPath `
    -ParentDirectory $DistributionDirectory `
    -CandidatePath (Join-Path $DistributionDirectory "$PackageName.zip")
$ChecksumPath = Resolve-OxideDirectChildPath `
    -ParentDirectory $DistributionDirectory `
    -CandidatePath "$ArchivePath.sha256"

$PackageFiles = @(
    [pscustomobject]@{
        SourcePath = Join-Path $OxideReleaseDirectory "oxide-ide.exe"
        EntryPath = "oxide-ide.exe"
    },
    [pscustomobject]@{
        SourcePath = Join-Path $BackendReleaseDirectory "rlox.exe"
        EntryPath = "rlox.exe"
    },
    [pscustomobject]@{
        SourcePath = Join-Path $BackendReleaseDirectory "rlox-lsp.exe"
        EntryPath = "rlox-lsp.exe"
    },
    [pscustomobject]@{
        SourcePath = $OxideLicense
        EntryPath = "licenses/Oxide-LICENSE"
    },
    [pscustomobject]@{
        SourcePath = Join-Path $BackendDirectory "LICENSE"
        EntryPath = "licenses/RLox-LICENSE"
    }
)

$ArchivePath = Write-OxideDeterministicArchive `
    -ArchivePath $ArchivePath `
    -RootName $PackageName `
    -Files $PackageFiles
$Hash = (Get-FileHash -LiteralPath $ArchivePath -Algorithm SHA256).Hash.ToLowerInvariant()
"$Hash  $([System.IO.Path]::GetFileName($ArchivePath))" |
    Set-Content -LiteralPath $ChecksumPath -Encoding ascii -NoNewline

Write-Output $ArchivePath
Write-Output $ChecksumPath
