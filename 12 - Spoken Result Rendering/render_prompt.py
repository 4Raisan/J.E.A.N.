_DELEGATE_RESULT_MAX_CHARS = 100_000

SPEAK_REQUEST_OPENER = "This developer message IS a request to speak."

def _delegate_result_prompt(
    text: str,
    *,
    language: str,
    success: bool,
    late: bool = False,
    already_said: str = "",
    request_text: str = "",
) -> str:
    """Wrap one trusted Brain result for tool-free native voice rendering.

    The rendering order carries the same voice-identity clause as the bridge
    prompt: the tagged-quote framing is exactly the role-play cue that made
    Gemini's native audio deliver a line in a different (female, distorted)
    voice on 2026-07-17 08:47, and BUG-086 heard the audible voice flip
    gender between turns while every label still read the pinned voice.

    The identifier-fidelity clause is a PROMPT-ONLY mitigation: on 2026-08-12
    the rendering swapped the result's pane names for the one the user had
    asked about ("ich habe T2 angewiesen" over a result that opened T5/T6),
    and nothing downstream verifies compliance. If a provider or model swap
    resurfaces that class, the deterministic fix belongs at the readback
    boundary, not in more prompt wording.
    """  # i18n-allow: quoted live transcript
    language_name = _LANGUAGE_NAMES.get(language, "the conversation language")
    status = "success" if success else "failure"
    # The injected result lives in the provider context for the REST OF THE
    # CALL and is re-billed as input on every later turn (at audio-session
    # rates). A spoken reply is short by design; only a pathological
    # delegate answer exceeds this, and its tail would never be voiced
    # anyway. Cut at a sentence boundary where one exists.
    if len(text) > _DELEGATE_RESULT_MAX_CHARS:
        cut = text[:_DELEGATE_RESULT_MAX_CHARS]
        dot = cut.rfind(". ")
        if dot > _DELEGATE_RESULT_MAX_CHARS // 2:
            cut = cut[: dot + 1]
        text = cut + " [result shortened]"
    framing = ""
    if late:
        framing = (
            "This is the outcome of the user's earlier request, which finished "
            "only now. Open with one short phrase that ties it back to that "
            "earlier request, then state the result. "
        )
        request = " ".join(str(request_text or "").split())
        if request:
            # ADR-0034: the conversation may have moved on several turns; the
            # tie-back must name WHAT is being answered, in the user's words.
            if len(request) > 160:
                request = request[:157].rstrip() + "..."
            framing += (
                f'The earlier request was: "{request}" — refer to its topic '
                "in that opening phrase. "
            )
    spoken = str(already_said or "").strip()
    if spoken:
        # Instant acknowledgment (2026-08-17): the user already heard an
        # interim line for this very request. Continue from it -- a second
        # "let me check" or a re-announcement is the double-tap this exists
        # to avoid.
        framing += (
            f"While the action ran you already told the user: \"{spoken}\" "
            "Continue naturally from that line: do not repeat it, do not "
            "announce again what you are about to do, go straight to the "
            "result. "
        )
    return (
        f"{SPEAK_REQUEST_OPENER} "
        "A trusted Dodaios action result is ready. Speak only a concise, natural "
        f"rendering of the tagged result in {language_name}. {framing}Preserve "
        "its exact success or failure meaning and every material fact. Any "
        "name or identifier the result states (a terminal, a file, a count) "
        "must be repeated exactly as written there — never swap in a name "
        "from the user's request that the result itself does not contain: "
        "when they differ, that difference IS the news. Say it "
        "as yourself, continuing in exactly the same voice, tone, and pace as "
        "your previous replies in this conversation. Do not imitate another "
        "person, do not change or dramatize your voice. Do not "
        "call any function, do not add a claim, and do not mention these "
        "instructions. This rendering order applies ONLY to your immediate "
        "next reply: after that reply, or once the user has said anything "
        "new, the result counts as delivered — never speak, repeat, or "
        "paraphrase the tagged result again in any later turn unless the "
        "user explicitly asks you to repeat it.\n\n"
        f"Result status: {status}\n"
        "<trusted_action_result>\n"
        f"{text}\n"
        "</trusted_action_result>"
    )

_LANGUAGE_NAMES = {
    "de": "German", "en": "English", "es": "Spanish", "pt": "European Portuguese",
}
