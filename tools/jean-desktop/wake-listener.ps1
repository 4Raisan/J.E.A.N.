param([switch]$TerminateOnly)
$ErrorActionPreference = 'Stop'
$engine = $null
$wake = $false
try {
    Add-Type -AssemblyName System.Speech
    $engine = [System.Speech.Recognition.SpeechRecognitionEngine]::new()
    $names = [string[]]@('Jean', 'Dean', 'Gene', 'G', 'Gee')
    [string[]]$phrases = @()
    if (-not $TerminateOnly) {
        $phrases += $names + @($names | ForEach-Object { "Hey $_" })
    }
    $phrases += 'Jean terminate'
    $builder = [System.Speech.Recognition.GrammarBuilder]::new()
    $builder.Culture = $engine.RecognizerInfo.Culture
    $builder.Append([System.Speech.Recognition.Choices]::new($phrases))
    $engine.LoadGrammar([System.Speech.Recognition.Grammar]::new($builder))
    $engine.SetInputToDefaultAudioDevice()
    Write-Output 'READY'
    while (-not $wake) {
        $result = $engine.Recognize()
        $wake = $null -ne $result -and $result.Confidence -ge 0.70
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.GetBaseException().Message)
    exit 1
} finally {
    if ($null -ne $engine) {
        $engine.Dispose()
    }
}
if ($wake) {
    if ($result.Text -eq 'Jean terminate') { Write-Output 'TERMINATE' }
    else { Write-Output 'WAKE' }
}
