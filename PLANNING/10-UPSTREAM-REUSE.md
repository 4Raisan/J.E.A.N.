# Upstream reuse plan for JEAN

The first deliverable is a Windows coding job that survives interruption, avoids replaying edits, and gives the user an independent verification result. Use the existing Rust task owner, ACP adapter, checkpoint store, and verifier. Keep each adoption small enough to compare with the current behavior.

| Upstream | Candidate and disposition |
|---|---|
| [Dodaios](https://github.com/Dodaios/Dodaios) | Study `dodaios/agent_chat/runner_cli.py` for cancel-before-kill and stall detection. Compare against JEAN's current lifecycle first. Borrow the worker activity view later. Review used a downloaded source file because the checkout was incomplete. |
| [Hearth](https://github.com/0pen-Sourcer/Hearth) | Use `hearth/acp_brain.py` and its tests as references for alternate ACP model selection and permission messages. Borrow review/apply/discard for isolated work after recovery is verified. |
| [Ari](https://github.com/DO0OG/Ari-VoiceCommand) | Adapt test scenarios from `VoiceCommand/agent/real_verifier.py` for app or browser outcomes. Keep model judgments advisory. |
| [isair/Jarvis](https://github.com/isair/jarvis) | Use the memory viewer and recall evaluations as design references for showing preference scope, evidence, correction, and forgetting. Review the noncommercial terms before any code reuse. |
| [TheJarvis-App](https://github.com/TheJarvis-App/jarvis-app) | Study `screen_windows.py` for bounded accessibility observations. Use JEAN's existing UI Automation path and add screenshot fallback only when justified by an actual failure. |
| [adewaskar/JARVIS](https://github.com/adewaskar/jarvis) | Compare `test/policy.test.mjs` and `bridge/policy.mjs` with JEAN. Adapt useful missing path, tool, and network boundary cases. |
| [JARVIS for Windows](https://github.com/AnaaySampat/jarvis-windows) | Study `actions/computer.py`, `actions/browser.py`, and `test_desktop_interventions.py` under its backend for target selection, intervention, and visible evidence. |
| [VirtualyMichael](https://github.com/VirtualyMichael/voice-assistant) | Defer. The audio endpoint checks in `voice_assistant/routing.py` are relevant only if native microphone capture returns. |
| [OpenJarvis](https://github.com/liaqateagle/OpenJarvis) | Use `tools/windows_ctl.py` and `tools/filesystem.py` to spot missing actions. Add an action only when a real workflow requires it. |
| [SrDarkoll/JARVIS](https://github.com/SrDarkoll/JARVIS) | Keep its Spotify matching and control split as a reference if the general click path fails a real Spotify workflow. Defer an app-specific adapter until then. |
| [FRIDAY](https://github.com/SAGAR-TAMANG/friday-tony-stark-demo) | Defer. `agent_friday.py` is a reference for turn-taking and interruption if conversation returns. Its service stack adds no value to the first coding workflow. |
| [JARVIS-6](https://github.com/psycoks/Jarvis) | No direct reuse selected. Browser activity display is a design reference. Verify licensing before copying any source. |

1. **Recovery and ACP.** Establish a baseline before editing. Use a disposable Git fixture and an independent check that fails before the edit and passes after it. Interrupt after a first edit, confirm uncertain work abstains, reconcile workspace evidence, and demonstrate continuation only on a supported recoverable signal. Verify exact model confirmation before any prompt. Add only the upstream-inspired test or fix that closes a reproduced gap. Compare the same scenario with Dodaios and Hearth when runnable. Separate fixture, live-provider, and installed-app evidence.
2. **Desktop actions.** Use Windows UI Automation or browser DOM for clear targets. Benchmark duplicate labels, stale dialogs, inaccessible controls, and post-action failures. Measure correct targets, wrong actions, abstentions, and time to verified result. Test a local scorer only if ambiguity remains a measured problem. Adapt Ari's and the Windows projects' observable checks where useful.
3. **Task visibility and review.** Surface worker, model, current step, checkpoint, and independent verdict using existing events. Add review/apply/discard only after isolation and stale-diff handling are proven. Prefer one readable task view before building multiple worker panes.
4. **Memory and missing tools.** Add a small view for accepted preferences and their evidence. Verify workspace isolation and explicit overrides. Add only the Windows actions required by demonstrated workflows.

Pin the upstream commit before copying, verify its license and notices for the exact files, and record provenance. Reimplement ideas in JEAN's existing Rust components when that is simpler than bringing in another runtime. Keep retired voice assets outside this work. No competitor runtime comparison is claimed until the same scenario has actually been run.
