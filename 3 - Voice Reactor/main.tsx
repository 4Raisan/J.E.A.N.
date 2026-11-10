import { useState } from 'react'
import { createRoot } from 'react-dom/client'
import { Scene } from './Scene'

import { attachOutputAnalyser, micLevel, startAnalyser, stopAnalyser } from './audio'

let player: HTMLAudioElement | undefined
let audioUrl: string | undefined
let output: Awaited<ReturnType<typeof attachOutputAnalyser>> | undefined

async function stop() {
  if (player) {
    player.onended = null
    player.pause()
  }
  if (audioUrl) URL.revokeObjectURL(audioUrl)
  const previous = output
  player = audioUrl = output = undefined
  await Promise.all([stopAnalyser(), previous?.stop()])
}

function readLevel() {
  return output?.level() ?? micLevel()
}
function App() {
  const [status, setStatus] = useState('Idle')
  const [busy, setBusy] = useState(false)

  async function selectSource(file?: File) {
    setBusy(true)
    try {
      await stop()
      if (file) {
        audioUrl = URL.createObjectURL(file)
        player = new Audio(audioUrl)
        output = await attachOutputAnalyser(player)
        player.onended = () => { void stop(); setStatus('Idle') }
        await player.play()
        setStatus('Playing audio')
      } else {
        await startAnalyser()
        setStatus('Listening to microphone')
      }
    } catch (error) {
      await stop()
      setStatus(error instanceof Error ? error.message : 'Audio source unavailable')
    } finally {
      setBusy(false)
    }
  }

  return <>
    <Scene readLevel={readLevel} />
    <div className="controls">
      <button disabled={busy} onClick={() => void selectSource()}>Enable mic</button>
      <label>Play audio <input type="file" accept="audio/*" disabled={busy} onChange={(event) => {
        const file = event.currentTarget.files?.[0]
        event.currentTarget.value = ''
        if (file) void selectSource(file)
      }} /></label>
      <button disabled={busy} onClick={() => { void stop(); setStatus('Idle') }}>Stop</button>
      <span role="status">{status}</span>
    </div>
  </>
}

createRoot(document.getElementById('root')!).render(<App />)
