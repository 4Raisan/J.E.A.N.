param(
    [Parameter(Mandatory = $true)][string]$JeanExe,
    [Parameter(Mandatory = $true)][string]$VoiceWorkerExe,
    [Parameter(Mandatory = $true)][string]$RecordingsCsv,
    [Parameter(Mandatory = $true)][string]$ZipformerDataDir,
    [Parameter(Mandatory = $true)][string]$ParakeetDataDir,
    [Parameter(Mandatory = $true)][string]$OutputCsv,
    [switch]$ValidateOnly,
    [switch]$SelfTest
)

$ErrorActionPreference = 'Stop'

function Normalize-Transcript([string]$Text) {
    return ([regex]::Replace($Text.ToLowerInvariant(), '[^\p{L}\p{N}]+', ' ').Trim() -replace '\s+', ' ')
}

function Get-WordErrors([string]$Expected, [string]$Actual) {
    $reference = @((Normalize-Transcript $Expected) -split ' ' | Where-Object { $_ })
    $hypothesis = @((Normalize-Transcript $Actual) -split ' ' | Where-Object { $_ })
    $n = $reference.Count; $m = $hypothesis.Count
    $dp = New-Object 'int[,]' ($n + 1), ($m + 1)
    for ($i = 0; $i -le $n; $i++) { $dp[$i, 0] = $i }
    for ($j = 0; $j -le $m; $j++) { $dp[0, $j] = $j }
    for ($i = 1; $i -le $n; $i++) {
        for ($j = 1; $j -le $m; $j++) {
            $cost = if ($reference[$i - 1] -ceq $hypothesis[$j - 1]) { 0 } else { 1 }
            $dp[$i, $j] = [Math]::Min([Math]::Min($dp[($i - 1), $j] + 1, $dp[$i, ($j - 1)] + 1), $dp[($i - 1), ($j - 1)] + $cost)
        }
    }
    $substitutions = 0; $deletions = 0; $insertions = 0; $i = $n; $j = $m
    while ($i -gt 0 -or $j -gt 0) {
        if ($i -gt 0 -and $j -gt 0 -and $reference[$i - 1] -ceq $hypothesis[$j - 1] -and $dp[$i, $j] -eq $dp[($i - 1), ($j - 1)]) { $i--; $j--; continue }
        if ($i -gt 0 -and $j -gt 0 -and $dp[$i, $j] -eq ($dp[($i - 1), ($j - 1)] + 1)) { $substitutions++; $i--; $j--; continue }
        if ($i -gt 0 -and $dp[$i, $j] -eq ($dp[($i - 1), $j] + 1)) { $deletions++; $i--; continue }
        $insertions++; $j--
    }
    $errors = $substitutions + $deletions + $insertions
    [pscustomobject]@{ ReferenceWords = $n; Substitutions = $substitutions; Deletions = $deletions; Insertions = $insertions; TranscriptErrors = $errors; WordErrorRate = if ($n -eq 0) { $null } else { [Math]::Round($errors / $n, 4) } }
}

function Test-ExactPhrase([string]$Transcript, [string]$Term) {
    $text = ' ' + (Normalize-Transcript $Transcript) + ' '
    $phrase = ' ' + (Normalize-Transcript $Term) + ' '
    return $text.Contains($phrase)
}

function Assert-RecognizerConfig([string]$DataDir, [string]$Expected) {
    $configPath = Join-Path $DataDir 'jean.toml'
    $text = Get-Content -LiteralPath $configPath -Raw
    $section = [regex]::Match($text, '(?ms)^\[voice\]\s*(?<body>.*?)(?=^\[|\z)')
    if (-not $section.Success -or -not [regex]::IsMatch($section.Groups['body'].Value, ('(?m)^\s*recognizer\s*=\s*["'']' + [regex]::Escape($Expected) + '["'']\s*(?:#.*)?$'))) {
        throw "$configPath must set [voice].recognizer = '$Expected' for this comparison leg."
    }
}

if ($SelfTest) {
    $errors = Get-WordErrors 'Jean opens Kokoro' 'Jean opens Coco'
    if ($errors.Substitutions -ne 1 -or $errors.Deletions -ne 0 -or $errors.Insertions -ne 0 -or $errors.WordErrorRate -ne 0.3333) { throw "WER self-test failed: $($errors | ConvertTo-Json -Compress)" }
    if (-not (Test-ExactPhrase 'Open Jean Desktop now' 'Jean Desktop')) { throw 'exact phrase self-test failed for a matching phrase' }
    if (Test-ExactPhrase 'Open Jeanette Desktop now' 'Jean') { throw 'exact phrase self-test accepted a partial word' }
    Write-Output 'Synthetic scoring checks passed; no audio was inferred.'
    return
}

function Invoke-Recognizer([string]$Model, [string]$DataDir, $Case) {
    $wav = [IO.Path]::GetFullPath([IO.Path]::Combine((Split-Path -Parent $RecordingsCsv), $Case.wav))
    if (-not (Test-Path -LiteralPath $wav -PathType Leaf)) { throw "Recording not found: $wav" }
    $tempRoot = Join-Path ([IO.Path]::GetTempPath()) ("jean-asr-" + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $tempRoot | Out-Null
    $stdout = Join-Path $tempRoot 'stdout.json'
    $stderr = Join-Path $tempRoot 'stderr.txt'
    $oldDataDir = $env:JEAN_DATA_DIR
    $oldWorker = $env:JEAN_VOICE_WORKER
    try {
        $env:JEAN_DATA_DIR = (Resolve-Path -LiteralPath $DataDir).Path
        $env:JEAN_VOICE_WORKER = (Resolve-Path -LiteralPath $VoiceWorkerExe).Path
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $proc = Start-Process -FilePath (Resolve-Path -LiteralPath $JeanExe).Path `
            -ArgumentList @('voice', 'transcribe', ('"' + $wav.Replace('"', '\"') + '"')) `
            -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
        [long]$peak = 0
        while (-not $proc.HasExited) {
            try {
                $proc.Refresh()
                $ids = @($proc.Id) + @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($proc.Id)" | ForEach-Object { [int]$_.ProcessId })
                $workingSet = [long]0
                foreach ($id in $ids) {
                    $sample = Get-Process -Id $id -ErrorAction SilentlyContinue
                    if ($sample) { $workingSet += $sample.WorkingSet64 }
                }
                if ($workingSet -gt $peak) { $peak = $workingSet }
            } catch { }
            Start-Sleep -Milliseconds 40
        }
        $proc.WaitForExit()
        $clock.Stop()
        $raw = if (Test-Path -LiteralPath $stdout) { Get-Content -LiteralPath $stdout -Raw } else { '' }
        $err = if (Test-Path -LiteralPath $stderr) { Get-Content -LiteralPath $stderr -Raw } else { '' }
        $transcript = ''
        $offlineProcessingMs = $null
        $engine = ''
        $modelConfirmed = $false
        if ($proc.ExitCode -eq 0) {
            try {
                $result = ConvertFrom-Json -InputObject $raw
                if ($null -eq $result.offline_processing_ms) { throw 'worker output has no offline_processing_ms field' }
                $transcript = [string]$result.transcript
                $offlineProcessingMs = [long]$result.offline_processing_ms
                $engine = [string]$result.engine
                $expectedEngine = if ($Model -eq 'zipformer') { 'streaming Zipformer transducer ASR' } else { 'offline Parakeet TDT v2 INT8 ASR' }
                $modelConfirmed = $engine.Contains($expectedEngine)
                if (-not $modelConfirmed) { $err += "`nSelected $Model but worker reported engine '$engine'; do not treat this row as inference for the selected model." }
            } catch { $err += "`nCould not parse offline transcript JSON: $raw`n$($_.Exception.Message)" }
        }
        $normalized = Normalize-Transcript $transcript
        $wordErrors = Get-WordErrors ([string]$Case.expected) $transcript
        $appResults = @{}
        foreach ($term in ([string]$Case.app_names -split ';' | Where-Object { $_.Trim() -and $_.Trim() -ne '-' })) {
            $key = $term.Trim()
            $appResults[$key] = Test-ExactPhrase $transcript $key
        }
        $developerResults = @{}
        foreach ($term in ([string]$Case.developer_terms -split ';' | Where-Object { $_.Trim() -and $_.Trim() -ne '-' })) {
            $key = $term.Trim()
            $developerResults[$key] = Test-ExactPhrase $transcript $key
        }
        [pscustomobject]@{
            Model = $Model
            Engine = $engine
            ModelConfirmed = $modelConfirmed
            Recording = $Case.wav
            ExitCode = $proc.ExitCode
            ExactTranscript = ($normalized -ceq (Normalize-Transcript ([string]$Case.expected)))
            ReferenceWords = $wordErrors.ReferenceWords
            Substitutions = $wordErrors.Substitutions
            Deletions = $wordErrors.Deletions
            Insertions = $wordErrors.Insertions
            TranscriptErrors = $wordErrors.TranscriptErrors
            WordErrorRate = $wordErrors.WordErrorRate
            AppNameMatchesJson = ($appResults | ConvertTo-Json -Compress)
            AppNameMatches = @($appResults.Values | Where-Object { $_ }).Count
            DeveloperTermMatchesJson = ($developerResults | ConvertTo-Json -Compress)
            DeveloperTermMatches = @($developerResults.Values | Where-Object { $_ }).Count
            Transcript = $transcript
            OfflineProcessingMs = $offlineProcessingMs
            CommandWallMs = $clock.ElapsedMilliseconds
            PeakWorkingSetBytes = $peak
            Error = $err.Trim()
        }
    } finally {
        $env:JEAN_DATA_DIR = $oldDataDir
        $env:JEAN_VOICE_WORKER = $oldWorker
        Remove-Item -LiteralPath $tempRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

foreach ($required in @($JeanExe, $VoiceWorkerExe, $RecordingsCsv, $ZipformerDataDir, $ParakeetDataDir)) {
    if (-not (Test-Path -LiteralPath $required)) { throw "Required comparison input is missing: $required" }
}
foreach ($dataDir in @($ZipformerDataDir, $ParakeetDataDir)) {
    if (-not (Test-Path -LiteralPath (Join-Path $dataDir 'jean.toml'))) { throw "Missing isolated config: $(Join-Path $dataDir 'jean.toml')" }
}
Assert-RecognizerConfig $ZipformerDataDir 'zipformer'
Assert-RecognizerConfig $ParakeetDataDir 'parakeet_tdt_v2_int8'
$cases = @(Import-Csv -LiteralPath $RecordingsCsv)
if ($cases.Count -eq 0) { throw 'The recording manifest has no rows.' }
foreach ($case in $cases) {
    if (-not $case.wav -or -not $case.expected -or -not $case.app_names -or -not $case.developer_terms) { throw 'Each CSV row must define wav, expected, app_names and developer_terms (term lists separated by semicolons; use an explicit term when a category does not apply).' }
    $wav = [IO.Path]::GetFullPath([IO.Path]::Combine((Split-Path -Parent $RecordingsCsv), $case.wav))
    if (-not (Test-Path -LiteralPath $wav -PathType Leaf)) { throw "Recording not found: $wav" }
}

if ($ValidateOnly) {
    Write-Output "Validated manifest and isolated recognizer configs ($($cases.Count) WAV case(s)); no inference was run."
    return
}

$rows = @()
foreach ($case in $cases) {
    $rows += Invoke-Recognizer 'zipformer' $ZipformerDataDir $case
    $rows += Invoke-Recognizer 'parakeet_tdt_v2_int8' $ParakeetDataDir $case
}
$rows | Export-Csv -LiteralPath $OutputCsv -NoTypeInformation -Encoding UTF8
Write-Output "Wrote $($rows.Count) measurements to $OutputCsv"
Write-Output 'OfflineProcessingMs is worker model load + WAV decode/inference time; CommandWallMs also includes CLI startup. Neither is live microphone release-to-text latency.'
