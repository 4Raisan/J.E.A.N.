$ErrorActionPreference = 'Stop'
$engine = $null
try {
    Add-Type -AssemblyName System.Speech
    $engine = [System.Speech.Recognition.SpeechRecognitionEngine]::new()
    [string[]]$phrases = @('task approved', 'approve task', 'deny task',
                           'task denied', 'Jean terminate')
    $builder = [System.Speech.Recognition.GrammarBuilder]::new()
    $builder.Culture = $engine.RecognizerInfo.Culture
    $builder.Append([System.Speech.Recognition.Choices]::new($phrases))
    $engine.LoadGrammar([System.Speech.Recognition.Grammar]::new($builder))
    $engine.SetInputToDefaultAudioDevice()
    do {
        $result = $engine.Recognize()
    } while ($null -eq $result -or $result.Confidence -lt 0.80)
    switch ($result.Text) {
        'Jean terminate' { Write-Output 'TERMINATE' }
        { $_ -in 'deny task', 'task denied' } { Write-Output 'DENY' }
        default { Write-Output 'APPROVE' }
    }
} catch {
    [Console]::Error.WriteLine($_.Exception.GetBaseException().Message)
    exit 1
} finally {
    if ($null -ne $engine) {
        $engine.Dispose()
    }
}
