[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

. (Join-Path $PSScriptRoot "windows-package-lib.ps1")

$script:Assertions = 0

function Assert-True {
    param(
        [Parameter(Mandatory)]
        [bool]$Condition,
        [Parameter(Mandatory)]
        [string]$Message
    )

    $script:Assertions += 1
    if (-not $Condition) {
        throw "Assertion failed: $Message"
    }
}

function Assert-Equal {
    param(
        $Expected,
        $Actual,
        [Parameter(Mandatory)]
        [string]$Message
    )

    $script:Assertions += 1
    if ($Expected -ne $Actual) {
        throw "Assertion failed: $Message. Expected '$Expected', got '$Actual'."
    }
}

function Assert-ThrowsLike {
    param(
        [Parameter(Mandatory)]
        [scriptblock]$Action,
        [Parameter(Mandatory)]
        [string]$Pattern,
        [Parameter(Mandatory)]
        [string]$Message
    )

    $script:Assertions += 1
    try {
        & $Action
    }
    catch {
        if ($_.Exception.Message -like $Pattern) {
            return
        }
        throw "Assertion failed: $Message. Unexpected error: $($_.Exception.Message)"
    }
    throw "Assertion failed: $Message. Expected an exception."
}

function Invoke-TestGit {
    param(
        [Parameter(Mandatory)]
        [string]$WorkingDirectory,
        [Parameter(Mandatory)]
        [string[]]$Arguments
    )

    & git -C $WorkingDirectory @Arguments | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Test setup git command failed: git $($Arguments -join ' ')"
    }
}

$TestRoot = Join-Path ([System.IO.Path]::GetTempPath()) (
    "oxide-package-tests-" + [System.Guid]::NewGuid().ToString('N')
)
New-Item -ItemType Directory -Path $TestRoot | Out-Null

try {
    $Revision = '0123456789abcdef0123456789abcdef01234567'
    $ManifestPath = Join-Path $TestRoot 'Cargo.toml'
    $LockPath = Join-Path $TestRoot 'Cargo.lock'
    $PinnedManifest = @'
[package]
name = "oxide-ide"
version = "1.2.3-beta.1"

[dependencies]
rlox-protocol = { version = "=0.1.0", rev = "__REV__", git = "https://github.com/fonzy1243/RLox.git" }
'@.Replace('__REV__', $Revision)
    Set-Content -LiteralPath $ManifestPath -Value $PinnedManifest -Encoding utf8NoBOM -NoNewline

    $Pin = Get-OxideBackendPin -ManifestPath $ManifestPath
    Assert-Equal $Revision $Pin.Revision 'the full backend revision is parsed exactly'
    Assert-Equal 'https://github.com/fonzy1243/RLox.git' $Pin.Repository 'the canonical repository is required'
    Assert-Equal '0.1.0' $Pin.Version 'the protocol version is exact'
    Assert-Equal '1.2.3-beta.1' (Get-OxidePackageVersion -ManifestPath $ManifestPath) 'the package version is read from [package]'

    $PathManifest = $PinnedManifest.Replace(
        'rlox-protocol = { version = "=0.1.0", rev = "' + $Revision + '", git = "https://github.com/fonzy1243/RLox.git" }',
        'rlox-protocol = { path = "../rlox-protocol" }'
    )
    Set-Content -LiteralPath $ManifestPath -Value $PathManifest -Encoding utf8NoBOM -NoNewline
    Assert-ThrowsLike {
        Get-OxideBackendPin -ManifestPath $ManifestPath
    } '*Expected: rlox-protocol*' 'a path dependency is rejected with the required pin shape'

    $ShortManifest = $PinnedManifest.Replace($Revision, '01234567')
    Set-Content -LiteralPath $ManifestPath -Value $ShortManifest -Encoding utf8NoBOM -NoNewline
    Assert-ThrowsLike {
        Get-OxideBackendPin -ManifestPath $ManifestPath
    } '*full 40-character commit*' 'a short revision is rejected'

    $DevOnlyManifest = $PinnedManifest.Replace('[dependencies]', '[dev-dependencies]')
    Set-Content -LiteralPath $ManifestPath -Value $DevOnlyManifest -Encoding utf8NoBOM -NoNewline
    Assert-ThrowsLike {
        Get-OxideBackendPin -ManifestPath $ManifestPath
    } '*exactly one pinned dependency*' 'a pin outside the direct dependency table is rejected'

    $ExtraFieldManifest = $PinnedManifest.Replace(
        'git = "https://github.com/fonzy1243/RLox.git"',
        'git = "https://github.com/fonzy1243/RLox.git", path = "../rlox-protocol"'
    )
    Set-Content -LiteralPath $ManifestPath -Value $ExtraFieldManifest -Encoding utf8NoBOM -NoNewline
    Assert-ThrowsLike {
        Get-OxideBackendPin -ManifestPath $ManifestPath
    } '*invalid rlox-protocol pin*' 'mixed Git and path provenance is rejected'

    Set-Content -LiteralPath $ManifestPath -Value $PinnedManifest -Encoding utf8NoBOM -NoNewline
    $Lock = @'
version = 4

[[package]]
name = "oxide-ide"
version = "1.2.3-beta.1"

[[package]]
name = "rlox-protocol"
version = "0.1.0"
source = "git+https://github.com/fonzy1243/RLox.git?rev=__REV__#__REV__"
'@.Replace('__REV__', $Revision)
    Set-Content -LiteralPath $LockPath -Value $Lock -Encoding utf8NoBOM -NoNewline
    Assert-OxideLockProvenance -LockPath $LockPath -Pin $Pin
    Assert-True $true 'an exact Cargo.lock source is accepted'

    $WrongLock = $Lock.Replace("#$Revision", '#ffffffffffffffffffffffffffffffffffffffff')
    Set-Content -LiteralPath $LockPath -Value $WrongLock -Encoding utf8NoBOM -NoNewline
    Assert-ThrowsLike {
        Assert-OxideLockProvenance -LockPath $LockPath -Pin $Pin
    } '*source must be exactly*' 'a mismatched lockfile commit is rejected'

    $FilesDirectory = Join-Path $TestRoot 'files'
    New-Item -ItemType Directory -Path $FilesDirectory | Out-Null
    $FirstFile = Join-Path $FilesDirectory 'first.bin'
    $SecondFile = Join-Path $FilesDirectory 'second.bin'
    Set-Content -LiteralPath $FirstFile -Value 'first' -Encoding ascii -NoNewline
    Set-Content -LiteralPath $SecondFile -Value 'second' -Encoding ascii -NoNewline
    $ArchiveFiles = @(
        [pscustomobject]@{ SourcePath = $SecondFile; EntryPath = 'z/second.bin' },
        [pscustomobject]@{ SourcePath = $FirstFile; EntryPath = 'a/first.bin' }
    )
    $FirstArchive = Join-Path $TestRoot 'first.zip'
    $SecondArchive = Join-Path $TestRoot 'second.zip'
    Write-OxideDeterministicArchive -ArchivePath $FirstArchive -RootName 'package' -Files $ArchiveFiles | Out-Null
    (Get-Item -LiteralPath $FirstFile).LastWriteTimeUtc = [datetime]::UtcNow.AddYears(-4)
    (Get-Item -LiteralPath $SecondFile).LastWriteTimeUtc = [datetime]::UtcNow
    Write-OxideDeterministicArchive -ArchivePath $SecondArchive -RootName 'package' -Files $ArchiveFiles | Out-Null
    $FirstHash = (Get-FileHash -LiteralPath $FirstArchive -Algorithm SHA256).Hash
    $SecondHash = (Get-FileHash -LiteralPath $SecondArchive -Algorithm SHA256).Hash
    Assert-Equal $FirstHash $SecondHash 'archive bytes do not depend on input order or source timestamps'

    $Archive = [System.IO.Compression.ZipFile]::OpenRead($FirstArchive)
    try {
        $EntryNames = @($Archive.Entries | ForEach-Object { $_.FullName })
        Assert-Equal 'package/a/first.bin|package/z/second.bin' ($EntryNames -join '|') 'archive entries are ordinally sorted'
        foreach ($Entry in $Archive.Entries) {
            Assert-Equal 1980 $Entry.LastWriteTime.Year 'archive timestamps use the fixed ZIP epoch'
        }
    }
    finally {
        $Archive.Dispose()
    }

    Assert-ThrowsLike {
        Assert-OxideArchiveEntryPath '../escape.exe'
    } '*Unsafe package entry path*' 'parent traversal is rejected'
    Assert-ThrowsLike {
        Resolve-OxideDirectChildPath -ParentDirectory $TestRoot -CandidatePath (Join-Path $TestRoot '../escape.zip')
    } '*direct child*' 'package output cannot escape dist'

    $BackendDirectory = Join-Path $TestRoot 'backend'
    New-Item -ItemType Directory -Path (Join-Path $BackendDirectory 'crates/rlox-protocol') -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $BackendDirectory 'Cargo.toml') -Value @'
[package]
name = "rlox"
version = "0.1.0"
'@ -Encoding utf8NoBOM -NoNewline
    Set-Content -LiteralPath (Join-Path $BackendDirectory 'crates/rlox-protocol/Cargo.toml') -Value @'
[package]
name = "rlox-protocol"
version = "0.1.0"
'@ -Encoding utf8NoBOM -NoNewline
    Set-Content -LiteralPath (Join-Path $BackendDirectory 'LICENSE') -Value 'license' -Encoding ascii -NoNewline
    Set-Content `
        -LiteralPath (Join-Path $BackendDirectory '.gitignore') `
        -Value '/target' `
        -Encoding ascii `
        -NoNewline
    Invoke-TestGit -WorkingDirectory $BackendDirectory -Arguments @('init', '--quiet')
    Invoke-TestGit -WorkingDirectory $BackendDirectory -Arguments @('config', 'user.name', 'Package Tests')
    Invoke-TestGit -WorkingDirectory $BackendDirectory -Arguments @('config', 'user.email', 'package-tests@example.invalid')
    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('add', '.gitignore', 'Cargo.toml', 'crates/rlox-protocol/Cargo.toml', 'LICENSE')
    Invoke-TestGit -WorkingDirectory $BackendDirectory -Arguments @('commit', '--quiet', '-m', 'fixture')
    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('remote', 'add', 'origin', 'https://github.com/fonzy1243/RLox.git')
    $BackendRevision = Invoke-OxideGitCapture `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('rev-parse', 'HEAD')
    $BackendPin = [pscustomobject]@{
        Repository = 'https://github.com/fonzy1243/RLox.git'
        Revision = $BackendRevision
        Version = '0.1.0'
    }
    Assert-OxideBackendCheckout `
        -BackendDirectory $BackendDirectory `
        -Pin $BackendPin `
        -RepositoryRoot $TestRoot
    Assert-True $true 'an exact standalone backend checkout is accepted'

    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('update-index', '--assume-unchanged', 'Cargo.toml')
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $BackendDirectory `
            -Pin $BackendPin `
            -RepositoryRoot $TestRoot
    } '*index visibility flags*' 'an assume-unchanged backend entry is rejected'
    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('update-index', '--no-assume-unchanged', 'Cargo.toml')

    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('update-index', '--skip-worktree', 'LICENSE')
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $BackendDirectory `
            -Pin $BackendPin `
            -RepositoryRoot $TestRoot
    } '*index visibility flags*' 'a skip-worktree backend entry is rejected'
    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('update-index', '--no-skip-worktree', 'LICENSE')

    Add-Content `
        -LiteralPath (Join-Path $BackendDirectory 'Cargo.toml') `
        -Value "`n# modified" `
        -Encoding utf8NoBOM
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $BackendDirectory `
            -Pin $BackendPin `
            -RepositoryRoot $TestRoot
    } '*must be clean*' 'a modified tracked backend file is rejected'
    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('restore', '--worktree', '--', 'Cargo.toml')

    Add-Content `
        -LiteralPath (Join-Path $BackendDirectory 'LICENSE') `
        -Value "`nmodified" `
        -Encoding utf8NoBOM
    Invoke-TestGit -WorkingDirectory $BackendDirectory -Arguments @('add', 'LICENSE')
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $BackendDirectory `
            -Pin $BackendPin `
            -RepositoryRoot $TestRoot
    } '*must be clean*' 'a staged backend change is rejected'
    Invoke-TestGit `
        -WorkingDirectory $BackendDirectory `
        -Arguments @('restore', '--staged', '--worktree', '--', 'LICENSE')

    $UntrackedBackendFile = Join-Path $BackendDirectory 'untracked.txt'
    Set-Content `
        -LiteralPath $UntrackedBackendFile `
        -Value 'untracked' `
        -Encoding ascii `
        -NoNewline
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $BackendDirectory `
            -Pin $BackendPin `
            -RepositoryRoot $TestRoot
    } '*must be clean*' 'an untracked backend file is rejected'
    Remove-Item -LiteralPath $UntrackedBackendFile -Force

    $InfoExcludePath = Join-Path $BackendDirectory '.git/info/exclude'
    $OriginalInfoExclude = Get-Content -LiteralPath $InfoExcludePath -Raw
    Add-Content `
        -LiteralPath $InfoExcludePath `
        -Value "`n/.cargo/config.toml" `
        -Encoding utf8NoBOM
    $IgnoredCargoDirectory = Join-Path $BackendDirectory '.cargo'
    $IgnoredCargoConfig = Join-Path $IgnoredCargoDirectory 'config.toml'
    New-Item -ItemType Directory -Path $IgnoredCargoDirectory | Out-Null
    Set-Content `
        -LiteralPath $IgnoredCargoConfig `
        -Value '[build]' `
        -Encoding utf8NoBOM `
        -NoNewline
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $BackendDirectory `
            -Pin $BackendPin `
            -RepositoryRoot $TestRoot
    } '*ignored files outside*' 'an ignored backend Cargo configuration is rejected'
    Remove-Item -LiteralPath $IgnoredCargoDirectory -Recurse -Force
    Set-Content `
        -LiteralPath $InfoExcludePath `
        -Value $OriginalInfoExclude `
        -Encoding utf8NoBOM `
        -NoNewline

    $IgnoredTargetDirectory = Join-Path $BackendDirectory 'target/cleanliness-fixture'
    $IgnoredTargetFile = Join-Path $IgnoredTargetDirectory 'artifact.bin'
    New-Item -ItemType Directory -Path $IgnoredTargetDirectory -Force | Out-Null
    Set-Content `
        -LiteralPath $IgnoredTargetFile `
        -Value 'build output' `
        -Encoding ascii `
        -NoNewline
    Assert-OxideBackendCheckout `
        -BackendDirectory $BackendDirectory `
        -Pin $BackendPin `
        -RepositoryRoot $TestRoot
    Assert-True $true 'an ignored artifact under the root target directory is accepted'
    Remove-Item -LiteralPath (Join-Path $BackendDirectory 'target') -Recurse -Force

    $SubmoduleSource = Join-Path $TestRoot 'submodule-source'
    New-Item -ItemType Directory -Path $SubmoduleSource | Out-Null
    Invoke-TestGit -WorkingDirectory $SubmoduleSource -Arguments @('init', '--quiet')
    Invoke-TestGit -WorkingDirectory $SubmoduleSource -Arguments @('config', 'user.name', 'Package Tests')
    Invoke-TestGit -WorkingDirectory $SubmoduleSource -Arguments @('config', 'user.email', 'package-tests@example.invalid')
    Set-Content `
        -LiteralPath (Join-Path $SubmoduleSource '.gitignore') `
        -Value '/ignored-build-input' `
        -Encoding ascii `
        -NoNewline
    Set-Content `
        -LiteralPath (Join-Path $SubmoduleSource 'tracked.txt') `
        -Value 'tracked' `
        -Encoding ascii `
        -NoNewline
    Invoke-TestGit -WorkingDirectory $SubmoduleSource -Arguments @('add', '.gitignore', 'tracked.txt')
    Invoke-TestGit -WorkingDirectory $SubmoduleSource -Arguments @('commit', '--quiet', '-m', 'fixture')

    $SubmoduleBackend = Join-Path $TestRoot 'submodule-backend'
    New-Item -ItemType Directory -Path (Join-Path $SubmoduleBackend 'crates/rlox-protocol') -Force | Out-Null
    Copy-Item -LiteralPath (Join-Path $BackendDirectory '.gitignore') -Destination $SubmoduleBackend
    Copy-Item -LiteralPath (Join-Path $BackendDirectory 'Cargo.toml') -Destination $SubmoduleBackend
    Copy-Item -LiteralPath (Join-Path $BackendDirectory 'LICENSE') -Destination $SubmoduleBackend
    Copy-Item `
        -LiteralPath (Join-Path $BackendDirectory 'crates/rlox-protocol/Cargo.toml') `
        -Destination (Join-Path $SubmoduleBackend 'crates/rlox-protocol')
    Invoke-TestGit -WorkingDirectory $SubmoduleBackend -Arguments @('init', '--quiet')
    Invoke-TestGit -WorkingDirectory $SubmoduleBackend -Arguments @('config', 'user.name', 'Package Tests')
    Invoke-TestGit -WorkingDirectory $SubmoduleBackend -Arguments @('config', 'user.email', 'package-tests@example.invalid')
    Invoke-TestGit `
        -WorkingDirectory $SubmoduleBackend `
        -Arguments @(
            '-c',
            'protocol.file.allow=always',
            'submodule',
            'add',
            '--quiet',
            $SubmoduleSource,
            'vendor/fixture'
        )
    Invoke-TestGit -WorkingDirectory $SubmoduleBackend -Arguments @('add', '.')
    Invoke-TestGit -WorkingDirectory $SubmoduleBackend -Arguments @('commit', '--quiet', '-m', 'fixture')
    Invoke-TestGit `
        -WorkingDirectory $SubmoduleBackend `
        -Arguments @('remote', 'add', 'origin', 'https://github.com/fonzy1243/RLox.git')
    $SubmoduleBackendRevision = Invoke-OxideGitCapture `
        -WorkingDirectory $SubmoduleBackend `
        -Arguments @('rev-parse', 'HEAD')
    $SubmoduleBackendPin = [pscustomobject]@{
        Repository = 'https://github.com/fonzy1243/RLox.git'
        Revision = $SubmoduleBackendRevision
        Version = '0.1.0'
    }
    Set-Content `
        -LiteralPath (Join-Path $SubmoduleBackend 'vendor/fixture/ignored-build-input') `
        -Value 'ignored' `
        -Encoding ascii `
        -NoNewline
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $SubmoduleBackend `
            -Pin $SubmoduleBackendPin `
            -RepositoryRoot $TestRoot
    } '*must not contain submodules*' 'a backend submodule with an ignored input is rejected'

    $StandaloneRoot = Join-Path $TestRoot 'standalone'
    $StandaloneScripts = Join-Path $StandaloneRoot 'scripts'
    New-Item -ItemType Directory -Path $StandaloneScripts -Force | Out-Null
    Copy-Item `
        -LiteralPath (Join-Path $PSScriptRoot 'package-windows.ps1') `
        -Destination $StandaloneScripts
    Copy-Item `
        -LiteralPath (Join-Path $PSScriptRoot 'windows-package-lib.ps1') `
        -Destination $StandaloneScripts
    $StandaloneManifest = $PinnedManifest.Replace($Revision, $BackendRevision)
    $StandaloneLock = $Lock.Replace($Revision, $BackendRevision)
    Set-Content `
        -LiteralPath (Join-Path $StandaloneRoot 'Cargo.toml') `
        -Value $StandaloneManifest `
        -Encoding utf8NoBOM `
        -NoNewline
    Set-Content `
        -LiteralPath (Join-Path $StandaloneRoot 'Cargo.lock') `
        -Value $StandaloneLock `
        -Encoding utf8NoBOM `
        -NoNewline
    Set-Content `
        -LiteralPath (Join-Path $StandaloneRoot 'LICENSE') `
        -Value 'oxide license' `
        -Encoding ascii `
        -NoNewline
    $OxideRelease = Join-Path $StandaloneRoot 'target/x86_64-pc-windows-msvc/release'
    $BackendRelease = Join-Path $BackendDirectory 'target/x86_64-pc-windows-msvc/release'
    New-Item -ItemType Directory -Path $OxideRelease -Force | Out-Null
    New-Item -ItemType Directory -Path $BackendRelease -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $OxideRelease 'oxide-ide.exe') -Value 'oxide' -Encoding ascii -NoNewline
    Set-Content -LiteralPath (Join-Path $BackendRelease 'rlox.exe') -Value 'rlox' -Encoding ascii -NoNewline
    Set-Content -LiteralPath (Join-Path $BackendRelease 'rlox-lsp.exe') -Value 'lsp' -Encoding ascii -NoNewline

    $PackageScript = Join-Path $StandaloneScripts 'package-windows.ps1'
    $ResolvedRevision = @(& $PackageScript -PrintBackendRevision)
    Assert-Equal 1 $ResolvedRevision.Count 'revision mode emits exactly one value'
    Assert-Equal $BackendRevision $ResolvedRevision[0] 'revision mode emits the manifest commit'
    $ValidationOutput = @(
        & $PackageScript -ValidateOnly -BackendDirectory $BackendDirectory
    )
    Assert-Equal 1 $ValidationOutput.Count 'validation mode emits one success record'
    Assert-True `
        $ValidationOutput[0].Contains($BackendRevision) `
        'validation confirms the exact backend revision'
    $PackageOutput = @(
        & $PackageScript -SkipBuild -BackendDirectory $BackendDirectory
    )
    Assert-Equal 2 $PackageOutput.Count 'packaging returns the archive and checksum paths'
    foreach ($OutputPath in $PackageOutput) {
        Assert-True (Test-Path -LiteralPath $OutputPath -PathType Leaf) 'every reported package output exists'
    }
    $PackageArchive = $PackageOutput[0]
    $PackageChecksum = $PackageOutput[1]
    $ExpectedHash = (Get-FileHash -LiteralPath $PackageArchive -Algorithm SHA256).Hash.ToLowerInvariant()
    $ExpectedChecksum = "$ExpectedHash  $([System.IO.Path]::GetFileName($PackageArchive))"
    Assert-Equal `
        $ExpectedChecksum `
        (Get-Content -LiteralPath $PackageChecksum -Raw) `
        'the SHA256 sidecar names and authenticates the archive'
    $PackageZip = [System.IO.Compression.ZipFile]::OpenRead($PackageArchive)
    try {
        $PackageEntries = @($PackageZip.Entries | ForEach-Object { $_.FullName })
        $ExpectedEntries = @(
            'oxide-ide-v1.2.3-beta.1-windows-x86_64/licenses/Oxide-LICENSE',
            'oxide-ide-v1.2.3-beta.1-windows-x86_64/licenses/RLox-LICENSE',
            'oxide-ide-v1.2.3-beta.1-windows-x86_64/oxide-ide.exe',
            'oxide-ide-v1.2.3-beta.1-windows-x86_64/rlox-lsp.exe',
            'oxide-ide-v1.2.3-beta.1-windows-x86_64/rlox.exe'
        )
        Assert-Equal `
            ($ExpectedEntries -join '|') `
            ($PackageEntries -join '|') `
            'the release contains only the three executables and both licenses'
    }
    finally {
        $PackageZip.Dispose()
    }

    $FirstPackageHash = (Get-FileHash -LiteralPath $PackageArchive -Algorithm SHA256).Hash
    & pwsh `
        -NoLogo `
        -NoProfile `
        -File $PackageScript `
        -SkipBuild `
        -BackendDirectory $BackendDirectory | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "The isolated packaging process failed with exit code $LASTEXITCODE."
    }
    $SecondPackageHash = (Get-FileHash -LiteralPath $PackageArchive -Algorithm SHA256).Hash
    Assert-Equal $FirstPackageHash $SecondPackageHash 'separate processes produce byte-for-byte identical packages'

    $WrongPin = [pscustomobject]@{
        Repository = $BackendPin.Repository
        Revision = 'ffffffffffffffffffffffffffffffffffffffff'
        Version = $BackendPin.Version
    }
    Assert-ThrowsLike {
        Assert-OxideBackendCheckout `
            -BackendDirectory $BackendDirectory `
            -Pin $WrongPin `
            -RepositoryRoot $TestRoot
    } '*but Cargo.toml pins*' 'a checkout at the wrong commit is rejected'
}
finally {
    if (Test-Path -LiteralPath $TestRoot) {
        Remove-Item -LiteralPath $TestRoot -Recurse -Force
    }
}

Write-Output "Package helper tests passed ($script:Assertions assertions)."
