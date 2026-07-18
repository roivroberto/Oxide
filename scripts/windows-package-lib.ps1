Set-StrictMode -Version Latest

$script:OxideBackendRepository = "https://github.com/fonzy1243/RLox.git"
$script:OxideProtocolVersion = "0.1.0"

function Get-OxideBackendPin {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ManifestPath
    )

    if (-not (Test-Path -LiteralPath $ManifestPath -PathType Leaf)) {
        throw "Standalone Cargo.toml is missing: $ManifestPath"
    }

    $Manifest = Get-Content -LiteralPath $ManifestPath -Raw
    $DependencyTables = [regex]::Matches(
        $Manifest,
        '(?ms)^\[dependencies\][ \t]*\r?\n(?<body>.*?)(?=^\[|\z)'
    )
    $AssignmentPattern = '(?ms)^[ \t]*rlox-protocol[ \t]*=[ \t]*\{(?<body>.*?)\}[ \t]*(?:#[^\r\n]*)?$'
    $AllAssignments = [regex]::Matches($Manifest, $AssignmentPattern)
    $Assignments = @()
    if ($DependencyTables.Count -eq 1) {
        $Assignments = [regex]::Matches(
            $DependencyTables[0].Groups['body'].Value,
            $AssignmentPattern
        )
    }
    $Expected = 'rlox-protocol = { git = "https://github.com/fonzy1243/RLox.git", rev = "<40-hex-commit>", version = "=0.1.0" }'
    if ($Assignments.Count -ne 1 -or $AllAssignments.Count -ne 1) {
        throw "Standalone Cargo.toml must define exactly one pinned dependency: $Expected"
    }

    $Body = $Assignments[0].Groups['body'].Value
    $FieldPattern = '(?<key>[A-Za-z][A-Za-z0-9_-]*)[ \t]*=[ \t]*"(?<value>[^"\r\n]*)"'
    $FieldMatches = [regex]::Matches($Body, $FieldPattern)
    $Remainder = [regex]::Replace($Body, $FieldPattern, '')
    $Remainder = [regex]::Replace($Remainder, '[,\s]', '')
    if ($Remainder.Length -ne 0 -or $FieldMatches.Count -ne 3) {
        throw "Standalone Cargo.toml has an invalid rlox-protocol pin. Expected: $Expected"
    }

    $Fields = @{}
    foreach ($Field in $FieldMatches) {
        $Key = $Field.Groups['key'].Value
        if ($Fields.ContainsKey($Key)) {
            throw "Standalone Cargo.toml repeats '$Key' in the rlox-protocol pin. Expected: $Expected"
        }
        $Fields[$Key] = $Field.Groups['value'].Value
    }

    $RequiredKeys = @('git', 'rev', 'version')
    foreach ($Key in $RequiredKeys) {
        if (-not $Fields.ContainsKey($Key)) {
            throw "Standalone Cargo.toml is missing '$Key' in the rlox-protocol pin. Expected: $Expected"
        }
    }
    foreach ($Key in $Fields.Keys) {
        if ($Key -notin $RequiredKeys) {
            throw "Standalone Cargo.toml contains unsupported '$Key' in the rlox-protocol pin. Expected: $Expected"
        }
    }

    if (-not [string]::Equals(
            $Fields['git'],
            $script:OxideBackendRepository,
            [System.StringComparison]::Ordinal
        )) {
        throw "Standalone Cargo.toml must use the canonical RLox repository. Expected: $Expected"
    }
    if ($Fields['rev'] -notmatch '\A[0-9A-Fa-f]{40}\z') {
        throw "Standalone Cargo.toml must pin rlox-protocol to a full 40-character commit. Expected: $Expected"
    }
    if (-not [string]::Equals(
            $Fields['version'],
            "=$script:OxideProtocolVersion",
            [System.StringComparison]::Ordinal
        )) {
        throw "Standalone Cargo.toml must pin rlox-protocol version =$script:OxideProtocolVersion. Expected: $Expected"
    }

    [pscustomobject]@{
        Repository = $Fields['git']
        Revision = $Fields['rev'].ToLowerInvariant()
        Version = $script:OxideProtocolVersion
    }
}

function Get-OxidePackageVersion {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ManifestPath
    )

    $Manifest = Get-Content -LiteralPath $ManifestPath -Raw
    $PackageMatch = [regex]::Match(
        $Manifest,
        '(?ms)^\[package\][ \t]*\r?\n(?<body>.*?)(?=^\[|\z)'
    )
    if (-not $PackageMatch.Success) {
        throw "Standalone Cargo.toml is missing a [package] table."
    }
    $VersionMatch = [regex]::Match(
        $PackageMatch.Groups['body'].Value,
        '(?m)^[ \t]*version[ \t]*=[ \t]*"(?<version>[^"\r\n]+)"[ \t]*(?:#[^\r\n]*)?$'
    )
    if (-not $VersionMatch.Success) {
        throw "Standalone Cargo.toml is missing package.version."
    }

    $Version = $VersionMatch.Groups['version'].Value
    $SemverPattern = '\A(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?(?:\+[0-9A-Za-z]+(?:[.-][0-9A-Za-z]+)*)?\z'
    if ($Version -notmatch $SemverPattern) {
        throw "package.version is not a supported semantic version: $Version"
    }
    return $Version
}

function Assert-OxideLockProvenance {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$LockPath,
        [Parameter(Mandatory)]
        [psobject]$Pin
    )

    if (-not (Test-Path -LiteralPath $LockPath -PathType Leaf)) {
        throw "Standalone Cargo.lock is missing: $LockPath"
    }

    $Lock = Get-Content -LiteralPath $LockPath -Raw
    $Blocks = [regex]::Matches(
        $Lock,
        '(?ms)^\[\[package\]\][ \t]*\r?\n(?<body>.*?)(?=^\[\[package\]\]|\z)'
    )
    $ProtocolBlocks = @(
        foreach ($Block in $Blocks) {
            $Name = [regex]::Match(
                $Block.Groups['body'].Value,
                '(?m)^[ \t]*name[ \t]*=[ \t]*"(?<value>[^"\r\n]+)"[ \t]*$'
            )
            if ($Name.Success -and $Name.Groups['value'].Value -eq 'rlox-protocol') {
                $Block.Groups['body'].Value
            }
        }
    )
    if ($ProtocolBlocks.Count -ne 1) {
        throw "Cargo.lock must contain exactly one rlox-protocol package from the pinned RLox commit."
    }

    $Version = [regex]::Match(
        $ProtocolBlocks[0],
        '(?m)^[ \t]*version[ \t]*=[ \t]*"(?<value>[^"\r\n]+)"[ \t]*$'
    )
    $Source = [regex]::Match(
        $ProtocolBlocks[0],
        '(?m)^[ \t]*source[ \t]*=[ \t]*"(?<value>[^"\r\n]+)"[ \t]*$'
    )
    $ExpectedSource = "git+$($Pin.Repository)?rev=$($Pin.Revision)#$($Pin.Revision)"
    if (-not $Version.Success -or $Version.Groups['value'].Value -ne $Pin.Version) {
        throw "Cargo.lock rlox-protocol version does not match the exact manifest pin."
    }
    if (-not $Source.Success -or -not [string]::Equals(
            $Source.Groups['value'].Value,
            $ExpectedSource,
            [System.StringComparison]::OrdinalIgnoreCase
        )) {
        throw "Cargo.lock rlox-protocol source must be exactly: $ExpectedSource"
    }
}

function Invoke-OxideGitCapture {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$WorkingDirectory,
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    $Output = @(& git -C $WorkingDirectory @Arguments 2>$null)
    if ($LASTEXITCODE -ne 0) {
        throw "git failed in '$WorkingDirectory': git $($Arguments -join ' ')"
    }
    return ($Output -join "`n").Trim()
}

function Assert-OxideBackendCheckout {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$BackendDirectory,
        [Parameter(Mandatory)]
        [psobject]$Pin,
        [Parameter(Mandatory)]
        [string]$RepositoryRoot
    )

    $CanonicalRoot = [System.IO.Path]::GetFullPath($RepositoryRoot).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $CanonicalBackend = [System.IO.Path]::GetFullPath($BackendDirectory).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $PathComparison = if ($IsWindows) {
        [System.StringComparison]::OrdinalIgnoreCase
    }
    else {
        [System.StringComparison]::Ordinal
    }
    if ([string]::Equals($CanonicalRoot, $CanonicalBackend, $PathComparison)) {
        throw "The RLox backend must be checked out in a separate directory."
    }
    if (-not (Test-Path -LiteralPath $CanonicalBackend -PathType Container)) {
        throw "The RLox backend checkout is missing: $CanonicalBackend"
    }

    $TopLevel = Invoke-OxideGitCapture `
        -WorkingDirectory $CanonicalBackend `
        -Arguments @('rev-parse', '--show-toplevel')
    $CanonicalTopLevel = [System.IO.Path]::GetFullPath($TopLevel).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    if (-not [string]::Equals($CanonicalBackend, $CanonicalTopLevel, $PathComparison)) {
        throw "The backend path is not the root of its own Git checkout: $CanonicalBackend"
    }

    $Head = Invoke-OxideGitCapture `
        -WorkingDirectory $CanonicalBackend `
        -Arguments @('rev-parse', 'HEAD^{commit}')
    if (-not [string]::Equals($Head, $Pin.Revision, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "The RLox checkout is at $Head, but Cargo.toml pins $($Pin.Revision)."
    }

    $Origin = Invoke-OxideGitCapture `
        -WorkingDirectory $CanonicalBackend `
        -Arguments @('remote', 'get-url', 'origin')
    $AllowedOrigins = @($Pin.Repository, $Pin.Repository.Substring(0, $Pin.Repository.Length - 4))
    $OriginMatches = $false
    foreach ($AllowedOrigin in $AllowedOrigins) {
        if ([string]::Equals($Origin, $AllowedOrigin, [System.StringComparison]::OrdinalIgnoreCase)) {
            $OriginMatches = $true
            break
        }
    }
    if (-not $OriginMatches) {
        throw "The RLox checkout origin is not the pinned repository: $Origin"
    }

    $StagedEntries = Invoke-OxideGitCapture `
        -WorkingDirectory $CanonicalBackend `
        -Arguments @('ls-files', '--stage')
    if ($StagedEntries -match '(?m)^160000 ') {
        throw "The RLox backend checkout must not contain submodules."
    }

    $IndexTags = Invoke-OxideGitCapture `
        -WorkingDirectory $CanonicalBackend `
        -Arguments @('ls-files', '-v')
    if ($IndexTags -cmatch '(?m)^[a-zS] ') {
        throw "The RLox backend checkout must not use index visibility flags."
    }

    $Status = Invoke-OxideGitCapture `
        -WorkingDirectory $CanonicalBackend `
        -Arguments @('status', '--porcelain=v1', '--untracked-files=all', '--ignore-submodules=none')
    if (-not [string]::IsNullOrEmpty($Status)) {
        throw "The RLox backend checkout must be clean before validation or packaging."
    }

    $DisallowedIgnored = Invoke-OxideGitCapture `
        -WorkingDirectory $CanonicalBackend `
        -Arguments @(
            'ls-files',
            '--others',
            '--ignored',
            '--exclude-standard',
            '--',
            '.',
            ':(exclude,top)target/**'
        )
    if (-not [string]::IsNullOrEmpty($DisallowedIgnored)) {
        throw "The RLox backend checkout contains ignored files outside the allowed target build output."
    }

    $RequiredBackendFiles = @(
        (Join-Path $CanonicalBackend 'Cargo.toml'),
        (Join-Path $CanonicalBackend 'crates/rlox-protocol/Cargo.toml'),
        (Join-Path $CanonicalBackend 'LICENSE')
    )
    foreach ($Path in $RequiredBackendFiles) {
        if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
            throw "The RLox checkout is missing a required file: $Path"
        }
    }

    $ProtocolManifest = Get-Content -LiteralPath $RequiredBackendFiles[1] -Raw
    $ProtocolPackage = [regex]::Match(
        $ProtocolManifest,
        '(?ms)^\[package\][ \t]*\r?\n(?<body>.*?)(?=^\[|\z)'
    )
    $ProtocolVersion = [regex]::Match(
        $ProtocolPackage.Groups['body'].Value,
        '(?m)^[ \t]*version[ \t]*=[ \t]*"(?<value>[^"\r\n]+)"[ \t]*$'
    )
    if (-not $ProtocolPackage.Success -or -not $ProtocolVersion.Success -or
        $ProtocolVersion.Groups['value'].Value -ne $Pin.Version) {
        throw "The checked-out rlox-protocol package version does not match $($Pin.Version)."
    }
}

function Resolve-OxideDirectChildPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ParentDirectory,
        [Parameter(Mandatory)]
        [string]$CandidatePath
    )

    $CanonicalParent = [System.IO.Path]::GetFullPath($ParentDirectory).TrimEnd(
        [System.IO.Path]::DirectorySeparatorChar,
        [System.IO.Path]::AltDirectorySeparatorChar
    )
    $CanonicalCandidate = [System.IO.Path]::GetFullPath($CandidatePath)
    $CandidateParent = [System.IO.Path]::GetDirectoryName($CanonicalCandidate)
    $PathComparison = if ($IsWindows) {
        [System.StringComparison]::OrdinalIgnoreCase
    }
    else {
        [System.StringComparison]::Ordinal
    }
    if (-not [string]::Equals($CanonicalParent, $CandidateParent, $PathComparison)) {
        throw "Package output must be a direct child of the distribution directory: $CanonicalCandidate"
    }
    return $CanonicalCandidate
}

function Assert-OxideArchiveEntryPath {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$EntryPath
    )

    if ($EntryPath.Length -eq 0 -or
        $EntryPath.StartsWith('/') -or
        $EntryPath.StartsWith('\') -or
        $EntryPath.Contains('\') -or
        $EntryPath.Contains(':')) {
        throw "Unsafe package entry path: $EntryPath"
    }
    $Segments = $EntryPath.Split('/')
    if ($Segments -contains '' -or $Segments -contains '.' -or $Segments -contains '..') {
        throw "Unsafe package entry path: $EntryPath"
    }
}

function Write-OxideDeterministicArchive {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string]$ArchivePath,
        [Parameter(Mandatory)]
        [ValidatePattern('\A[A-Za-z0-9][A-Za-z0-9._+-]{0,127}\z')]
        [string]$RootName,
        [Parameter(Mandatory)]
        [object[]]$Files
    )

    if ($Files.Count -eq 0) {
        throw "At least one file is required to create a package."
    }
    $SeenEntries = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    $SortedFiles = @(
        foreach ($File in $Files) {
            if ($null -eq $File.SourcePath -or $null -eq $File.EntryPath) {
                throw "Every package file needs SourcePath and EntryPath."
            }
            Assert-OxideArchiveEntryPath $File.EntryPath
            if (-not (Test-Path -LiteralPath $File.SourcePath -PathType Leaf)) {
                throw "Required release file is missing: $($File.SourcePath)"
            }
            $FullEntryPath = "$RootName/$($File.EntryPath)"
            if (-not $SeenEntries.Add($FullEntryPath)) {
                throw "Duplicate package entry path: $FullEntryPath"
            }
            [pscustomobject]@{
                SourcePath = [System.IO.Path]::GetFullPath($File.SourcePath)
                EntryPath = $FullEntryPath
            }
        }
    ) | Sort-Object -Property EntryPath -CaseSensitive

    $CanonicalArchive = [System.IO.Path]::GetFullPath($ArchivePath)
    if (Test-Path -LiteralPath $CanonicalArchive) {
        Remove-Item -LiteralPath $CanonicalArchive -Force
    }

    $FileStream = $null
    $Archive = $null
    try {
        $FileStream = [System.IO.File]::Open(
            $CanonicalArchive,
            [System.IO.FileMode]::CreateNew,
            [System.IO.FileAccess]::Write,
            [System.IO.FileShare]::None
        )
        $Archive = [System.IO.Compression.ZipArchive]::new(
            $FileStream,
            [System.IO.Compression.ZipArchiveMode]::Create,
            $false
        )
        $FixedTimestamp = [System.DateTimeOffset]::new(
            1980,
            1,
            1,
            0,
            0,
            0,
            [System.TimeSpan]::Zero
        )
        foreach ($File in $SortedFiles) {
            $Entry = $Archive.CreateEntry(
                $File.EntryPath,
                [System.IO.Compression.CompressionLevel]::NoCompression
            )
            $Entry.LastWriteTime = $FixedTimestamp
            $Entry.ExternalAttributes = 0
            $SourceStream = $null
            $EntryStream = $null
            try {
                $SourceStream = [System.IO.File]::OpenRead($File.SourcePath)
                $EntryStream = $Entry.Open()
                $SourceStream.CopyTo($EntryStream)
            }
            finally {
                if ($null -ne $EntryStream) {
                    $EntryStream.Dispose()
                }
                if ($null -ne $SourceStream) {
                    $SourceStream.Dispose()
                }
            }
        }
    }
    finally {
        if ($null -ne $Archive) {
            $Archive.Dispose()
        }
        elseif ($null -ne $FileStream) {
            $FileStream.Dispose()
        }
    }

    return $CanonicalArchive
}

function Invoke-OxideCargo {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    & cargo @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "cargo failed with exit code $LASTEXITCODE`: cargo $($Arguments -join ' ')"
    }
}
