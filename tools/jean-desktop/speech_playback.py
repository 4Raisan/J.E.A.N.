"""Play JEAN's synthesized Windows speech on the desktop output."""

import subprocess
import sys
import winsound


def main():
    cli, *options = sys.argv[1:]
    synthesis = subprocess.run(
        [cli, "speak", "--wav-stdout", *options],
        input=sys.stdin.buffer.read(),
        stdout=subprocess.PIPE,
        creationflags=subprocess.CREATE_NO_WINDOW,
        timeout=30,
    )
    if synthesis.returncode:
        return synthesis.returncode
    if not synthesis.stdout.startswith(b"RIFF"):
        raise RuntimeError("JEAN did not produce a speech WAV.")
    winsound.PlaySound(synthesis.stdout, winsound.SND_MEMORY | winsound.SND_NODEFAULT)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
