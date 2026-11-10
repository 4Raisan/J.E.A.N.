import { useMemo } from 'react'
import { Canvas, useFrame } from '@react-three/fiber'
import {
  EffectComposer,
  Bloom,
  ChromaticAberration,
  Vignette,
  Noise,
} from '@react-three/postprocessing'
import { BlendFunction } from 'postprocessing'
import * as THREE from 'three'
import { Core } from './Core'
import { Particles } from './Particles'
const accent = "#00e5ff"
const AMP_LIVE = 0.085

/**
 * Everything the three children animate from, in one mutable object.
 *
 * These used to be render-time props, which had two consequences. The scene
 * subscribed to mic level, so the entire r3f tree reconciled sixty times a
 * second for numbers that never leave useFrame; and because they were only
 * delivered on a re-render, they were delivered late or — before the analyser
 * starts and `level` is a constant zero — not at all, which is why the ignition
 * screen never picked up its own spin rate.
 *
 * Nothing here belongs in React state. The scene is updated by mutation and
 * React only ever mounts it: Rig writes this object once per frame, the
 * children read it in their own useFrame. Children subscribe to the frame loop
 * before their parent does, so they are reading the previous frame's values —
 * one frame of lag on quantities that are all lerped anyway, and invisible.
 */
export type Drive = {
  color: THREE.Color
  /** 0..1 smoothed loudness, with an idle breath under it. */
  level: number
  /** Rotation multiplier for the drifting dust. */
  spin: number
  /** Target displacement amplitude for the core surface. */
  amp: number
  /** 0..1 power-up reveal — the ring assembles outwards from the centre. */
  open: number
  /**
   * The reactor slice of the ui state, already resolved and smoothed.
   *
   * Everything here is a no-op at UI_DEFAULTS: colour follows the phase exactly
   * as it always did, and the three multipliers sit at one.
   */
  reactor: {
    /** ui.reactor.color if set, otherwise the accent. */
    color: THREE.Color
    scale: number
    intensity: number
    /** Multiplier on the ring's own rotation rate. */
    spin: number
    /** 0 ring, 1 sphere, 2 wire — a number because it ends up in a uniform. */
    style: number
    visible: boolean
  }
}

/**
 * A colour target that only re-parses when the string actually changes.
 *
 * The colours in the ui slice are arbitrary CSS written by JARVIS, and
 * THREE.Color.set logs a warning for anything it cannot parse. Setting one
 * unconditionally per frame turns a single typo into sixty console lines a
 * second, which drowns everything else out during a demo.
 */
function Rig({ readLevel }: { readLevel: () => number }) {
  const drive = useMemo<Drive>(
    () => ({
      color: new THREE.Color(accent),
      level: 0,
      spin: 1.1,
      amp: AMP_LIVE,
      open: 0,
      reactor: {
        color: new THREE.Color(accent),
        scale: 1,
        intensity: 1,
        spin: 1,
        style: 0,
        visible: true,
      },
    }),
    [],
  )
  useFrame((state, dt) => {
    drive.open = 1.6
    const level = Math.max(0, Math.min(1, readLevel()))
    const idle = 0.12
    // Idle breathing so the orb is never completely still.

    const breathe =
      (Math.sin(state.clock.elapsedTime * 0.9) * 0.5 + 0.5) * idle
    const want = Math.max(level, breathe)
    drive.level += (want - drive.level) * Math.min(1, dt * 9)

    // Slow drift on the camera keeps handheld-ish life in the shot.
    const t = state.clock.elapsedTime
    state.camera.position.x = Math.sin(t * 0.13) * 0.35
    state.camera.position.y = Math.cos(t * 0.17) * 0.22
    state.camera.lookAt(0, 0, 0)
  })

  // Nothing in here is lit: both the core and the dust are raw ShaderMaterials,
  // which do not read the light list. The scene therefore has no lights at all.
  //
  // The tilted gyro rings that used to sit here are gone. Steeply tilted arcs
  // crossing in front of a complete circle do not read as depth — they read as
  // scratches on the lens, and as broken circles in a design whose whole
  // subject is one unbroken one.
  return (
    <>
      <Core drive={drive} />
      <Particles drive={drive} />

    </>
  )
}

export function Scene({ readLevel }: { readLevel: () => number }) {
  return (
    <Canvas
      className="scene"
      camera={{ position: [0, 0, 6.2], fov: 45 }}
      gl={{ antialias: true, alpha: true }}
      dpr={[1, 2]}
    >
      <Rig readLevel={readLevel} />
      {/*
        multisampling={0} on purpose. The default is 8, which allocates a
        half-float MSAA target — at dpr 2 that is a 3200x1800 buffer — and there
        is not one polygon edge in this scene for it to smooth: everything is
        additive blobs, point sprites and lines, all of which are already
        soft-edged by their own falloff and then blurred again by bloom.

        Tone mapping is also deliberate, if less obviously so. EffectComposer
        forces gl.toneMapping to NoToneMapping while it is mounted (there is no
        prop for it), so the additive output clips instead of rolling off. That
        hard clip to white is the neon look this piece wants — the sweep and the
        rim are meant to blow out. If you ever want the filmic roll-off back,
        add a <ToneMapping mode={ToneMappingMode.ACES_FILMIC} /> effect at the
        end of this chain rather than touching the renderer.
      */}
      <EffectComposer multisampling={0}>
        {/* Bloom is what turns additive lines into "hologram". */}
        <Bloom
          intensity={1.15}
          // A higher threshold keeps the mid-tones intact so the orb doesn't
          // flatten into a solid white disc.
          luminanceThreshold={0.22}
          luminanceSmoothing={0.85}
          mipmapBlur
          radius={0.72}
        />
        <ChromaticAberration
          offset={new THREE.Vector2(0.0009, 0.0012)}
          radialModulation={false}
          modulationOffset={0}
        />
        <Noise opacity={0.035} blendFunction={BlendFunction.OVERLAY} />
        <Vignette eskil={false} offset={0.22} darkness={0.95} />
      </EffectComposer>
    </Canvas>
  )
}
