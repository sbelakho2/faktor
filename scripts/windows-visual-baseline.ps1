# Faktor Windows visual-baseline gate (self-hosted Windows certificate lane).
#
# The JetBrains visual parity baseline
# (`apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json`,
# schema `faktor-parity-visual-baselines/v3`) must carry a DISTINCT record per
# release platform. This step runs on the Windows agent and:
#
#   1. FAILS when the `windows` record is missing or malformed -- a platform
#      is never certified by inheriting linux/macos digests (the v2 canonical
#      pool is refused, and an identical windows<->linux/macos digest for the
#      same panel is an explicit inheritance failure). The windows record must
#      ALSO carry its own distinct `windows-...` environment fingerprint with
#      a resolved-font token: a re-pin made on a Windows agent records
#      `windows-amd64-jvm17-f<hex>` (or the host's equivalent) and the policy
#      refuses a record that reuses another platform's fingerprint or has
#      none;
#   2. renders this platform's panels and compares them against the pinned
#      windows record by running `bash apps/jetbrains/compile-and-smoke.sh`
#      (the JetBrains parity matrix compares per-platform records and fails on
#      drift), so a stale windows baseline fails on the agent that owns it;
#   3. in `-WriteBaselines` mode first re-pins the windows record with
#      `bash apps/jetbrains/compile-and-smoke.sh --write-baselines`, copies the
#      produced file to `target/certification/visual-baselines-windows.json`
#      for retrieval, and then re-runs the comparison against it.
#
# On success the lane writes its own
# `target/certification/ci-certification-windows-visual.json` record
# (`faktor-windows-visual-baseline/v1`) and lists it as a REQUIRED artifact of
# the `windows-visual-baseline` lane marker, which `certificate-windows`
# verifies (and `scripts/certification/evidence.mjs` models as the canonical
# `windows` workflow: no Windows certificate without this lane + artifact).
#
# Fail-closed: no Git Bash / JetBrains toolchain on the agent, a missing or
# inherited baseline, or a render drift all fail the lane. Nothing is ever
# skipped silently.
#
# Usage (operator, on the Windows agent, from the repo root):
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows-visual-baseline.ps1 -WriteBaselines
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows-visual-baseline.ps1
#   powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows-visual-baseline.ps1 -SelfTest
#
# The compare mode is what `certificate-windows` depends on. `-SelfTest`
# exercises the baseline policy function offline (no toolchain, no Windows
# required) and is the fast correctness proof of the fail-closed checks.

[CmdletBinding()]
param(
    [string]$RepoRoot = (Get-Location).Path,
    [string]$BaselinePath = "apps/jetbrains/frontend/src/test/resources/parity/visual-baselines.json",
    [string]$OutDir = "target/certification",
    [string]$LaneMarker = "target/certification/lanes/windows-visual-baseline.json",
    [switch]$WriteBaselines,
    [switch]$SkipRender,
    [switch]$SelfTest
)

$ErrorActionPreference = "Stop"

$BASELINE_SCHEMA = "faktor-parity-visual-baselines/v3"
$REQUIRED_PLATFORMS = @("linux", "macos", "windows")
$EMPTY_ARTIFACT_DIGEST = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"

function Test-Digest {
    param([string]$Value)
    return ($Value -match '^[0-9a-f]{64}$')
}

# True when an environment string carries a resolved-font fingerprint token
# (`-f` followed by at least 8 hex chars), the same shape the Kotlin
# `visualEnvironment()` writes. A windows record without one is a pre-pin
# placeholder, never a certified environment.
function Test-FontFingerprint {
    param([string]$Value)
    $marker = $Value.LastIndexOf("-f")
    if ($marker -lt 0) { return $false }
    $token = $Value.Substring($marker + 2)
    return ($token.Length -ge 8 -and $token -match '^[0-9a-f]+$')
}

# Pure baseline policy function: returns an object with ok/reasons/coverage
# and the windows digest map. No side effects, so -SelfTest can exercise it.
function Test-VisualBaselineText {
    param([string]$Text)

    $reasons = New-Object System.Collections.ArrayList
    $coverage = [ordered]@{ linux = "not_certified"; macos = "not_certified"; windows = "not_certified" }
    $windowsDigests = $null
    $environments = @{}
    $windowsEnvironment = ""

    $root = $null
    try {
        $root = $Text | ConvertFrom-Json
    }
    catch {
        [void]$reasons.Add("malformed-json")
        return [pscustomobject]@{ ok = $false; reasons = @($reasons); coverage = $coverage; windows_digests = $null }
    }
    if ($null -eq $root -or $null -eq $root.schema) {
        [void]$reasons.Add("malformed-json")
        return [pscustomobject]@{ ok = $false; reasons = @($reasons); coverage = $coverage; windows_digests = $null }
    }
    if ([string]$root.schema -ne $BASELINE_SCHEMA) {
        [void]$reasons.Add("wrong-schema:$($root.schema)")
        return [pscustomobject]@{ ok = $false; reasons = @($reasons); coverage = $coverage; windows_digests = $null }
    }
    $required = @($root.requiredPlatforms)
    foreach ($platform in $REQUIRED_PLATFORMS) {
        if ($required -notcontains $platform) {
            [void]$reasons.Add("required-platform-missing:$platform")
        }
    }

    $platforms = @{}
    if ($null -ne $root.platforms) {
        foreach ($property in $root.platforms.PSObject.Properties) {
            if ($REQUIRED_PLATFORMS -notcontains $property.Name) {
                [void]$reasons.Add("unknown-platform:$($property.Name)")
                continue
            }
            $record = $property.Value
            $environment = [string]$record.environment
            if ([string]::IsNullOrWhiteSpace($environment)) {
                [void]$reasons.Add("missing-environment:$($property.Name)")
            }
            else {
                $environments[$property.Name] = $environment
                if ($property.Name -eq "windows") { $windowsEnvironment = $environment }
            }
            $digests = @{}
            if ($null -ne $record.digests) {
                foreach ($digestProperty in $record.digests.PSObject.Properties) {
                    $value = [string]$digestProperty.Value
                    if (-not (Test-Digest $value)) {
                        [void]$reasons.Add("bad-digest:$($property.Name)/$($digestProperty.Name)")
                    }
                    $digests[$digestProperty.Name] = $value
                }
            }
            if ($digests.Count -eq 0) {
                [void]$reasons.Add("empty-digests:$($property.Name)")
            }
            else {
                $platforms[$property.Name] = $digests
            }
        }
    }
    else {
        [void]$reasons.Add("missing-platforms")
    }

    if (-not $platforms.ContainsKey("windows")) {
        [void]$reasons.Add("windows-baseline-missing")
    }
    else {
        $windowsDigests = $platforms["windows"]
        $coverage["windows"] = "certified"
        foreach ($panel in $windowsDigests.Keys) {
            foreach ($other in @("linux", "macos")) {
                if ($platforms.ContainsKey($other) -and $platforms[$other].ContainsKey($panel)) {
                    if ($platforms[$other][$panel] -eq $windowsDigests[$panel]) {
                        [void]$reasons.Add("inherited-digest:$panel-from-$other")
                    }
                }
            }
        }
    }
    # A windows record is certified only with a DISTINCT windows environment
    # fingerprint. Reusing linux/macos (or a fingerprint-less placeholder)
    # means the record was not produced by a Windows re-pin and must be
    # refused, never accepted as an inherited or fabricated pin.
    if (-not [string]::IsNullOrWhiteSpace($windowsEnvironment)) {
        if (-not $windowsEnvironment.StartsWith("windows-")) {
            [void]$reasons.Add("windows-environment-mismatch:$windowsEnvironment")
        }
        if (-not (Test-FontFingerprint $windowsEnvironment)) {
            [void]$reasons.Add("missing-font-fingerprint:windows")
        }
        foreach ($other in @("linux", "macos")) {
            if ($environments.ContainsKey($other) -and $environments[$other] -eq $windowsEnvironment) {
                [void]$reasons.Add("inherited-environment:windows-from-$other")
            }
        }
    }
    foreach ($platform in @("linux", "macos")) {
        if ($platforms.ContainsKey($platform)) { $coverage[$platform] = "certified" }
    }

    return [pscustomobject]@{
        ok              = ($reasons.Count -eq 0)
        reasons         = @($reasons)
        coverage        = $coverage
        windows_digests = $windowsDigests
        windows_environment = $windowsEnvironment
    }
}

function Write-JsonFile {
    param([string]$Path, [object]$Object)
    $directory = Split-Path -Parent $Path
    if ($directory -and -not (Test-Path $directory)) {
        New-Item -ItemType Directory -Force -Path $directory | Out-Null
    }
    Set-Content -Path $Path -Value ($Object | ConvertTo-Json -Compress -Depth 8)
}

function Get-RepoTree {
    Push-Location $RepoRoot
    try {
        return (git rev-parse 'HEAD^{tree}' 2>$null).Trim()
    }
    catch {
        return ""
    }
    finally {
        Pop-Location
    }
}

function Write-LaneMarker {
    param(
        [string]$Status,
        [string]$Reason,
        [string[]]$Artifacts = @()
    )
    $now = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    $tree = Get-RepoTree
    # The lane declares BOTH commands (offline refusal self-test, then the
    # compare): the marker digest must cover exactly what ran.
    $cmds = @'
$ErrorActionPreference = "Stop"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows-visual-baseline.ps1 -SelfTest
$ErrorActionPreference = "Stop"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/windows-visual-baseline.ps1
'@ -replace "`r`n", "`n"
    $cmds = $cmds.TrimEnd("`n")
    $bytes = [Text.Encoding]::UTF8.GetBytes($cmds)
    $sha256 = [Security.Cryptography.SHA256]::Create()
    $cmdsDigest = ([BitConverter]::ToString($sha256.ComputeHash($bytes))).Replace("-", "").ToLower()
    $cmdsB64 = [Convert]::ToBase64String($bytes)

    $artifactEntries = @()
    $artifactDigest = "sha256:$EMPTY_ARTIFACT_DIGEST"
    if ($Artifacts.Count -gt 0) {
        # Same canonical digest as scripts/certification/evidence.mjs: artifact
        # entries sorted by path, each line `sha256:<hex>\t<path>\n`.
        $concatenated = ""
        foreach ($artifact in ($Artifacts | Sort-Object)) {
            $full = if ([System.IO.Path]::IsPathRooted($artifact)) { $artifact } else { Join-Path $RepoRoot $artifact }
            $hash = (Get-FileHash -Algorithm SHA256 -Path $full).Hash.ToLower()
            $artifactEntries += [ordered]@{ path = $artifact; sha256 = "sha256:$hash" }
            $concatenated += "sha256:$hash`t$artifact`n"
        }
        $concatBytes = [Text.Encoding]::UTF8.GetBytes($concatenated)
        $digestSha = [Security.Cryptography.SHA256]::Create()
        $artifactDigest = "sha256:" + ([BitConverter]::ToString($digestSha.ComputeHash($concatBytes))).Replace("-", "").ToLower()
    }

    $marker = [ordered]@{
        schema          = "faktor-woodpecker-lane/v2"
        lane            = "windows-visual-baseline"
        status          = $Status
        reason          = $Reason
        commit          = "$env:CI_COMMIT_SHA"
        tree            = $tree
        runner          = [ordered]@{ os = "windows"; arch = "amd64"; ci = "woodpecker"; run_id = "$env:CI_PIPELINE_NUMBER" }
        started_at      = $now
        finished_at     = $now
        commands_b64    = $cmdsB64
        commands_digest = "sha256:$cmdsDigest"
        artifacts       = $artifactEntries
        artifact_digest = $artifactDigest
    }
    Write-JsonFile -Path (Join-Path $RepoRoot $LaneMarker) -Object $marker
}

function Invoke-ParitySmoke {
    param([switch]$Write)
    $bash = Get-Command bash -ErrorAction SilentlyContinue
    if ($null -eq $bash) {
        throw "windows-jetbrains-toolchain-missing: bash (Git Bash) is required on the Windows agent to render/compare the JetBrains parity matrix; install Git Bash + the pinned JetBrains toolchain (kotlinc or the Gradle wrapper) and re-run"
    }
    $smokeArgs = @("apps/jetbrains/compile-and-smoke.sh")
    if ($Write) { $smokeArgs += "--write-baselines" }
    Push-Location $RepoRoot
    try {
        & bash @smokeArgs
        $rc = $LASTEXITCODE
    }    finally {
        Pop-Location
    }
    if ($rc -ne 0) {
        if ($Write) {
            throw "windows-visual-baseline-write-failed: bash apps/jetbrains/compile-and-smoke.sh --write-baselines exited $rc"
        }
        throw "windows-visual-render-drift: the pinned windows baseline does not match this render (bash apps/jetbrains/compile-and-smoke.sh exited $rc)"
    }
}

function Invoke-SelfTest {
    $hexA = "a" * 64
    $hexB = "b" * 64
    $hexC = "c" * 64
    $hexD = "d" * 64
    $linuxEnv = "linux-amd64-jvm17-f2927b2734270"
    $macEnv = "mac-os-x-aarch64-jvm17-f0123456789ab"
    $winEnv = "windows-amd64-jvm17-fcafef00d123"
    # A complete v3 baseline: every required platform carries its own record
    # and the windows record carries a DISTINCT windows font fingerprint (the
    # shape a -WriteBaselines re-pin writes on the Windows agent).
    $complete = @"
{"schema":"$BASELINE_SCHEMA","requiredPlatforms":["linux","macos","windows"],"platforms":{"linux":{"environment":"$linuxEnv","digests":{"task-tree":"$hexA","settings":"$hexB"}},"macos":{"environment":"$macEnv","digests":{"task-tree":"$hexB","settings":"$hexA"}},"windows":{"environment":"$winEnv","digests":{"task-tree":"$hexC","settings":"$hexD"}}}}
"@
    $noWindows = @"
{"schema":"$BASELINE_SCHEMA","requiredPlatforms":["linux","macos","windows"],"platforms":{"linux":{"environment":"$linuxEnv","digests":{"task-tree":"$hexA"}},"macos":{"environment":"$macEnv","digests":{"task-tree":"$hexB"}}}}
"@
    $failures = 0
    $cases = @()

    $cases += [pscustomobject]@{ name = "complete v3 baseline passes"; text = $complete; expectOk = $true; expectReason = $null }
    $cases += [pscustomobject]@{ name = "missing windows record fails"; text = $noWindows; expectOk = $false; expectReason = "windows-baseline-missing" }
    $cases += [pscustomobject]@{ name = "inherited windows digest fails"; text = $complete.Replace($hexC, $hexA); expectOk = $false; expectReason = "inherited-digest:task-tree-from-linux" }
    $cases += [pscustomobject]@{ name = "malformed digest fails"; text = $complete.Replace($hexA, "not-a-digest"); expectOk = $false; expectReason = "bad-digest:linux/task-tree" }
    $cases += [pscustomobject]@{ name = "wrong windows digest (not 64-hex) fails"; text = $complete.Replace('"settings":"' + $hexD + '"', '"settings":"not-a-digest"'); expectOk = $false; expectReason = "bad-digest:windows/settings" }
    $cases += [pscustomobject]@{ name = "v2 canonical pool fails"; text = '{"schema":"faktor-parity-visual-baselines/v2","panelDigests":{}}'; expectOk = $false; expectReason = "wrong-schema:faktor-parity-visual-baselines/v2" }
    $cases += [pscustomobject]@{ name = "empty windows digests fail"; text = $complete.Replace('"windows":{"environment":"' + $winEnv + '","digests":{"task-tree":"' + $hexC + '","settings":"' + $hexD + '"}}', '"windows":{"environment":"' + $winEnv + '","digests":{}}'); expectOk = $false; expectReason = "empty-digests:windows" }
    # Re-pin policy: the windows record must be produced by a Windows render
    # (own windows- environment + font fingerprint), never inherited.
    $cases += [pscustomobject]@{ name = "windows record reusing the linux environment fails"; text = $complete.Replace($winEnv, $linuxEnv); expectOk = $false; expectReason = "windows-environment-mismatch:$linuxEnv" }
    $cases += [pscustomobject]@{ name = "windows record without a font fingerprint fails"; text = $complete.Replace($winEnv, "windows-amd64-jvm17"); expectOk = $false; expectReason = "missing-font-fingerprint:windows" }

    foreach ($case in $cases) {
        $result = Test-VisualBaselineText -Text $case.text
        $ok = ($result.ok -eq $case.expectOk)
        if ($ok -and $null -ne $case.expectReason) {
            $ok = ($result.reasons -contains $case.expectReason)
        }
        if ($ok -and $case.expectOk) {
            # The re-pin contract: a passing windows record exposes the
            # distinct windows environment fingerprint the pin recorded.
            $ok = ($result.windows_environment -eq $winEnv)
        }
        if ($ok) {
            Write-Host "windows-visual-baseline selftest ok: $($case.name)"
        }
        else {
            Write-Host "windows-visual-baseline selftest FAIL: $($case.name) -> ok=$($result.ok) reasons=$($result.reasons -join ',')" -ForegroundColor Red
            $failures += 1
        }
    }
    if ($failures -gt 0) {
        Write-Host "windows-visual-baseline selftest: FAIL ($failures case(s))" -ForegroundColor Red
        return 1
    }
    Write-Host "windows-visual-baseline selftest: PASS (missing/inherited/malformed/foreign-fingerprint windows baselines are refused)"
    return 0
}

if ($SelfTest) {
    exit (Invoke-SelfTest)
}

$baselineFull = Join-Path $RepoRoot $BaselinePath
# The certifying record `certificate-windows` (and the canonical `windows`
# workflow in scripts/certification/evidence.mjs) requires.
$recordPath = Join-Path $RepoRoot (Join-Path $OutDir "ci-certification-windows-visual.json")
$recordRel = "$OutDir/ci-certification-windows-visual.json"
$artifacts = @()
$mode = "compare"

try {
    if ($WriteBaselines) {
        $mode = "write"
        Write-Host "windows-visual-baseline: re-pinning the windows record from this render (--write-baselines)"
        Invoke-ParitySmoke -Write
    }

    if (-not (Test-Path $baselineFull)) {
        throw "windows-visual-baseline-missing: no baseline at $BaselinePath; run -WriteBaselines on the Windows agent (bash apps/jetbrains/compile-and-smoke.sh --write-baselines) and commit the merged file"
    }
    $text = Get-Content -Path $baselineFull -Raw
    $policy = Test-VisualBaselineText -Text $text
    if (-not $policy.ok) {
        throw "windows-visual-baseline-invalid: $($policy.reasons -join ', ')"
    }

    if ($WriteBaselines) {
        $produced = Join-Path $RepoRoot (Join-Path $OutDir "visual-baselines-windows.json")
        $directory = Split-Path -Parent $produced
        if ($directory -and -not (Test-Path $directory)) {
            New-Item -ItemType Directory -Force -Path $directory | Out-Null
        }
        Copy-Item -Path $baselineFull -Destination $produced -Force
        $artifacts += "$OutDir/visual-baselines-windows.json"
        Write-Host "windows-visual-baseline: recorded $produced (commit it into $BaselinePath)"
    }

    if (-not $SkipRender) {
        Write-Host "windows-visual-baseline: comparing this render against the pinned windows record"
        Invoke-ParitySmoke
    }

    $coverage = ($policy.coverage.GetEnumerator() | ForEach-Object { "$($_.Key)=$($_.Value)" }) -join ","
    $panelCount = 0
    if ($null -ne $policy.windows_digests) { $panelCount = $policy.windows_digests.Count }
    $record = [ordered]@{
        schema           = "faktor-windows-visual-baseline/v1"
        status           = "passed"
        mode             = $mode
        baseline_path    = $BaselinePath
        baseline_sha256  = (Get-FileHash -Algorithm SHA256 -Path $baselineFull).Hash.ToLower()
        windows_panels   = $panelCount
        windows_environment = $policy.windows_environment
        coverage         = $coverage
        coverage_note    = "the windows record is its own render with its own windows font fingerprint; no linux/macos digest or environment is ever inherited"
        re_pinned        = [bool]$WriteBaselines
        does_not_prove   = @("screenshot/pixel comparison", "interactive IDE behavior outside the parity smoke")
        commit           = "$env:CI_COMMIT_SHA"
        tree             = Get-RepoTree
        runner           = [ordered]@{ os = "windows"; arch = "amd64"; ci = "woodpecker"; run_id = "$env:CI_PIPELINE_NUMBER" }
        finished_at      = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
    }
    Write-JsonFile -Path $recordPath -Object $record
    # The lane marker REQUIRES this record: certificate-windows verifies the
    # artifact entry (path + sha256) and refuses a certificate without it.
    $artifacts += $recordRel
    Write-LaneMarker -Status "passed" -Reason "" -Artifacts $artifacts
    Write-Host "windows-visual-baseline: PASS ($panelCount windows panels, $coverage; record $recordRel)"
    exit 0
}
catch {
    $reason = $_.Exception.Message
    try {
        $failedRecord = [ordered]@{
            schema      = "faktor-windows-visual-baseline/v1"
            status      = "failed"
            reason      = $reason
            mode        = $mode
            commit      = "$env:CI_COMMIT_SHA"
            tree        = Get-RepoTree
            runner      = [ordered]@{ os = "windows"; arch = "amd64"; ci = "woodpecker"; run_id = "$env:CI_PIPELINE_NUMBER" }
            finished_at = (Get-Date).ToUniversalTime().ToString("yyyy-MM-ddTHH:mm:ssZ")
        }
        Write-JsonFile -Path $recordPath -Object $failedRecord
        $artifacts += $recordRel
        Write-LaneMarker -Status "failed" -Reason $reason -Artifacts $artifacts
    }
    catch {
        Write-Host "windows-visual-baseline: could not write the failure record: $($_.Exception.Message)" -ForegroundColor Red
    }
    Write-Host "windows-visual-baseline: FAIL: $reason" -ForegroundColor Red
    exit 1
}
