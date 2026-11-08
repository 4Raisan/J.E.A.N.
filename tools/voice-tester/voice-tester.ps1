<#
JEAN voice tester: a small window for the two things that need a human, plus one that
does not.

It talks the owner protocol that `jean interact` already speaks -- newline-delimited
Envelope<OwnerCommand> frames in, Envelope<OwnerReply> frames out -- over the stdin and
stdout of that same command, and it supervises the same worker `jean voice` verbs use.
Nothing here is a second speech stack.

What it deliberately does NOT do is turn a spoken phrase into a task. `Submit` and
`SubmitTranscript` are never sent: the recognized words are read out of
`snapshot.transcript` and the press is dropped with `ClearCapture`, so this window has no
code path that could start work. `model.provider = "none"` in its isolated config is the
second lock on the same door.

The words land in an editable Dictation field, which is also where an external dictation
engine (Wispr Flow's hotkey, or anything else that types into the focused control) puts text:
standard Windows text input, so the caret, selection and clipboard accelerators are the
system's. That field is a review surface, nothing more. Handing reviewed text to JEAN as a goal
is a separate, explicit action for the task UI to own, and adding it here would reintroduce
exactly the coupling this window exists to avoid.

  -Offline            use config/offline, where a WAV is the microphone and the speaker is
                      a counted sink: real engines, no endpoints, nothing audible.
  -SelfTest           drive the same state machine headlessly and report timings.
  -HoldSeconds        how long the self test holds the press open (default 7, and the
                      published clip is 6.625 s).
#>
param(
    [switch]$Offline,
    [switch]$SelfTest,
    [int]$HoldSeconds = 7
)

$ErrorActionPreference = 'Stop'

# The launcher starts this process with -WindowStyle Hidden because one small window is the
# product, so there is no console to die in: a script that throws before the form exists would
# take the double-click with it and leave no trace. A dialog is the only surface that does not
# depend on the console, hence the assembly here rather than further down.
Add-Type -AssemblyName System.Windows.Forms
trap {
    $text = if ($_.Exception) { "$($_.Exception.Message)" } else { "$_" }
    if ($SelfTest) {
        # A dialog nobody can click would hang an unattended run; this path has a console.
        Write-Output "FAILED: $text"
        exit 1
    }
    [void][System.Windows.Forms.MessageBox]::Show(
        $text, 'JEAN voice tester could not start', 'OK', 'Error')
    exit 1
}

# ---------------------------------------------------------------- paths, from our own
# location: Explorer gives a launcher the folder it sits in, not one we can guess.
$Here = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $MyInvocation.MyCommand.Path }
$Root = (Resolve-Path (Join-Path $Here (Join-Path '..' '..'))).Path
$Jean = Join-Path $Root 'target\debug\jean.exe'
$Worker = Join-Path $Root 'target\p5q\bin\jean-voice-sherpa-dynamic.exe'
$Mode = if ($Offline) { 'offline' } else { 'mic' }
$Data = Join-Path $Here (Join-Path 'config' $Mode)
$Config = Join-Path $Data 'jean.toml'

# The sample the window speaks. Ordinary words on purpose: they are what the lexicon
# path proves, and none of them are the fifteen entries the installed model cannot
# render (IMPLEMENTATION_STATUS.md 63.1).
$TestSentence = 'Jean voice tester. This is a spoken test sentence.'

function Get-SetupProblems {
    # Each string lands in a 436 px label, so they are written to be read at a glance: what is
    # missing, where it should be, and the one command that produces it. A path that is longer
    # than the line gets its own entry in the detail row rather than pushing the advice off it.
    $found = @()
    if (-not (Test-Path -LiteralPath $Jean)) {
        $found += "JEAN CLI is not built. Run: cargo build -p jean-cli"
        $found += "looked in: $Jean"
    }
    if (-not (Test-Path -LiteralPath $Worker)) {
        $found += 'Voice worker is missing. Rebuild it with --features sherpa into target\p5q\bin (voice-assets.md 4a-ii).'
        $found += "looked in: $Worker"
    }
    if (-not (Test-Path -LiteralPath $Config)) {
        $found += "The isolated voice config is missing: $Config"
    }
    if (Test-Path -LiteralPath $Config) {
        $text = Get-Content -LiteralPath $Config -Raw
        $wav = [regex]::Match($text, "offline_wav\s*=\s*'([^']+)'").Groups
        if ($Mode -eq 'offline') {
            if (-not $wav[1].Success -or -not (Test-Path -LiteralPath $wav[1].Value)) {
                $found += "Offline mode needs the WAV this config names, and it is not there: $($wav[1].Value)"
            }
        }
        foreach ($key in 'encoder', 'tokens', 'tts_model', 'tts_lexicon') {
            $m = [regex]::Match($text, "$key\s*=\s*'([^']+)'")
            if (-not $m.Groups[1].Success) { $found += "voice.models.$key is not set in $Config"; continue }
            if (-not (Test-Path -LiteralPath $m.Groups[1].Value)) {
                $found += "voice.models.$key points at a file that is not there: $($m.Groups[1].Value)"
            }
        }
    }
    return $found
}

# Isolation is set here, in this process's own environment, and not only on the session's
# ProcessStartInfo: the self test also runs `jean tasks` to prove nothing was submitted, and
# an unqualified call would read your real task database instead of the tester's.
$env:JEAN_DATA_DIR = $Data
$env:JEAN_VOICE_WORKER = $Worker
Remove-Item Env:JEAN_NO_VOICE -ErrorAction SilentlyContinue

# ---------------------------------------------------------------- protocol client
$Queue = [System.Collections.Concurrent.ConcurrentQueue[string]]::new()
$ErrQueue = [System.Collections.Concurrent.ConcurrentQueue[string]]::new()
$script:Session = $null
$script:Next = 0
$script:Snap = $null
$script:Reply = $null          # the Envelope<OwnerReply> body answering $script:Pending
$script:ReplyId = -1           # the request id that body answers
$script:Pending = 0            # request id whose reply moves the state machine
$script:Phase = 'boot'
$script:Activation = $null
$script:Utterance = $null
$script:Status = 'Starting JEAN...'
$script:Words = ''
# Words waiting to be placed in the dictation field: a delivery, not a mirror. Keeping them separate is what lets the operator edit the field without the state machine
# overwriting the edit on the next 100 ms tick.
$script:InsertWords = ''
# StartNew, not new(): `Stopwatch::new()` is created *stopped*, which makes every Elapsed read 0
# and silently disables every phase deadline below -- the window would then wait forever instead
# of saying the microphone never confirmed.
$script:Stamp = [System.Diagnostics.Stopwatch]::StartNew()
$script:PhaseSince = 0
$script:Log = New-Object System.Collections.Generic.List[string]
$script:Proc = $null
$script:Stdin = $null
$script:LastErr = ''

function Write-Log($text) {
    $script:Log.Add(('{0,7} ms  {1}' -f [int]$script:Stamp.Elapsed.TotalMilliseconds, $text))
}

function Set-Phase($phase, $status) {
    $script:Phase = $phase
    $script:PhaseSince = $script:Stamp.Elapsed.TotalMilliseconds
    if ($status) { $script:Status = $status }
    Write-Log ("{0}: {1}" -f $phase, $status)
}

function Start-JeanSession {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $Jean
    $psi.Arguments = 'interact'          # JSON frames, which is what this window speaks
    $psi.WorkingDirectory = $Root
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.StandardOutputEncoding = [System.Text.UTF8Encoding]::new($false)
    $psi.StandardErrorEncoding = [System.Text.UTF8Encoding]::new($false)
    $psi.EnvironmentVariables['JEAN_DATA_DIR'] = $Data
    $psi.EnvironmentVariables['JEAN_VOICE_WORKER'] = $Worker
    $psi.EnvironmentVariables.Remove('JEAN_NO_VOICE') | Out-Null

    $script:Proc = [System.Diagnostics.Process]::Start($psi)
    $script:Stdin = $script:Proc.StandardInput
    $script:Stdin.AutoFlush = $true

    # Reads land on the process's own event thread and only ever touch a queue: the UI
    # thread never waits on the worker, which is the whole point of the split.
    $script:OutEvent = Register-ObjectEvent -InputObject $script:Proc -EventName OutputDataReceived `
        -MessageData $Queue -Action { $Event.MessageData.Enqueue([string]$EventArgs.Data) }
    $script:ErrEvent = Register-ObjectEvent -InputObject $script:Proc -EventName ErrorDataReceived `
        -MessageData $ErrQueue -Action { $Event.MessageData.Enqueue([string]$EventArgs.Data) }
    $script:Proc.BeginOutputReadLine()
    $script:Proc.BeginErrorReadLine()
    Write-Log ("session process {0} started; data {1}; worker {2}" -f $script:Proc.Id, $Data, $Worker)
}

function Send-Owner($op, $props) {
    if (-not $script:Stdin) { return 0 }
    $script:Next = $script:Next + 1
    $body = New-Object System.Collections.Specialized.OrderedDictionary
    $body['op'] = $op
    if ($props) { foreach ($k in $props.Keys) { $body[$k] = $props[$k] } }
    $frame = New-Object System.Collections.Specialized.OrderedDictionary
    $frame['version'] = 1
    $frame['session'] = [string]$script:Session
    $frame['request'] = $script:Next
    $frame['deadline_ms'] = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds() + 5000
    $frame['body'] = $body
    $json = ConvertTo-Json -InputObject $frame -Compress -Depth 4
    # Explicit UTF-8 bytes with one LF, flushed on the raw stream: writing text through the
    # StreamWriter would add CRLF, and touching its buffer at all makes it emit its UTF-8
    # encoding preamble ahead of the very first frame. That preamble is unavoidable here --
    # it appears the moment StandardInput is read -- so the session drops it on arrival
    # rather than treating a well-formed frame as malformed, which is what it used to do.
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json + "`n")
    try {
        $stream = $script:Stdin.BaseStream
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush()
    } catch { Write-Log "stdin closed: $_" }
    return $script:Next
}

function Pump-Frames {
    $line = $null
    while ($Queue.TryDequeue([ref]$line)) {
        if ([string]::IsNullOrWhiteSpace($line)) { continue }
        $object = $null
        try { $object = $line | ConvertFrom-Json } catch { Write-Log "undecodable frame: $line"; continue }
        if ($object.PSObject.Properties.Name -contains 'version') {
            $id = [int]$object.request
            Write-Log ("reply {0}: accepted={1} {2}" -f $id, $object.body.accepted, $object.body.detail)
            if ($object.PSObject.Properties.Name -contains 'session') { $script:Session = $object.session }
            if ($object.body.PSObject.Properties.Name -contains 'snapshot') { $script:Snap = $object.body.snapshot }
            # Only the reply that answers the request the machine is waiting on becomes
            # `$script:Reply`. The `status` polls share the same pipe, and letting one of
            # them overwrite an answer that has already arrived would drop a state
            # transition -- a frame that says the microphone opened, then never gets acted on.
            if ($id -eq [int]$script:Pending) {
                $script:Reply = $object.body
                $script:ReplyId = $id
            }
        }
        else {
            # The session announces itself with a bare Snapshot before any request.
            $script:Snap = $object
            if ($object.PSObject.Properties.Name -contains 'session') { $script:Session = $object.session }
        }
    }
    $err = $null
    while ($ErrQueue.TryDequeue([ref]$err)) {
        if ($err) {
            $script:LastErr = $err
            Write-Log "session stderr: $err"
        }
    }
}

function Property($name) {
    if ($script:Snap -and ($script:Snap.PSObject.Properties.Name -contains $name)) { return $script:Snap.$name }
    return $null
}

# Put finalized words where the operator is working, the way a dictation engine does: at the
# caret, separated from what is already there, with the caret left after the inserted text so
# the next phrase continues from there instead of starting a new paragraph. Selection is
# replaced, because text the operator has highlighted is text they meant to get rid of.
function Insert-Words($box, $text) {
    if (-not $box -or [string]::IsNullOrWhiteSpace($text)) { return }
    $start = $box.SelectionStart
    $current = $box.Text
    $separator = ''
    if ($start -gt 0 -and -not [char]::IsWhiteSpace($current[$start - 1])) { $separator = ' ' }
    $box.Text = $current.Substring(0, $start) + $separator + $text + $current.Substring($start + $box.SelectionLength)
    $box.SelectionStart = $start + $separator.Length + $text.Length
    $box.SelectionLength = 0
    $box.ScrollToCaret()
}

# One tick of the state machine. Every wait is expressed as "what am I watching for",
# so the same function drives the window's timer and the headless self test.
function Step-Machine {
    Pump-Frames

    # Ahead of the session guard: a process that dies on a bad config never sends the opening
    # snapshot, and this window would otherwise sit saying "Starting JEAN..." forever. What it
    # wrote on stderr -- an older CLI handed a frame it cannot decode, a worker that could not
    # load, a config that will not parse -- is the actionable part.
    if ($script:Phase -ne 'failed' -and $script:Proc -and $script:Proc.HasExited) {
        Set-Phase 'failed' ("The JEAN session exited with code {0}." -f $script:Proc.ExitCode)
        if ($script:LastErr) { $script:Status = "$($script:Status) $($script:LastErr.Trim())" }
        return
    }
    if (-not $script:Session) { return }

    switch ($script:Phase) {
        'boot' {
            # The worker loads the recognizer, Kokoro and the 6 MB lexicon before it
            # answers Configured, so this phase is real work, not a spinner.
            $voice = [string](Property 'voice')
            if (-not ($script:Snap.PSObject.Properties.Name -contains 'capturing')) {
                # Same lesson as the preserved workers, on the other side of the pipe: a
                # CLI built before this protocol field cannot answer what the window asks,
                # and a silent 20 s timeout would look like a broken microphone.
                Set-Phase 'failed' 'JEAN CLI is older than this tester. Rebuild it: cargo build -p jean-cli'
            }
            elseif ($voice -eq 'ready') { Set-Phase 'idle' 'Ready. Start listening, or play the test sentence.' }
            elseif ($voice -like 'unavailable*') { Set-Phase 'failed' "Voice worker could not start: $voice" }
            elseif ($voice -eq 'disabled') { Set-Phase 'failed' 'Voice is disabled in this configuration; nothing to test.' }
            else {
                if ($script:Tick % 3 -eq 0) { Send-Owner 'status' $null | Out-Null }
                $script:Status = 'Starting worker (loading models)...'
            }
        }
        'await-begin' {
            if ($script:Reply -and [int]$script:ReplyId -eq [int]$script:Pending) {
                if ($script:Reply.accepted) {
                    $script:Activation = [int](Property 'activation')
                    Set-Phase 'starting' 'Starting microphone...'
                }
                else { Set-Phase 'idle' "Start refused: $($script:Reply.detail)" }
                $script:Reply = $null
            }
        }
        'starting' {
            if ($script:Tick % 2 -eq 0) { Send-Owner 'status' $null | Out-Null }
            if ((Property 'capturing') -eq $true) { Set-Phase 'listening' 'Listening - speak now.' }
            elseif ((Property 'activation') -eq $null) { Set-Phase 'idle' 'Capture ended on its own (deadline expired).' }
            elseif ($script:Stamp.Elapsed.TotalMilliseconds - $script:PhaseSince -gt 20000) {
                Set-Phase 'idle' 'The microphone never confirmed. Press Start again, or check jean voice devices.'
            }
        }
        'listening' {
            $script:Status = 'Listening - speak now.'
        }
        'await-end' {
            if ($script:Reply -and [int]$script:ReplyId -eq [int]$script:Pending) {
                if ($script:Reply.accepted) { Set-Phase 'transcribing' 'Transcribing...' }
                else { Set-Phase 'idle' "Stop refused: $($script:Reply.detail)" }
                $script:Reply = $null
            }
        }
        'transcribing' {
            if ($script:Tick % 2 -eq 0) { Send-Owner 'status' $null | Out-Null }
            $words = [string](Property 'transcript')
            if ($words) {
                $script:Words = $words
                # One-shot hand-off to the dictation field. It is a delivery, not a mirror:
                # the field is editable, so the next tick must not push these words over
                # whatever the operator has typed or corrected since they arrived.
                $script:InsertWords = $words
                Write-Log ("transcript in {0} ms: {1}" -f [int]($script:Stamp.Elapsed.TotalMilliseconds - $script:PhaseSince), $words)
                $script:Pending = Send-Owner 'clear_capture' @{ activation = [int]$script:Activation }
                Set-Phase 'await-clear' "Heard: $words"
            }
            elseif ($script:Stamp.Elapsed.TotalMilliseconds - $script:PhaseSince -gt 45000) {
                $script:Pending = Send-Owner 'clear_capture' @{ activation = [int]$script:Activation }
                Set-Phase 'await-clear' 'No transcript arrived for that press.'
            }
        }
        'await-clear' {
            if ($script:Reply -and [int]$script:ReplyId -eq [int]$script:Pending) {
                $script:Activation = $null
                Set-Phase 'idle' $script:Status
                $script:Reply = $null
            }
        }
        'await-speak' {
            if ($script:Reply -and [int]$script:ReplyId -eq [int]$script:Pending) {
                if ($script:Reply.accepted) {
                    $script:Utterance = [int](Property 'utterance')
                    Set-Phase 'speaking' 'Speaking the test sentence...'
                }
                else { Set-Phase 'idle' "Play refused: $($script:Reply.detail)" }
                $script:Reply = $null
            }
        }
        'speaking' {
            if ($script:Tick % 2 -eq 0) { Send-Owner 'status' $null | Out-Null }
            if ((Property 'utterance') -eq $null) {
                Set-Phase 'idle' ("Playback finished. Stop worked; the slot freed in {0} ms." -f [int]($script:Stamp.Elapsed.TotalMilliseconds - $script:PhaseSince))
            }
            else {
                $script:Status = 'Speaking (synthesis runs at about 1.7x real time, so sound starts a few seconds in).'
            }
        }
        'await-stop' {
            if ($script:Reply -and [int]$script:ReplyId -eq [int]$script:Pending) {
                if ($script:Reply.accepted) { Set-Phase 'stopping' 'Stopping playback...' }
                else { Set-Phase 'speaking' "Stop refused: $($script:Reply.detail)" }
                $script:Reply = $null
            }
        }
        'stopping' {
            if ($script:Tick % 2 -eq 0) { Send-Owner 'status' $null | Out-Null }
            if ((Property 'utterance') -eq $null) {
                Set-Phase 'idle' ("Playback stopped in {0} ms; nothing was cancelled but the utterance." -f [int]($script:Stamp.Elapsed.TotalMilliseconds - $script:PhaseSince))
            }
        }
        'failed' { }
    }
    $script:Tick = $script:Tick + 1
}

$script:Tick = 0

# ---------------------------------------------------------------- the four actions
function Do-StartListening {
    if ($script:Phase -ne 'idle') { $script:Status = "Not now: currently $($script:Phase)."; return }
    $script:Pending = Send-Owner 'begin_capture' $null
    Set-Phase 'await-begin' 'Asking the worker to open the microphone...'
}

function Do-StopListening {
    if ($script:Phase -notin 'listening', 'starting') { $script:Status = "Nothing is being captured (state: $($script:Phase))."; return }
    $script:Pending = Send-Owner 'end_capture' @{ activation = [int]$script:Activation }
    Set-Phase 'await-end' 'Releasing the press...'
}

function Do-PlaySentence {
    if ($script:Phase -ne 'idle') { $script:Status = "Not now: currently $($script:Phase)."; return }
    $script:Pending = Send-Owner 'speak' @{ text = $TestSentence }
    Set-Phase 'await-speak' 'Asking the worker to speak...'
}

function Do-StopPlayback {
    if ($script:Phase -notin 'speaking', 'stopping') { $script:Status = 'Nothing is playing.'; return }
    $script:Pending = Send-Owner 'interrupt' @{ utterance = [int]$script:Utterance }
    Set-Phase 'await-stop' "Interrupting utterance $($script:Utterance)..."
}

function Stop-JeanSession {
    if (-not $script:Proc) { return $true }
    try {
        Send-Owner 'shutdown' $null | Out-Null
        Start-Sleep -Milliseconds 200
        Pump-Frames
    } catch { }
    try { $script:Stdin.Close() } catch { }
    if (-not $script:Proc.WaitForExit(5000)) {
        Write-Log 'session did not exit within 5 s; terminating it'
        try { $script:Proc.Kill(); $script:Proc.WaitForExit(2000) | Out-Null } catch { }
        return $false
    }
    foreach ($event in @($script:OutEvent, $script:ErrEvent)) {
        if ($event) { Unregister-Event -SubscriptionId $event.Id -ErrorAction SilentlyContinue }
    }
    Write-Log ("session exited with code {0}" -f $script:Proc.ExitCode)
    return $script:Proc.ExitCode -eq 0
}

# ---------------------------------------------------------------- headless self test
if ($SelfTest) {
    $problems = @(Get-SetupProblems)
    Write-Output "mode    : $Mode"
    Write-Output "cli     : $Jean"
    Write-Output "worker  : $Worker"
    Write-Output "data    : $Data"
    if ($problems.Count) { $problems | ForEach-Object { Write-Output "PROBLEM: $_" }; exit 1 }

    Start-JeanSession
    $verdicts = [ordered]@{}

    # The dictation field's contract, checked on a control rather than on the window: words
    # arrive at the caret, back to back, and a phrase inserted into text the operator has
    # already edited does not walk over it. Nothing here needs the worker, so it is also the one
    # check that keeps running when the session cannot start.
    $box = New-Object System.Windows.Forms.TextBox
    try {
        Insert-Words $box 'first phrase'
        $box.SelectionStart = $box.Text.Length
        Insert-Words $box 'second phrase'
        $verdicts['dictation appends at caret'] = ($box.Text -eq 'first phrase second phrase')
        $box.Text = 'alpha  omega'
        $box.SelectionStart = 6
        Insert-Words $box 'beta'
        $verdicts['dictation inserts mid-edit'] = ($box.Text -eq 'alpha beta omega')
        $verdicts['dictation caret parks after words'] = ($box.SelectionStart -eq 'alpha beta'.Length)
        $box.Text = 'keep this'
        $box.SelectionStart = 0
        Insert-Words $box ''
        $verdicts['empty delivery changes nothing'] = ($box.Text -eq 'keep this')
    }
    finally { $box.Dispose() }
    $deadline = (Get-Date).AddSeconds(150)
    while ($script:Phase -eq 'boot' -and (Get-Date) -lt $deadline) { Step-Machine; Start-Sleep -Milliseconds 100 }
    $verdicts['worker ready'] = $script:Phase -eq 'idle'
    Write-Output ("worker ready after {0:N0} ms" -f $script:Stamp.Elapsed.TotalMilliseconds)

    if ($verdicts['worker ready']) {
        $before = $script:Stamp.Elapsed.TotalMilliseconds
        Do-StartListening
        $deadline = (Get-Date).AddSeconds(25)
        while ($script:Phase -notin 'listening', 'failed' -and (Get-Date) -lt $deadline) { Step-Machine; Start-Sleep -Milliseconds 50 }
        $verdicts['capture confirmed'] = $script:Phase -eq 'listening'
        Write-Output ("capture confirmed after {0:N0} ms" -f ($script:Stamp.Elapsed.TotalMilliseconds - $before))

        Start-Sleep -Seconds $HoldSeconds
        $before = $script:Stamp.Elapsed.TotalMilliseconds
        Do-StopListening
        $deadline = (Get-Date).AddSeconds(60)
        while ($script:Phase -notin 'idle', 'failed' -and $script:Words -eq '' -and (Get-Date) -lt $deadline) { Step-Machine; Start-Sleep -Milliseconds 50 }
        $verdicts['transcript arrived'] = [bool]$script:Words
        Write-Output ("transcript after {0:N0} ms: {1}" -f ($script:Stamp.Elapsed.TotalMilliseconds - $before), $script:Words)

        # The discard reply is still on the wire when the words land, and Play is refused
        # until the slot is idle again. Waiting here is the same rule the buttons enforce,
        # not a workaround for it.
        $deadline = (Get-Date).AddSeconds(10)
        while ($script:Phase -ne 'idle' -and (Get-Date) -lt $deadline) { Step-Machine; Start-Sleep -Milliseconds 50 }

        $task = Property 'task'
        $verdicts['no task in the session'] = ($null -eq $task)
        $listed = & $Jean tasks 2>&1 | Out-String
        $verdicts['task table empty'] = $listed -match 'no tasks match'
        Write-Output ("jean tasks says: {0}" -f ($listed.Trim() -replace '\s+', ' '))

        # Interruption, device free: synthesis is cancelled inside the engine's own
        # progress callback, so stopping before the first sample is the harder case.
        $before = $script:Stamp.Elapsed.TotalMilliseconds
        Do-PlaySentence
        $deadline = (Get-Date).AddSeconds(15)
        while ($script:Phase -ne 'speaking' -and (Get-Date) -lt $deadline) { Step-Machine; Start-Sleep -Milliseconds 50 }
        Start-Sleep -Milliseconds 1200
        $atStop = $script:Stamp.Elapsed.TotalMilliseconds
        Do-StopPlayback
        $deadline = (Get-Date).AddSeconds(20)
        while ($script:Phase -ne 'idle' -and (Get-Date) -lt $deadline) { Step-Machine; Start-Sleep -Milliseconds 50 }
        $verdicts['playback interrupted'] = $script:Phase -eq 'idle'
        Write-Output ("stop latency {0:N0} ms after pressing Stop; whole utterance {1:N0} ms" -f `
            ($script:Stamp.Elapsed.TotalMilliseconds - $atStop), ($script:Stamp.Elapsed.TotalMilliseconds - $before))
        Write-Output "transcript box still holds: $($script:Words)"
    }

    $verdicts['clean shutdown'] = Stop-JeanSession
    Write-Output ''
    Write-Output "STATE MACHINE LOG"
    $script:Log | ForEach-Object { Write-Output "  $_" }
    Write-Output ''
    $failed = @($verdicts.Keys | Where-Object { -not $verdicts[$_] })
    foreach ($k in $verdicts.Keys) { Write-Output ("{0,-24} {1}" -f $k, $(if ($verdicts[$k]) { 'PASS' } else { 'FAIL' })) }
    if ($failed.Count) { Write-Output "FAILED: $($failed -join ', ')"; exit 1 }
    Write-Output 'ALL CHECKS PASSED'
    exit 0
}

# ---------------------------------------------------------------- the window
# System.Windows.Forms is already loaded, for the trap at the top.
Add-Type -AssemblyName System.Drawing

$form = New-Object System.Windows.Forms.Form
$form.Text = "JEAN voice tester ($Mode)"
$form.ClientSize = New-Object System.Drawing.Size(460, 372)
$form.StartPosition = 'CenterScreen'
$form.FormBorderStyle = 'FixedSingle'
$form.MaximizeBox = $false
$form.Font = New-Object System.Drawing.Font('Segoe UI', 9)

$label = New-Object System.Windows.Forms.Label
$label.SetBounds(12, 10, 436, 44)
$label.TextAlign = 'MiddleLeft'
$label.Font = New-Object System.Drawing.Font('Segoe UI', 10, [System.Drawing.FontStyle]::Bold)
$form.Controls.Add($label)

$heard = New-Object System.Windows.Forms.Label
$heard.SetBounds(12, 56, 200, 18)
$heard.Text = 'Dictation'
$form.Controls.Add($heard)

# An ordinary editable multiline text box, deliberately: a dictation engine that types into
# whatever has focus -- Wispr Flow's hotkey among them -- needs a real edit control with a caret,
# a selection and the standard clipboard accelerators, and anything custom-drawn or read-only
# silently swallows the result. JEAN's own transcript lands in the same field, at the caret.
$dictation = New-Object System.Windows.Forms.TextBox
$dictation.SetBounds(12, 76, 436, 118)
$dictation.Multiline = $true
$dictation.WordWrap = $true
$dictation.ScrollBars = 'Vertical'
$dictation.HideSelection = $false
$dictation.AcceptsTab = $true
$dictation.BackColor = [System.Drawing.SystemColors]::Window
# Without this the caption above and the edit below are both named 'Dictation' to an
# accessibility client -- the edit inherits its name from the nearest label -- and a locator that
# means one of them gets two. `jean desktop act` refuses that tie rather than guessing, which is
# the right answer and a reason to name the control what it is.
$dictation.AccessibleName = 'Dictation field'
$form.Controls.Add($dictation)

function New-Button($text, $x, $y, $height) {
    $b = New-Object System.Windows.Forms.Button
    $b.Text = $text
    $b.SetBounds($x, $y, 212, $height)
    $form.Controls.Add($b)
    return $b
}
$btnCopy = New-Button 'Copy all' 12 200 30
$btnClear = New-Button 'Clear' 236 200 30
$btnStart = New-Button 'Start listening' 12 236 34
$btnStop = New-Button 'Stop listening' 236 236 34
$btnPlay = New-Button 'Play test sentence' 12 276 34
$btnKill = New-Button 'Stop playback' 236 276 34

$detail = New-Object System.Windows.Forms.Label
$detail.SetBounds(12, 316, 436, 44)
$detail.ForeColor = [System.Drawing.SystemColors]::GrayText
$form.Controls.Add($detail)

$problems = @(Get-SetupProblems)
if ($problems.Count) {
    $script:Phase = 'failed'
    $script:Status = 'Cannot start: ' + $problems[0]
    $detail.Text = ($problems -join '  |  ')
    $detail.ForeColor = [System.Drawing.Color]::Firebrick
    # The label clips and the launcher console does not: -WindowStyle Hidden still owns one,
    # and closing the window leaves the whole list in it.
    foreach ($p in $problems) { Write-Host "voice tester: $p" }
}
else {
    Start-JeanSession
}

$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 100
$timer.Add_Tick({
        Step-Machine
        $label.Text = $script:Status
        # A delivery, not a mirror. Syncing the box to $script:Words on every tick would
        # overwrite whatever the operator typed or corrected between two presses.
        if ($script:InsertWords) {
            $words = $script:InsertWords
            $script:InsertWords = ''
            Insert-Words $dictation $words
        }
        $idle = $script:Phase -eq 'idle'
        $btnStart.Enabled = $idle
        $btnPlay.Enabled = $idle
        $btnStop.Enabled = $script:Phase -in 'starting', 'listening', 'await-begin'
        $btnKill.Enabled = $script:Phase -in 'speaking', 'stopping', 'await-speak', 'await-stop'
        $voice = [string](Property 'voice')
        if ($problems.Count -eq 0) {
            $detail.Text = 'voice {0} | state {1} | task {2} | utterance {3} | nothing here submits a task' -f `
                $voice, $script:Phase, $(if ($null -eq (Property 'task')) { 'none' } else { 'present' }), `
                $(if ($null -eq (Property 'utterance')) { '-' } else { (Property 'utterance') })
        }
        $form.Text = "JEAN voice tester ($Mode) - $voice"
    })
$timer.Start()

$btnStart.Add_Click({ Do-StartListening })
$btnStop.Add_Click({ Do-StopListening })
$btnPlay.Add_Click({ Do-PlaySentence })
$btnKill.Add_Click({ Do-StopPlayback })

# The selection if there is one, the whole field if there is not: "copy the result" should not
# depend on having thought to select it first. Ctrl+C works in either case regardless.
$btnCopy.Add_Click({
        if ($dictation.SelectionLength -gt 0) {
            $dictation.Copy()
            $script:Status = "Copied $($dictation.SelectionLength) selected characters."
        }
        elseif ($dictation.Text.Length -gt 0) {
            [System.Windows.Forms.Clipboard]::SetText($dictation.Text)
            $script:Status = "Copied $($dictation.Text.Length) characters."
        }
    })
$btnClear.Add_Click({ [void]$dictation.Clear(); $dictation.Focus() })

$form.Add_Shown({
        $label.Text = $script:Status
        # Focus opens in the field, which is the whole point of it being a real edit control:
        # a dictation hotkey lands wherever the caret is, and the caret should be ready to
        # receive words the moment the window appears.
        $form.ActiveControl = $dictation
    })
$form.Add_FormClosing({
        # Closing the window is the shutdown: capture and playback stop with the worker,
        # and the worker is this window's own child, so it dies with it.
        $timer.Stop()
        [void](Stop-JeanSession)
    })

[void]$form.ShowDialog()
