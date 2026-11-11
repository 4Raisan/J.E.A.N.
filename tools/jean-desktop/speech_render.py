"""Deterministic spoken readback for long JEAN results (project 12 approach).

Project 12 renders a trusted task result into a concise spoken reply with a
local model, preserving outcome, key names and counts. No local rendering
model is verified on this machine, so this module does the same job
deterministically: it keeps the outcome word, lifts the lines that carry
counts, identifiers and proper names, and caps the length. Every fact it
speaks is copied verbatim from the result — nothing is paraphrased or
invented. When a local Ollama model becomes available, render_prompt.py's
_delegate_result_prompt remains the upgrade path; until then this is the
honest fallback and is marked as such in VOICE_SLICE_STATUS.md.
"""
import re

SPEAK_THRESHOLD = 480          # chars; shorter results are spoken verbatim
MAX_SPOKEN_CHARS = 600
MAX_FACT_LINES = 3

_OUTCOME_FAIL = re.compile(
    r"\b(fail|failed|failure|error|denied|refused|timeout|timed out|"
    r"needs_attention|cancelled|canceled|exit(?:ed)?\s+(?:with\s+)?[1-9])\b",
    re.IGNORECASE)
_OUTCOME_OK = re.compile(
    r"\b(succeed|succeeded|success|completed|finished|verified|"
    r"exit(?:ed)?\s+(?:with\s+)?0|ok\b|done)\b", re.IGNORECASE)
# A line worth voicing: carries a count/number, an identifier-ish token
# (path, id, ALLCAPS, snake/kebab), or a quoted/proper name.
_FACT_HINT = re.compile(
    r"(\d|[A-Za-z]:\\|/|\bid\b|t-\w+|[A-Z][A-Z0-9_]{2,}|"
    r"['\"][^'\"]{2,40}['\"]|\b[A-Z][a-z]+ [A-Z][a-z]+\b|"
    r"(?i:\b(?:one|two|three|four|five|six|seven|eight|nine|ten)\s+"
    r"(?:files?|items?|tests?|records?|rows?|errors?|warnings?)\b))")


def outcome_of(text):
    """'failure', 'success' or 'unknown' — first match wins, failure first
    because a result that mentions both usually led with the problem."""
    if _OUTCOME_FAIL.search(text):
        return "failure"
    if _OUTCOME_OK.search(text):
        return "success"
    return "unknown"


def _fact_lines(text):
    lines = []
    for raw in str(text).splitlines():
        line = " ".join(raw.split())
        if not line or len(line) > 160:
            continue
        if _FACT_HINT.search(line):
            lines.append(line)
    # Shortest first keeps the spoken readback inside the cap; stable order
    # for equal lengths preserves the result's own ordering.
    return sorted(lines, key=len)[:MAX_FACT_LINES]


def render_for_speech(text):
    """A concise verbatim readback of a long result, or the text unchanged
    when it is already short enough to speak directly."""
    text = str(text).strip()
    if len(text) <= SPEAK_THRESHOLD:
        return text
    outcome = outcome_of(text)
    opener = {"success": "The task finished.",
              "failure": "The task failed.",
              "unknown": "Here is the result."}[outcome]
    facts = _fact_lines(text)
    spoken = opener + (" " + " ".join(facts) if facts else "")
    if len(spoken) > MAX_SPOKEN_CHARS:
        spoken = spoken[:MAX_SPOKEN_CHARS - 1].rsplit(" ", 1)[0] + "…"
    return spoken


def needs_readback(text):
    return len(str(text).strip()) > SPEAK_THRESHOLD
