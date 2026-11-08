# Voice assets: what JEAN needs to speak and listen, and what it does not have

Scope: Phase 5 native speech (`workers/voice`). This document is the **only**
place that names download URLs for speech. Nothing in this repository's build
fetches any of it.

## 0. What is already true without any of this

| Build | Speech | Audio devices |
|---|---|---|
| default (`jean`, no flags) | none loaded, no worker started | none opened |
| `[voice] enabled = true`, `backend = 'fixture'` | a test double that returns one configured string | a simulated endpoint |
| `[voice] enabled = true`, `backend = 'sherpa_onnx'` | **refused** with the key names to fix | refused with it |
| `--features sherpa`, assets absent | compiled, and **refuses to link** naming `sherpa-onnx-c-api` | as above |
| `--features sherpa`, archive extracted and `SHERPA_ONNX_LIB_DIR` set, ASR bundle present | **links (one `LNK4098`) and recognises real audio** — measured 2026-11-07 (§7 tier 3) | real endpoints still need §9 step 1, but `voice.offline_wav` runs this same engine with **no device at all** (§7 tier 3a) |
| the same, with `RUSTFLAGS=-C target-feature=+crt-static` | **links with no linker message at all** and imports no Visual C++ runtime DLL (§61.4) | as above |

`jean-voice` ships with the `sherpa` feature **off**. The dependency is now a real
optional dependency (`dep:sherpa-onnx`), not a commented line: with the feature off
Cargo never builds it, `Cargo.lock` merely carries its graph, and the default
workspace build is verified `--offline --locked`. Enabling the feature is still an
explicit act, because the `-sys` build script wants a 122 MB native archive unless
provisioning is declared first (§1).

The refusal is deliberate: `assets::recognition` and `assets::synthesis` are
measured separately, so a machine with only the ASR bundle recognises and says so,
while synthesis refuses and names its own keys; the worker's label for the fixture
engine says `fixture (no inference, no device)`. No placeholder is reported as
working speech.

Three commands need no microphone and no speaker, and none of them is a reason to
open one:

* `jean voice doctor` — JSON: feature state, per-bundle asset presence, the two
  provisioning environment variables as the worker sees them, and the hotkey. It
  does not enumerate endpoints, so a routine diagnostic never touches audio.
* `jean voice transcribe clip.wav` — recognises a **prerecorded** file through the
  same `EngineFactory`/`Recognizer` path a live press uses, feeding it in 100 ms
  packets like a WASAPI callback would. It is how a speech bundle is validated
  against a known phrase before anyone speaks into a microphone.
* `jean voice synthesize out.wav "phrase"` — renders through the configured engine
  into a WAV and reports the levels it measured before encoding. No endpoint is
  opened, so the file is for the operator to play. `--phonemes` takes input already
  in the bundle's own token alphabet, which reaches the acoustic model when the
  lexicon that §4a describes is absent — and says `input: "phonemes"` so a report
  cannot mistake one for the other.

Run `jean voice doctor` to get all of that as JSON.

## 1. Native library (code, not weights)

| | |
|---|---|
| Archive | `sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2` |
| URL | <https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.1/sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2> |
| Release | tag `v2.0.1`, published `2026-10-26T15:24:29Z` (GitHub API, measured) |
| Size | 122,863,596 bytes (GitHub API, measured) |
| SHA-256 | `sha256:23e7b986b197eda0bbc47ff3d7d5c87afd6ef2b9077214ba26b0f1ec52ef0e66` (GitHub API `digest`, measured) |
| Code licence | Apache-2.0 (`sherpa-onnx` and `sherpa-onnx-sys` 2.0.1 crate manifests, read from the published sources) |
| Link mode | static (`features = ["static"]`), which asks the linker for `sherpa-onnx-c-api` |

**What has been measured here, and what has not.** The archive above is now **on
this machine, checksum-verified against the published digest, and linked**. The
bytes matched `sha256:23e7b986…f0e66` exactly, so the paragraph below, which was
written when nothing had been seen, is now history rather than a caveat:

| Measurement | Result |
|---|---|
| What `jean-voice.exe` links today | imports `VCRUNTIME140.dll` + `api-ms-win-crt-*.dll` — the **dynamic** CRT (`/defaultlib:msvcrt` on rustc's link line) |
| A `/MT` static library linked into that binary | **links**, and `link.exe` emits `LNK4098: defaultlib 'LIBCMT' conflicts with use of other libs`; the probe runs |
| The same library linked with `-C target-feature=+crt-static` | links with no conflict; the produced exe imports **no** CRT DLL (1,674,752 bytes vs 1,598,464) |
| Feature on, no provisioning | `error: could not find native static library 'sherpa-onnx-c-api'` |
| **Feature on, real archive extracted (2026-11-07)** | **links, `[exit] 0`, with exactly the predicted `LNK4098` and nothing else**; 12 `.lib` files, 1,057,860,400 bytes, `jean-voice.exe` 24,688,128 bytes, still importing `VCRUNTIME140.dll` |
| **The real archive, `-C target-feature=+crt-static` (2026-11-07, §61.4)** | **`[exit] 0`, zero linker messages**, 25 DLL imports → **12**, no `VCRUNTIME140*` and no `api-ms-win-crt-*` left, 25,115,136 bytes (+375,808); the same three clips and the same supervised suite pass on this executable |

So the statement the probe predicted and the real link confirmed: the `-MT` archive
**does link** against today's default-CRT build, with an LNK4098 warning, and the
residual risk is not a failed link — it is two CRTs each with their own heap and
locale state sharing one process. Three clips have now been recognised through that
boundary (§7) with no heap fault and no leak-related abort, which is *observed
behaviour under a small load*, not a proof of cross-boundary safety. The alternative
is no longer a probe: `RUSTFLAGS=-C target-feature=+crt-static` was built against
the real archive and **run** — same transcripts, same supervised cancellation
suite, one CRT in the process, no redistributable dependency, and no linker message
to explain (§61.4). It costs a full rebuild of the feature-on tree (110 crates,
3 m 21 s at `-j 2`), so it is a deliberate switch, not a default, and both
executables are kept side by side rather than one replacing the other.

**How the build script behaves when provisioning is missing** — both branches
executed, and **neither downloaded anything**:

```text
SHERPA_ONNX_ARCHIVE_DIR=<dir without the archive>
  → SHERPA_ONNX_ARCHIVE_DIR does not contain expected archive:
    <dir>\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2

SHERPA_ONNX_LIB_DIR=<path that is not a directory>
  → SHERPA_ONNX_LIB_DIR does not exist or is not a directory: <path>
```

`target/sherpa-onnx-prebuilt/` was left at 0 bytes by both runs. Only with
*neither* variable set does the script reach its `ureq` download — which is why
provisioning is declared before the feature is enabled, and why §6 writes the two
variables into the same commands that turn the feature on. The crate's graph is in
`Cargo.lock` either way; resolution is not the risk, the archive is.

To compile `sherpa.rs` against the real crate without any binaries, take the
script's own documented skip: set `DOCS_RS=1` and use `cargo check`. That is how
the adapter was type-checked in §59, and it is compile-only evidence — it proves
the signatures, not the symbols.

## 2. Recognition bundle (streaming ASR)

| | |
|---|---|
| Archive | `sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2` |
| URL | <https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2> |
| Size | 506,956,414 bytes — **matched by the supplied file** (measured against the real bytes 2026-11-07) |
| SHA-256 | **not published** for this asset (`digest` is empty on the release API). Computed locally on the supplied file: `455f40e556aa2b20ac9d3bffd603b58002075c1193b4070938540c11efe0a4da`. This is *our* hash of *this* download, recorded so a later copy can be compared against it; it is not a checksum the publisher has confirmed, and it does not upgrade into one. |
| Model-weight licence | Apache-2.0, per the model card shipped with the bundle (`license: apache-2.0`), naming `marcoyang/icefall-libri-giga-pruned-transducer-stateless7-streaming-2023-04-04` as the source model |
| Training data | LibriSpeech and GigaSpeech. Those datasets have their own terms; the Apache-2.0 grant above is the grant on **the exported weights**, and is the thing JEAN relies on |
| Engine rate | 16 kHz mono — set `voice.sample_rate = 16000` |
| Extracted | `%LOCALAPPDATA%\JEAN\models\sherpa-onnx-streaming-zipformer-en-2023-06-21\` — 13 files, 546,279,902 bytes, every one byte-identical in length to its archive entry |

Files used, with the config key each one fills:

| File in the unpacked bundle | Config key |
|---|---|
| `encoder-epoch-99-avg-1.int8.onnx` | `voice.models.encoder` |
| `decoder-epoch-99-avg-1.onnx` | `voice.models.decoder` |
| `joiner-epoch-99-avg-1.int8.onnx` | `voice.models.joiner` |
| `tokens.txt` | `voice.models.tokens` |

The int8 encoder and joiner are the ones sherpa's own example script uses; the
fp32 decoder is unchanged. Mixing int8 and fp32 across the three files is fine —
it is the same exported model — but all three must come from **this one
directory**, because a mismatched `tokens.txt` silently produces the wrong words.

### 2b. Offline alternative: Parakeet TDT 0.6B v2 INT8 (identified, not provisioned)

The pinned `sherpa-onnx` 2.0.1 Rust crate and its matching native tree **can** run NVIDIA's
English Parakeet TDT 0.6B v2. There is no `tdt` field on `OfflineModelConfig`, and that is a false
clue: the model is loaded through the ordinary offline **transducer** fields plus
`model_type = "nemo_transducer"`, which is both what the crate's own doc example does and what
upstream's CLI for this asset says.

| | |
|---|---|
| Asset | `sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2` |
| Download | `https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8.tar.bz2` |
| Size | 482,440,461 bytes compressed, ≈661 MB extracted |
| Checksum | the GitHub release publishes **none** (`digests: null`); verify the extracted files against the mirror's LFS SHA-256 at `https://huggingface.co/csukuangfj/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8` — `encoder.int8.onnx` `92a10bd9…57ca8b` (652,282,309 B), `decoder.int8.onnx` `4d097bae…020f41` (7,257,753 B), `joiner.int8.onnx` `77a29f3e…56f428` (1,739,080 B), `tokens.txt` (9,384 B, not an LFS object) |
| Destination | `%LOCALAPPDATA%\JEAN\models\asr\sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8\` |

It does not work yet, and not because of the library: the worker builds an `OnlineRecognizer`
only ([`workers/voice/src/sherpa.rs`](file:///C:/Users/4Raisan/Desktop/MIII/Personal%20Projects/J.E.A.N.%20-%20The%20AI%20Companion/workers/voice/src/sherpa.rs)),
and a TDT export has no streaming cache inputs for it. Two changes are needed — an
`OfflineRecognizer` pass fed from the buffered press at `EndCapture`, and config keys for a second
model family, since `voice.models.encoder/decoder/joiner/tokens` already name the streaming bundle
and `VoiceConfig` denies unknown fields. A selector defaulting to streaming keeps **this** zipformer
the default and the comparison honest; `IMPLEMENTATION_STATUS.md` §65.4 has the detail and
`target/p5u-compare.ps1` is the harness that runs the moment both halves exist.

## 3. Voice activity detection (optional)

| | |
|---|---|
| File | `silero_vad_v5.onnx` |
| URL | <https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad_v5.onnx> |
| Size | 2,313,101 bytes (GitHub API, measured) |
| SHA-256 | **not published** for `silero_vad_v5.onnx`. `silero_vad.onnx` (643,854 bytes) does have `sha256:9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6`, and `silero_vad.int8.onnx` (212,860 bytes) has `sha256:c36d490aff5ab924ca6c7aeec4d8f6bd3d22db6fa7611b9c5b17eae58ac3a20` |
| Model-weight licence | MIT — `snakers4/silero-vad` repository licence, read from the GitHub API |
| Config key | `voice.models.vad` |

Two hard constraints the code enforces, not recommends:

* Silero v5 is **16 kHz only**, with a **512-sample** window. `SherpaEngine`
  skips the VAD whenever `voice.sample_rate != 16000`, rather than feeding it
  audio it cannot interpret. Recognition still works; silence is simply not
  trimmed.
* Without `voice.models.vad` the worker still recognises speech. VAD changes
  what gets decoded, not whether speech is possible.

## 4. Synthesis bundle (TTS) — **provisioned; engine proven; text proven since the lexicon**

### 4a. Selected: `kokoro-en-v0_19`

| | |
|---|---|
| Archive | `kokoro-en-v0_19.tar.bz2` |
| URL | <https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-en-v0_19.tar.bz2> |
| Size | 319,625,534 bytes (GitHub API `size`, measured) |
| SHA-256 | `sha256:912804855a04745fa77a30be545b3f9a5d15c4d66db00b88cbcd4921df605ac7` (GitHub API `digest`, measured) |
| int8 variant | `kokoro-int8-en-v0_19.tar.bz2`, 103,248,205 bytes, `sha256:c9f0dd393615805b0bab050c340834d5e684e732aec91c0e860cd30e982c08bd` |
| Source model | `hexgrad/Kokoro-82M`, revision `f3ff3571791e39611d31c381e3a41a3af07b4987` |
| Exported-weight licence | **Apache-2.0** — from the source model's own `license` field on the Hugging Face API, not inferred from a dataset pointer |
| Config keys | `tts_model`, `tts_voices`, `tts_tokens`, `tts_lexicon`, `tts_speaker` |
| Path shape | what the **extracted bundle and the 2.0.1 headers** say, measured 2026-11-07: `model.onnx` + `voices.bin` + `tokens.txt` + `LICENSE` + `README.md` + `espeak-ng-data/`, and **no lexicon file anywhere in it**. `OfflineTtsKokoroModelConfig` in the vendored `sherpa-onnx-2.0.1/src/tts.rs:162` carries `model, voices, tokens, data_dir, length_scale, dict_dir, lexicon, lang`, with `data_dir` documented as *"Deprecated. Ignored since v2.0.0. Use `lexicon` … instead"*. So the upstream example the earlier version of this row cited is not the shape 2.0.1 accepts: the lexicon is required, and the `espeak-ng-data` directory the archive ships is the front end 2.0.1 stopped using. |

**The asset gap this creates, stated exactly.** With `tts_model`, `tts_voices`,
`tts_tokens` and `tts_dict_dir` configured, the 2.0.1 engine loads all three model
files and then refuses at the text-to-phoneme step:

```
--kokoro-lang is deprecated and ignored in sherpa-onnx >= v2.0.0. espeak-ng is no longer used
No lexicon available. Please provide --kokoro-lexicon
Failed to convert '…' to token IDs
```

`voice.models.tts_lexicon` is the key that closes it, and no such file exists on
this machine or inside the supplied archive — so **synthesis of arbitrary text is
provisioned except for one asset**, and `jean voice doctor` now lists
`voice.models.tts_lexicon` as absent rather than reporting the bundle as ready.
What *is* proven without it is the acoustic model: `jean voice synthesize
--phonemes` feeds `GenerationConfig::phoneme_codepoints` (the documented
alternative to a lexicon, `tts.rs:435`) with symbols taken from the bundle's own
177-token `tokens.txt`, which exercises model + voices + inference + WAV encoding
and nothing else. That is a synthesis *engine* result, not a text-to-speech result.

### 4a-ii. The text front end that closes it — installed, verified, and speaking

**Source: upstream pairs exactly this bundle with exactly one file.** At tag
`v2.0.1` — the same version as the installed native library —
`rust-api-examples/examples/kokoro_tts_en_lexicon.rs` sets `model`, `voices` and
`tokens` to `./kokoro-en-v0_19/…`, `lexicon: Some("./lexicon-en-us.txt")`,
`length_scale: 1.0`, and passes **no `dict_dir` and no `lang`**.
`dart-api-examples/tts/run-kokoro-en-lexicon.sh` does the same thing and shows where
the file comes from: it downloads `kokoro-en-v0_19.tar.bz2` **and**
`lexicon-en-us.txt` from the same `tts-models` release, side by side. So this is not
a guessed compatibility; it is the vendor's own English Kokoro recipe for the bundle
this machine already has.

| | |
|---|---|
| Asset | `lexicon-en-us.txt` |
| Link | <https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/lexicon-en-us.txt> |
| Size | **6,305,773 bytes** (GitHub API `size`, measured 2026-11-07) |
| Published checksum | **`sha256:8b67ae549a465eee78f92ecb74644ad90c3c58ce65f19c68baf85878a10ef73a`** (GitHub API `digest`) |
| Destination | `%LOCALAPPDATA%\JEAN\models\tts\lexicon-en-us.txt` |
| Config key | `voice.models.tts_lexicon` = that absolute path |
| Layout (its own header) | `# Lexicon: word(s) \|\| space-separated phones`, `# Generated by build_lexicon.py`, `# Single-word exceptions (CMUDict + en_list + compounds): 167907`, `# Multi-word entries: 690`; body lines look like `kaldi \|\| k ˈ æ l d i` |
| Licence lineage | The header declares the table built from **CMUdict** (BSD-2-Clause, attribution required) plus an `en_list` and compound entries. The release ships **no licence file next to this asset**, so the grant is stated by the file itself rather than by a document beside it — say so plainly rather than calling it Apache because the model is. |
| Licence-anchored alternative | `lexicon-us-en.txt` inside `kokoro-multi-lang-v1_0.tar.bz2` — 349,886,476 bytes, `sha256:60703707bc05ccd48d60e1eb723f38b07d6e474795a0b6a588d7220400e0f7a4`, a bundle that does carry a `LICENSE`, generated in-repo by `scripts/kokoro/v1.0/generate_lexicon_en.py` (Apache-2.0, © Xiaomi). Costs a 350 MB download to obtain one 6 MB text file. The same file exists raw on Hugging Face (`csukuangfj/kokoro-multi-lang-v1_0/lexicon-us-en.txt`, 5,956,885 bytes) but publishes only a git blob oid, no sha256. |

**Compatibility with the model's tokens, measured on the whole file.** Kokoro consumes
phonemes **one character at a time** — the bundle's `tokens.txt` is a 177-glyph character
table, and sherpa's own generator writes `" ".join(list(phones))` — so the question is per
glyph, not per phone token. `target/p5q-check-lexicon.py` on the complete 6,305,773-byte
asset: **168,654 entries, all parsing as `word || phones`; 53 glyphs used; 51 inside
`tokens.txt`; 2 outside** — `U+0303 COMBINING TILDE` in 13 entries (`accouchement`, `blanc`,
`croissant`, `croissants`, …) and `U+0327 COMBINING CEDILLA` in 2 (`dolgellau`, `utrecht`) —
**15 of 168,654 entries, 0.0089 %**. An unknown glyph is a word the model cannot render and
sherpa answers it `Failed to convert … to token IDs`, so the honest statement is that these
fifteen (French and Welsh proper nouns) are refused, not that the table is clean. An earlier
version of this section quoted 46 glyphs and 1 outsider: that was a bounded 64 KB read, and a
prefix **understates** both numbers — which is why the checker prints `PARTIAL` instead of
scoring a truncated file silently.

**Provision it, then prove it.** These are the commands; the repository does not download the
asset. They were run by the operator on 2026-11-07 and the result is §7 tier 3c — kept here as
the recipe, with the paths corrected to the pair that actually works together.

```powershell
$m = "$env:LOCALAPPDATA\JEAN\models\tts"
curl.exe -sSL -o "$m\lexicon-en-us.txt" `
  https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/lexicon-en-us.txt
# Expect 6305773 bytes and 8B67AE549A465EEE78F92ECB74644AD90C3C58CE65F19C68BAF85878A10EF73A
(Get-Item "$m\lexicon-en-us.txt").Length
(Get-FileHash -Algorithm SHA256 "$m\lexicon-en-us.txt").Hash
# Alphabet check against the installed bundle (run from the repository root):
python target\p5q-check-lexicon.py "$m\lexicon-en-us.txt"
#   expect: matches published True · lexicon entries 168654 · glyphs outside model 2
#   (15 entries: the combining tilde and cedilla cases). 0 would mean a different file.
# Then add one line under [voice.models] in target\p5o-data\jean.toml:
#   tts_lexicon = 'C:\Users\4Raisan\AppData\Local\JEAN\models\tts\lexicon-en-us.txt'
$env:JEAN_DATA_DIR     = "$PWD\target\p5o-data"
$env:JEAN_VOICE_WORKER = "$PWD\target\p5q\bin\jean-voice-sherpa-dynamic.exe"
.\target\debug\jean.exe voice doctor        # tts_lexicon must read present: true
.\target\debug\jean.exe voice synthesize "$PWD\sentence.wav" `
  "The quick brown fox jumps over the lazy dog."        # no --phonemes: this is text
python target\p5q-measure-wav.py "$PWD\sentence.wav"    # re-read it; do not trust one report
```

**Two paths in that block are load-bearing, and one of them is a trap.** Use the *current*
`target\debug\jean.exe`, not `target\p5p\bin\jean-default-cli.exe`: the preserved §61.4 CLI
predates `jean interact --tty`, so it cannot show the operator surface at all. And use a worker
rebuilt **after** the §62.3 config key: `VoiceConfig` is `deny_unknown_fields`, so
`target\p5p\bin\jean-voice-sherpa-dynamic.exe` answers the current CLI with
`unknown field 'fixture_utterance_ms'` and exit 1 (`IMPLEMENTATION_STATUS.md` §63.3). A
preserved worker is only runnable as a pair with the schema it was built against.

Before the asset, **ordinary text was refused**, and the refusal was the correct behaviour, not
a defect: `jean voice doctor` listed `voice.models.tts_lexicon` as absent and `synthesize` said
so by key name rather than loading a model it could not use. With it, ordinary text reaches the
engine: **2,845 ms of 24 kHz mono audio for the sentence above, peak 0.395, rms 0.057, 10,457
zero crossings, `silent: false`, generated in 4,912 ms of inference** — §7 tier 3c.
`--phonemes` (§61.3) stays available and is no longer the only route to the acoustic model.

This satisfies the rule that blocked TTS before: the grant is on the weights, it
is explicit, and it is not a dataset licence standing in for a derived model's
terms. The bundle has now been **supplied by the operator, verified against the
digest above, extracted, and loaded by the real engine** (§61) — the Apache-2.0
`LICENSE` inside it is the same grant, not a pointer. What remains between this
and a spoken answer is one asset (§the block above) and one physical act: playing
a generated clip, which is the operator's, not the repository's.

One open item belongs to the operator, not to the code: the Apache-2.0 grant
covers the weights, and the **shared** voice names inside `voices.bin` are a
separate creative question if a recording is ever redistributed. Nothing JEAN
synthesizes here is redistributed by this project.

### 4b. Rejected: `vits-piper-en_US-amy-low`

| | |
|---|---|
| Archive | `vits-piper-en_US-amy-low.tar.bz2` |
| URL | <https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/vits-piper-en_US-amy-low.tar.bz2> |
| Sizes / digests | `amy-low` 67,095,344 bytes `sha256:c70f5284a09a7fd4ed203b39b2ff51cac1432b422b852eb647b481dade3cf639`; `amy-low-int8` 21,099,246 bytes `sha256:93070ac9fadf512e56c46bdd0c5d2ce96b424fdc4e683d560167410bd2c4df7d`; `amy-low-fp16` 35,850,818 bytes `sha256:aa6d0a048e5442d1d4ccf20eef4db895ad5faff72f1189f3dbd8ae026640fdfb` (all GitHub API, measured) |
| Lexicon | `lexicon-en-us.txt`, 6,305,773 bytes, `sha256:8b67ae549a465eee78f92ecb74644ad90c3c58ce65f19c68baf85878a10ef73a` → `voice.models.tts_lexicon` |
| Engine rate | 22,050 Hz for `amy-low` (the model reports its own rate; the worker resamples to the endpoint) |


**Licence status: not established, and therefore not shipped as a default.**
The `amy` model card shipped in the bundle says:

> Dataset — URL: <https://github.com/MycroftAI/mimic3-voices> — License: **See URL**
> Training: finetuned from U.S. English Ryan voice (low quality)

That is the exact case the batch rule names: a dataset pointer is not a
redistribution grant for the weights, and "See URL" resolved to a repository is
not a licence for a derived fine-tune whose base model has its own terms. Nothing
here has been checked about the *derived* weights, so this document does not
claim Apache, MIT or CC for them.

Option 2 is what §4a did. If `amy` or any other Piper voice is preferred for
quality reasons, the gap above has to be closed first and recorded here; the
`rhasspy/piper-voices` dataset returns **HTTP 401 to an anonymous API request**,
which is itself a reason not to build a default on it — a licence a machine
cannot read is a licence an operator has to take on trust.

The code path **does** depend on which engine wins, which is why
`voice.models.tts_voices` exists: `SherpaSynthesizer` reads `tts_model`,
`tts_tokens` and `tts_lexicon` for both, additionally requires `tts_voices` for
Kokoro, and passes `tts_speaker` as `GenerationConfig::sid`. Setting
`tts_voices` is the switch — neither engine can infer the bundle type from a
filename, and `OfflineTts::sample_rate()` still decides the rate the render
thread resamples from.

## 5. Required directory layout

The layout is yours to choose; these are absolute paths the keys must point at.
`%LOCALAPPDATA%\JEAN\models` is what `config/defaults.toml` documents as home.

```
%LOCALAPPDATA%\JEAN\
├── models\
│   ├── sherpa-onnx-streaming-zipformer-en-2023-06-21\
│   │   ├── encoder-epoch-99-avg-1.int8.onnx
│   │   ├── decoder-epoch-99-avg-1.onnx
│   │   ├── joiner-epoch-99-avg-1.int8.onnx
│   │   └── tokens.txt
│   ├── vad\silero_vad_v5.onnx
│   └── tts\
│       ├── kokoro-en-v0_19\            ← selected (§4a)
│       │   ├── model.onnx
│       │   ├── voices.bin
│       │   ├── tokens.txt
│       │   ├── LICENSE                 ← the Apache-2.0 grant, inside the bundle
│       │   ├── README.md
│       │   └── espeak-ng-data\         ← shipped, and unused by 2.0.1 (§4a)
│       └── vits-piper-en_US-amy-low\   ← rejected on licence (§4b)
│           ├── en_US-amy-low.onnx
│           └── tokens.txt
└── native\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib\
    └── lib\                      ← SHERPA_ONNX_LIB_DIR points here (12 .lib files)
```

The Kokoro tree above is **the archive's own contents**, verified file by file
after extraction: there is no `lexicon-en-us.txt` in it. That file belongs to the
VITS path (§4b, 6,305,773 bytes). A Kokoro run needs its own lexicon, pointed at
by `voice.models.tts_lexicon`, and §4a records what the engine says when it is
absent.

The `-lib` archive ships **only** `lib\` — there is no `include\` tree in it, which
is fine because the Rust wrapper declares the C API itself rather than generating
bindings at build time. What the linker actually receives is 12 libraries totalling
1,057,860,400 bytes, of which `onnxruntime.lib` alone is 944,135,846: the static
runtime is inside the executable, so no `onnxruntime.dll` has to sit beside
`jean-voice.exe`.

The C run time is the one thing that does *not* follow from that, and it is now
measured (§61.4). Built the ordinary way, `jean-voice.exe` imports
`VCRUNTIME140.dll` and `VCRUNTIME140_1.dll` plus the `api-ms-win-crt-*` set,
because Rust links the shared CRT while these libraries were compiled `/MT`; that
mismatch is what `LNK4098` reports. Built with `RUSTFLAGS=-C
target-feature=+crt-static` the warning disappears and the import list drops to 12
system DLLs, so the worker needs no Visual C++ redistributable at all. Both
executables are kept and both pass the same native tests.

Each of those four ASR files and each synthesis file goes into its own
`voice.models.*` key; `jean voice doctor` reports each as present or absent, so
the layout above is a convention, not a requirement the code enforces.

`voice.models.native_lib_dir` is a **diagnostic** field: a static link needs no
runtime library directory at all, so it is checked for existence and reported,
not loaded. It matters only if the dependency is switched to
`features = ["shared"]`, in which case the DLLs must be beside `jean-voice.exe`
or on `PATH` before the process starts.

## 6. Provisioning steps (run these yourself; the repository will not)

```powershell
# 1. Native library archive, into a directory you control.
#    DONE ON THIS MACHINE 2026-11-07 from C:\Users\4Raisan\Downloads: digest
#    matched, 12 .lib files extracted (1,057,860,400 bytes).
$dest = "$env:LOCALAPPDATA\JEAN\native"
New-Item -ItemType Directory -Force $dest | Out-Null
curl.exe -sSL -o "$dest\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2" `
  https://github.com/k2-fsa/sherpa-onnx/releases/download/v2.0.1/sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2
# Verify before unpacking. Expect 122863596 bytes.
(Get-Item "$dest\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2").Length
Get-FileHash -Algorithm SHA256 "$dest\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2"
# Expected: 23E7B986B197EDA0BBC47FF3D7D5C87AFD6EF2B9077214BA26B0F1EC52EF0E66
tar -xf "$dest\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib.tar.bz2" -C $dest

# 2. Recognition bundle. Expect 506956414 bytes; no digest is published, so
#    record the hash you get and put it in section 2 above — done: this machine's
#    download measured 455f40e5…efe0a4da and extracted 13 files byte-exact.
$models = "$env:LOCALAPPDATA\JEAN\models"
curl.exe -sSL -o "$env:TEMP\zipformer.tar.bz2" `
  https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-streaming-zipformer-en-2023-06-21.tar.bz2
Get-FileHash -Algorithm SHA256 "$env:TEMP\zipformer.tar.bz2"
tar -xf "$env:TEMP\zipformer.tar.bz2" -C $models

# 3. VAD (optional). Expect 2313101 bytes.
curl.exe -sSL -o "$models\silero_vad_v5.onnx" `
  https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad_v5.onnx

# 4. Synthesis bundle — the one selected in §4a. Expect 319625534 bytes and
#    sha256 912804855a04745fa77a30be545b3f9a5d15c4d66db00b88cbcd4921df605ac7.
#    DONE ON THIS MACHINE 2026-11-07 from C:\Users\4Raisan\Downloads: size and
#    digest matched the published values, 360/360 files extracted
#    byte-consistently (369,315,617 bytes), and the archive's own LICENSE is
#    Apache-2.0. What the archive does NOT contain is a lexicon — see §4a.
New-Item -ItemType Directory -Force "$models\tts" | Out-Null
curl.exe -sSL -o "$env:TEMP\kokoro.tar.bz2" `
  https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models/kokoro-en-v0_19.tar.bz2
Get-FileHash -Algorithm SHA256 "$env:TEMP\kokoro.tar.bz2"
tar -xf "$env:TEMP\kokoro.tar.bz2" -C "$models\tts"
# 103248205 bytes / c9f0dd393615805b0bab050c340834d5e684e732aec91c0e860cd30e982c08bd
# is the int8 variant, if 319 MB is more than you want.
```

Then, in this repository:

1. Nothing to edit. `workers/voice/Cargo.toml` already carries
   `sherpa = ["dep:sherpa-onnx"]` and the pinned optional dependency; the old
   "uncomment this line" instruction was replaced in §59 and the manifest is the
   authority, not this document.
2. Point the build at the extracted `lib` directory and **nothing else**, so no
   fetch path exists:
   ```powershell
   $env:SHERPA_ONNX_LIB_DIR = "$env:LOCALAPPDATA\JEAN\native\sherpa-onnx-v2.0.1-win-x64-static-MT-Release-lib\lib"
   ```
   If you would rather it unpack a local archive than use a directory, set
   `SHERPA_ONNX_ARCHIVE_DIR` to the folder containing the `.tar.bz2`. Either
   variable removes the network path; **neither set means it downloads**, which is
   why step 1 is manual.
3. `target\dev.cmd build -p jean-voice --features sherpa -j 2`
   (add `--offline --locked` if you want the build to refuse any new package).
   Outcomes, with the one this machine took marked:
   * **taken, 2026-11-07 — success, `[exit] 0`, one linker message:
     `LNK4098: defaultlib 'LIBCMT' conflicts with use of other libs`.** No
     `__imp_` symbol errors, no `/LIBPATH` error, no second command needed. The
     produced `jean-voice.exe` is 24,688,128 bytes against the 1,665,536-byte
     default build.
   * `could not find native static library 'sherpa-onnx-c-api'` — the directory
     is not the one the archive unpacked to;
   * a `link.exe` failure naming unresolved `__imp_` symbols — the CRT question in
     §1, where the alternative is
   `$env:RUSTFLAGS = "-C target-feature=+crt-static"`, which `jean-voice` was
   measured to build **and run** with (2026-11-07, §61.4): `[exit] 0`, **no linker
   message of any kind**, 12 DLL imports instead of 25, and no Visual C++ runtime
   DLL in the list. It costs a full rebuild of the feature-on tree to switch, so
   both executables are kept and one is named per run rather than one replacing the
   other.
4. Fill the `voice.models` keys with the absolute paths from section 5, set
   `voice.backend = 'sherpa_onnx'`, and confirm with `jean voice doctor`.
   `recognition_assets` must all be `present: true`; `synthesis_assets` may stay
   false and recognition will still work, because the two are provisioned
   separately. `engine.label` becomes
   `sherpa-onnx 2.0.1 (streaming transducer ASR; synthesis engine per config)`,
   `sherpa_feature_linked` becomes `true`, and `native_provisioning` prints the
   `SHERPA_ONNX_LIB_DIR` path the worker process actually inherited.
   To validate without touching your own `jean.toml`, keep the file in a scratch
   directory and run with `$env:JEAN_DATA_DIR` pointed at it — that directory both
   supplies `<data_dir>\jean.toml` and receives anything the run writes, so
   `%LOCALAPPDATA%\JEAN` stays as you left it.
5. Prove recognition against a known phrase **without a microphone** (§7 tier 3).
   The ASR bundle ships its own clips with published transcripts, so nothing needs
   recording first:
   ```powershell
   jean voice transcribe "$env:LOCALAPPDATA\JEAN\models\sherpa-onnx-streaming-zipformer-en-2023-06-21\test_wavs\0.wav"
   ```
   `test_wavs/trans.txt` holds the expected text for `0.wav`, `1.wav` and `8k.wav`.

6. Prove synthesis **without a speaker**, the same way (§7 tier 3b). Two commands,
   two different results, and the difference is the lexicon:
   ```powershell
   jean voice synthesize "$PWD\hello.wav" "hɛlˈoʊ wˈɜːld" --phonemes
   jean voice synthesize "$PWD\words.wav" "hello world"
   ```
   The first renders through the loaded Kokoro model and reports the levels it
   measured (`sample_rate`, `duration_ms`, `peak`, `rms`, `silent`, and
   `input: "phonemes"` so a log cannot be read as text-to-speech); the second
   refuses with `voice.models.tts_lexicon` named. Both exit non-zero if the written
   file is actually silence — "a file appeared" is not the claim.

## 7. Live validation, in the tiers the code is separated into

Each tier is evidence for itself and proves nothing about the next.

**Tier 1 — device-free, runs today (`cargo test`).**
`workers/voice/tests/speech_path.rs` drives `capture_once`/`speak_once` with
scripted endpoints: format conversion for f32/i16/i32/u8, stereo averaging,
refusal of a partial frame, resampling continuous across packet boundaries,
48 kHz→16 kHz before the recognizer, the capture buffer dropping oldest frames
and counting them, device loss surfacing as `DeviceLost` rather than silence,
an unsupported endpoint failing the open before any transcript is claimed, a stop
during generation never opening a speaker, a stop during the final flush
producing no transcript, and every utterance ending by stopping the endpoint.
`workers/voice/src/wav.rs` covers the RIFF reader: 16-bit and 32-bit-float,
extensible headers, stereo averaging, 24-bit and ADPCM refused by name, and a
file with no `data` chunk. `crates/jean-core/tests/interaction.rs` covers PTT
through the real worker process, spoken controls, announcements, and four
separate cancellation scopes. `apps/jean-cli/tests/cli_voice.rs` covers the
config keys, the two asset lists, hotkey grammar, budget-vs-deadline rejection,
transcribe with a real WAV, and `JEAN_NO_VOICE`.

**Tier 2a — native compile.** Achieved. `cargo check -p jean-voice --features
sherpa` with the build script's own skip set resolves the real 2.0.1 crate,
compiles `sherpa.rs` against it, and found one genuine error of mine
(`model_config.joiner` → `model_config.transducer.joiner`). Clippy is clean
under `-D warnings` in this configuration. This proves the signatures and
nothing about symbols, accuracy or the archive.

**Tier 2b — native link.** **Achieved 2026-11-07, first attempt, no build-scoped
fix needed for the link itself.** With `SHERPA_ONNX_LIB_DIR` at the extracted `lib`
directory, `cargo build -p jean-voice --features sherpa --offline --locked -j 2`
exits **0**; the only linker message is `LNK4098` (§1). `cargo clippy` in the same
configuration with `-D warnings` is also 0. The archive's published digest matched
its bytes, and every one of the 12 `.lib` files came out at the size the archive
declared. That warning was then **removed, not explained away**: the same build with
`RUSTFLAGS=-C target-feature=+crt-static` emits **no linker message at all**, drops
`VCRUNTIME140.dll`, `VCRUNTIME140_1.dll` and the whole `api-ms-win-crt-*` set from
the import table, and passes the same recognition and cancellation tests (§61.4).
This proves symbols resolve; it says nothing about accuracy, which is tier 3, or
about long-session CRT behaviour.

**Tier 3 — model inference, no hardware.** **Achieved with real inference on all
three clips the ASR bundle ships, and all three now transcribe completely.**
`jean voice transcribe` fed each file through the native engine in 100 ms packets
and printed this against `test_wavs/trans.txt`:

| Clip | Audio | Expected (bundle `trans.txt`) | Actual, before §61.2 | Actual, now | Result |
|---|---|---|---|---|---|
| `0.wav` | 6.625 s @ 16 kHz | `…THE SQUALID QUARTER OF THE BROTHELS` | identical, all 20 words | identical, all 20 words | exact |
| `1.wav` | 16.715 s @ 16 kHz | `…ON THAT SAME DISHONOURED BOSOM…` | `DISHONORED`, 38/39 | `DISHONORED`, 38/39 | 38/39 |
| `8k.wav` | 4.825 s @ **8 kHz** | `…LESS WITH HOPE THAN APPREHENSION` | ended `…THAN APPREHENS` | **all 8 words** | **fixed** |

The 8 kHz loss was first blamed on the linear resampler (§60.6) and that was
**wrong**: a scipy-resampled 16 kHz copy of the same clip truncated identically, so
the resampler was excluded, and appending 100 ms of silence recovered the word — the
cause is the transducer needing roughly 0.4 s of trailing audio to emit its last
token. The fix is a 0.5 s tail pad in `finish()` (`workers/voice/src/sherpa.rs`),
and the two clips that never needed it still transcribe as before. The earlier
fp32/int8 agreement still stands and still excludes quantisation. Wall clock per
clip stays ~2–4 s including model load.
With `backend = 'fixture'` the command still answers and its `engine` field says
the transcript came from a test double — so a transcript that looks right is not
by itself proof of inference.

**Tier 3a — inference through the live worker, still no hardware.** **Achieved
2026-11-07, and it closed the gap this section used to describe as tier-4-only.**
`voice.offline_wav` outranks the backend derivation, so a real engine runs inside
the **supervised** worker against a file: `workers/voice/tests/native_session.rs`
(feature-gated, skips with a printed reason when the bundle or the native executable
is absent) measures press-open 30–76 ms, **28 pings answered during a 7 s native
decode with the slowest at 0–2 ms**, release→transcript 72 ms, and an expired press
refused as `stale push-to-talk activation` with no transcript delivered — plus the
independent-task witness in §61.5. Cancellation during native *synthesis* remains a
tier-1 fixture observation, because `Speak` takes text and text still needs the
lexicon (§4a).

**Tier 3b — synthesis, no speaker.** **Achieved for the engine, and for text as of tier 3c.**
`jean voice synthesize --phonemes` rendered `hɛlˈoʊ wˈɜːld` through Kokoro at
24,000 Hz / 1.03 s, peak 0.332, 57 % of samples above −30 dBFS, verified from the
written file by a separate program. The same command *without* `--phonemes` used to refuse
naming `voice.models.tts_lexicon`, which the supplied bundle does not ship; that refusal is
what §4a-ii provisioned away, so the seam now proves the engine while the text path proves the
product (§61.3).

**Tier 3c — ordinary text, no speaker, no phoneme seam.** **Achieved 2026-11-07 with the
lexicon installed** (§4a-ii, `IMPLEMENTATION_STATUS.md` §63.5):
`jean voice synthesize <wav> "The quick brown fox jumps over the lazy dog."` — `"input": "text"`,
**68,283 frames at 24,000 Hz = 2,845 ms, peak 0.395 (−8.1 dBFS), rms 0.057, `clipped: false`,
`silent: false`**, re-read independently by `target/p5q-measure-wav.py` (10,457 zero crossings,
35.8 % above −30 dBFS) so the claim does not rest on the engine's own report. Inference took
4,912 ms for 2,845 ms of audio — ≈1.7× real time, measured after model construction, and the
number that sets how long an operator waits between `/say` and the first sound. What this tier
still does not cover: intelligibility (a listening judgement, §9 step 4) and the fifteen lexicon
entries whose glyphs this model cannot render (§4a-ii).

**Tier 4 — physical microphone and speaker.** Nothing here was executed by this
repository. §9 is the procedure, in the order that produces evidence without
wasting a run.

## 8. What this build does not do, stated plainly

* **No always-on listening and no wake word.** Capture begins only on an explicit
  `BeginCapture` and ends on release or `voice.capture_timeout_ms`. A wake word
  would need a trained keyword model, which D-013 rules out for pretrained
  openWakeWord weights (CC BY-NC-SA).
* **No acoustic barge-in.** `Interrupt` stops playback through a control message.
  It does not listen for your voice while the speaker is busy, and nothing in the
  pipeline measures whether you spoke over it. Two different things, and only the
  first is implemented.
* **Ambient speech authorises nothing.** A transcript becomes a task only through
  an explicit `SubmitTranscript` for the activation that produced it, and a spoken
  phrase can only *report* or *cancel* — never start work.
* **Synthesis is offline, not streamed.** `synthesize` returns a whole utterance
  and the render thread then writes it. Cancellation aborts generation through the
  callback and stops the endpoint, but a five-sentence answer is not heard before
  it is finished.
* **Resampling is linear.** The device→engine path interpolates linearly in
  `audio.rs`. sherpa's own `LinearResampler` exists and is unused. What this is
  responsible for is now bounded by measurement rather than by assumption: the
  8 kHz clip that lost its final word in §7 loses it **identically when the file is
  resampled by scipy instead of by `audio.rs`**, so the linear resampler was not
  the cause and is not exonerated of anything either — it remains an unverified
  quality risk, tested only for this one symptom. A speech quality complaint about
  the resampler should still be tested against sherpa's implementation.
* **Native inference now has a device-free path.** `voice.offline_wav` (§9 step 3a)
  outranks the backend derivation: set, the worker reads that WAV instead of any
  endpoint and renders to a counted sink instead of any speaker, so a real engine
  runs a whole press-release-transcript cycle with nothing recorded and nothing
  played. That is how native cancellation was measured (§7 tier 3a) rather than
  deferred. Unset — the shipped default — real endpoints are used, which is the
  only thing an operator can actually speak to.
* One utterance slot, one capture, one task — by design. Announcements are opt-in
  precisely because they take that slot from you.

## 9. Operator procedure for the physical test

This is the part the repository deliberately cannot do: opening a microphone,
playing audio through a speaker, and speaking. Run it in this order — each step
either produces a recorded result or tells you which prerequisite is still
missing, and no step depends on the one after it.

**State as of 2026-11-07 (fourth pass).** Steps 0 and the file-based half of Step 3
are closed with real inference: the native archive, the ASR bundle and the Kokoro
bundle are extracted and digest-verified, `--features sherpa` links in both CRT
flavours, all three published clips transcribe, and the synthesis engine renders
audio to a file (§7 tiers 2b, 3, 3a, 3b). Native *cancellation* is no longer a
Step-6-only observation either — it was measured against the supervised worker
without a device (`IMPLEMENTATION_STATUS.md` §61.5). Two things changed at §62 that
belong to this procedure rather than to the code notes: **Step 4 is pressable now**
(`jean interact --tty`, `/stop`), and **the session can be pointed at the preserved
native worker** (`JEAN_VOICE_WORKER`), which is what makes Steps 2–5 runnable against
a build that actually speaks. **And the last prerequisite outside the physical plane is
gone:** the Kokoro lexicon is installed, digest-verified and configured (§4a-ii), so
ordinary text synthesizes (§7 tier 3c) and Step 4 is now an audible test rather than a
refusal. What remains in this procedure is exactly what the repository cannot do:
holding a key, listening, and judging whether the words were the words. Steps 2–5 have a
window form now (Step 1b) for the two of those that need a human either way.

Before you record your own phrase, run the pre-flight. It costs seconds and it
separates "a microphone problem" from "an engine problem" — and if you built the
native worker somewhere other than `target\debug`, say which executable you mean,
because `cargo build --workspace` does not preserve it (§61.4):

```powershell
# Which worker to use. Optional; omit to take the sibling of jean.exe.
$env:JEAN_VOICE_WORKER = "$PWD\target\p5p\bin\jean-voice-sherpa-dynamic.exe"

# Recognition, through the same recognizer a live press uses.
$w = "$env:LOCALAPPDATA\JEAN\models\sherpa-onnx-streaming-zipformer-en-2023-06-21\test_wavs"
jean voice doctor                   # sherpa_feature_linked, per-asset presence
jean voice transcribe "$w\0.wav"    # expect the first line of test_wavs\trans.txt
jean voice transcribe "$w\8k.wav"   # expect …THAN APPREHENSION, not …APPREHENS

# Synthesis, to a file, with no speaker opened. This one needs no lexicon because
# the phrase is already in the bundle's own token alphabet; play the WAV yourself.
jean voice synthesize "$PWD\hello.wav" "hɛlˈoʊ wˈɜːld" --phonemes
#   expect: sample_rate 24000, duration_ms ≈ 1030, silent false, peak ≈ 0.33
jean voice synthesize "$PWD\words.wav" "hello world"
#   expect: a refusal naming voice.models.tts_lexicon — that is the asset, not a bug
```

**Step 0 — what you are about to test.**

```powershell
jean voice doctor
```
Read `sherpa_feature_linked`, `native_provisioning`, `recognition_assets` and
`audio_backend` before touching a device. `audio_backend == "Fixture"` means no
microphone will be opened no matter what you say, and the run proves nothing.

**Step 1 — devices, read-only.**

```powershell
jean voice devices
```
Copy one `capture` id and one `render` id out of the JSON into `voice.input_device`
and `voice.output_device`; **empty is also a valid choice** and means the system
default, which is the configuration the pre-flight above was run against. Set them
only if the default endpoint is not the one you intend to speak into. This command
only enumerates: it activates nothing, and it is the one voice verb that is safe to
run on a machine you have not provisioned.

**Step 1b — or do Steps 2–5 with a mouse, in a window.**

`tools\voice-tester\` holds a double-clickable tester for exactly this procedure's physical
plane: *Start listening* (which shows `Starting microphone…` until the worker confirms the
stream opened, then `Listening — speak now.`), *Stop listening* (`Transcribing…`, then the words
in the box), *Play test sentence*, and *Stop playback*, which interrupts during synthesis as
well as during audible playback. Closing the window shuts down the session and the worker it
started.

The box under the status line is **Dictation**, and it is an ordinary editable text control: JEAN's
own transcript is placed at the caret rather than overwriting what you typed, so you can correct
it, and an external dictation engine — Wispr Flow's hotkey included — has a real focused edit
control to write into. *Copy all* copies the selection, or the whole field when nothing is
selected. Nothing in this window submits what the field holds; reviewed text becomes a goal only
through the task UI, as its own action. The field's edit, read-back and copy were verified from
another process through `jean desktop act`/`inspect` plus the clipboard (`IMPLEMENTATION_STATUS.md`
§65.2) — the last step, your hotkey on your foreground window, is the physical one.

| Launcher | What it touches |
|---|---|
| `JEAN Voice Tester (offline).cmd` | nothing: `config\offline\jean.toml` names a WAV as the microphone and the speaker is a counted sink, while the real zipformer and Kokoro engines do the work. Rehearse here. |
| `JEAN Voice Tester.cmd` | your microphone and speaker: `config\mic\jean.toml`, same everything else |

Both resolve their paths from the launcher's own folder and set `JEAN_DATA_DIR` plus
`JEAN_VOICE_WORKER` themselves, so `%LOCALAPPDATA%\JEAN\jean.toml`, its preferences and its task
database are not read or written by a tester run — the `jean.db` that appears next to the tester's
own config belongs to the tester. It cannot submit a task: the seven owner commands it can send
are `status`, `begin_capture`, `end_capture`, `clear_capture`, `speak`, `interrupt`, `shutdown`,
and the words it hears are read out of `snapshot.transcript`, which is the transcript-only path
(`IMPLEMENTATION_STATUS.md` §64.2). Expect about 6 seconds between opening the window and
`Ready.` — that is the recognizer, Kokoro and the 6.3 MB lexicon loading — then about 0.24 s from
pressing Start to `Listening`, about 0.3 s from releasing the press to the words, and the
transcript-only discard is what keeps that round trip from becoming a task.
It needs a `target\debug\jean.exe` built from this tree; if the window says the CLI is older
than the tester, rebuild it (§64.4).

**Step 2 — turn the plane on, and open the session that owns playback.**

```powershell
jean preferences set voice.enabled true     # or leave the isolated config's own setting
$env:JEAN_DATA_DIR   = "$PWD\target\p5o-data"
$env:JEAN_VOICE_WORKER = "$PWD\target\p5q\bin\jean-voice-sherpa-dynamic.exe"
jean interact --tty
```

`JEAN_NO_VOICE` still wins over the file, by design, so unset it if you exported it earlier.
Two things about that last line are worth understanding before you press a key. It is the
**session** that owns playback, so it is the session you interrupt (Step 4); plain
`jean interact` is the same session with a machine-facing JSON-lines face. And naming the
worker is not decoration: without `JEAN_VOICE_WORKER` the session launches `jean-voice.exe`
sitting next to `jean.exe`, which is the feature-less default build, and a workspace build
replaces that sibling silently (`IMPLEMENTATION_STATUS.md` §61.4, §62.4). A value that is not
a file stops the command with its name in the message instead of starting a session whose
voice mysteriously never becomes ready.

**Step 2b — the first microphone test cannot start work, and here is why.** Before you speak
a phrase you have not scored, check the one setting that decides whether a transcript becomes
a task:

```powershell
jean model status      # expect provider = none in this isolated configuration
```

With `provider = none`, `jean plan`, `jean do` and a submitted goal all refuse at the planning
gate before any tool, window, file or network is touched — that refusal is a recorded Phase-2
behaviour, not a hope. So the first press is safe in a way that does not depend on what you
say. If you would rather it also not be *submitted*, say exactly `status` with
`voice.spoken_controls = true` **while the session owns a task**: the phrase is matched exactly
after normalisation (`status`, `task status`, `report status`, `what is the status`), honoured
only while a task is active, and it can report or cancel — never start work
(`crates/jean-core/src/speech.rs:29`). Note the asymmetry that makes the second option weaker
here than on a machine with a model: with no task active, a spoken control does not match, and
the phrase falls through to the goal path that provider `none` then refuses.

**Step 3 — a known phrase, push-to-talk.**

**Make the first press the harmless one.** Hold `voice.hotkey` (default
`Ctrl+Alt+Space`), say `status`, release, and read the transcript back. A status phrase is the
only spoken request whose worst case is a report, and Step 2b explains the two independent
reasons it cannot turn into work in this configuration. Score it as a transcription result,
not as a control result: the sentence you get is what the recognizer heard, and whether it
*acted* as a control depends on `voice.spoken_controls` and on a task being active.

Then decide the real phrase in advance and write it down; the expected text is part of the
result. Speak it once, release. `jean voice ptt` does the same thing on demand without an
interactive session. Record:

| Field | Why it is in the record |
|---|---|
| expected phrase | the only way to score a transcript |
| actual transcript | the result itself |
| release→transcript seconds | the finalize budget and the decode cost, together |
| device ids | so a later difference can be attributed to hardware |
| `dropped` frames, if reported | a bounded buffer that overflowed is a different failure from a model that misheard |

**Step 3a — the same press, with no microphone, for anyone who wants to see it
before holding a key.** `voice.offline_wav` names a WAV instead of an endpoint and
turns the render side into a counted sink. The supervised suite already uses that
shape (`target/p5p-offline/jean.toml` is an isolated configuration that sets it to
the bundle's own `1.wav`), and it is how the native timings in §7 tier 3a and
§61.5 were taken:

```powershell
$env:JEAN_NATIVE_WORKER_EXE = "$PWD\target\p5q\bin\jean-voice-sherpa-dynamic.exe"
cargo test -p jean-voice --features sherpa --offline --locked -j 2 --test native_session -- --nocapture
```

Each test prints the executable it used, the model-load time, the ping count
answered during a real 7-second decode, and the release→transcript latency. With
`JEAN_NATIVE_WORKER_EXE` unset, or the bundle absent, they skip and say why — a
green run there means nothing about native inference, which is the same rule
`backend = 'fixture'` follows everywhere else in this document. Prefer the worker named
above over the §61.4 copies in `target\p5p\bin` because those were built against the
pre-§62.3 schema, and the contract between a config sender and this worker is
`deny_unknown_fields` — measured from the CLI, where the current `jean voice doctor`
pair produced `unknown field 'fixture_utterance_ms'` and exit 1 (§63.3). The same
mismatch applies to any harness serializing a current `VoiceConfig`; it was not
separately re-run here, because rebuilding the native test binary is outside this
batch's budget. Rebuild the preserved copies with
`RUSTFLAGS=-C target-feature=+crt-static` when you want the LNK4098-free variant
again.

**Step 4 — playback and interruption by command.** **Both halves exist now, and this one makes
sound.** The interrupt has been pressable since §62.2, and since §63 an ordinary sentence
synthesizes, so `/say` in the session below is real Kokoro audio through your default output
device — not silence into a counter, and not a refusal. Read `audio_backend` from
`jean voice doctor` first: with `target\p5o-data` it is `System`, which means exactly that. If
you want to see the same control land without any speaker, use the device-free configuration in
Step 3a's shape (`offline_wav` set) rather than this one.

In the `jean interact --tty` session from Step 2, ask for something long enough to hear and
stop it mid-sentence. Expect **roughly five seconds between `/say` and the first sound** for a
two-to-three-second sentence: generation runs at ≈1.7× real time on this CPU (§7 tier 3c) and
happens on the worker's synthesis thread, so the session keeps answering `/status` throughout —
that is the design, and Step 4 is where you see it.

```text
/say Report the status of the backup job and the three open approvals.   → playback starts
/stop                                                                    → the speaker stops
state: … utterance=- …                                                   → printed without typing
```

`/stop` with no argument interrupts the utterance this session says it owns;
`/stop <id>` sends that id to the existing identity check, so an id that already ended is
answered `refused: stale utterance` and the newer playback is untouched — worth pressing once
on purpose, because that refusal is the whole reason the interrupt cannot stop something it
does not own. Nothing is playing, `/stop` → `refused: nothing is playing to interrupt`; the
session does not invent an id to make the command look successful. Record the utterance id from
the `ok: utterance N submitted` line, the elapsed time from `/say` to the `utterance=-` state
line, and whether a task that was running at the time is still running afterwards: interruption
reaches playback and nothing else, which is Step 5's subject.

**Measure this separately from barge-in.** What this step tests is that a *control message*
stops playback. Speaking over the microphone while the speaker is busy is a different
capability (§8), is not implemented, and nothing in JEAN listens while it talks — so do not
score Step 4 by whether your voice interrupted it, and do not report it as if it did.

For the device-free version of the same claim, with no microphone, no speaker and a measured
number, see `target/p5r-tty-demo.py` (`IMPLEMENTATION_STATUS.md` §62.3): 2,091 ms of playback
uninterrupted against 233 ms cut short, and a stale id that refused while the newer utterance
ran its full 2,105 ms.

**Step 5 — status and cancellation during a long-running task.** A task this *session* owns can
only exist if a model can plan one, so with `provider = none` (Step 2b) start the long task from
the registry instead, in a second window pointed at the same isolated data directory:

```powershell
jean exec phys.sleeper --goal "a task to look at while voice is exercised"
jean tasks                       # copy the task id and its state
```

`target/p5o-data/tools.toml` registers that tool for this procedure; it sleeps for 60 seconds
and verifies its own output, and it is the only entry there. Then, from the tty session:

```text
/status                                  → voice, utterance, task, shutting_down, rejected_events
/stop                                    → playback stops; the state line still shows the task
jean cancel <task-id>                    → the runner is told, its process tree dies
jean tasks                               → state cancelled, and it stays cancelled
```

A spoken `cancel the task` (or `cancel`, `stop task`, `stop the task`) reaches
`CancelTask` only through the session's own task slot with `voice.spoken_controls = true`, which
is why the registry route above is the one that works with no model: a spoken phrase can report
or cancel a task the session owns and can never start work. Record the cancel latency
(request → state `cancelled`), and confirm the voice worker stayed answerable throughout — ping
it with `/status` while the task is running and again after the cancel. The measured fixture cost
of keeping inference off the control loop is in `IMPLEMENTATION_STATUS.md` §58.2 — an interrupt
round trip of ~22 ms rather than ~0.25 ms — and Steps 4 and 5 together are where you find out what
it costs with real inference in the loop.

**Step 6 — the exits that are not success.** A task that survives a voice
failure, a voice worker that survives a task failure, and a device unplugged
mid-capture should each produce a specific, reported outcome rather than a hang.
If any of them hangs, capture the exact command, the wall-clock timings and
whether `jean voice doctor` still answers, and keep it — a reproducible
cancellation hang is the single finding that would justify replacing this
speech path, and it needs evidence rather than a description.

**What to report afterwards.** Which steps ran, the expected/actual phrase
pairs, the two latencies, and for anything that did not run, the exact missing
prerequisite. A successful link, a successful model load and a correct
transcript are three different results; do not let one be read as the others.
