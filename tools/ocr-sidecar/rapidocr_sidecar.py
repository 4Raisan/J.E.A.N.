"""RapidOCR one-shot sidecar for J.E.A.N. — stdio JSON, no server.

Protocol v1 (jean-windows::ocr::PROTOCOL_VERSION):
  request (one JSON line on stdin):
    {"version":1,"image":"<bmp>","languages":["en"],"regions":[]}
  reply (one JSON line on stdout):
    {"lines":[{"text":"...","confidence":0.5,"box":[[x,y],[x,y],[x,y],[x,y]}],...],"warning":null}
    or {"lines":[],"warning":"<reason>"}

Local only. No network. Image is a file path under the caller's retention
policy — this script never uploads anything.
"""
import json
import sys


def main() -> int:
    try:
        line = sys.stdin.readline()
        if not line:
            print(json.dumps({"lines": [], "warning": "empty request"}))
            return 0
        req = json.loads(line)
        image = req.get("image", "")
        if not image:
            print(json.dumps({"lines": [], "warning": "no image"}))
            return 0
        from rapidocr_onnxruntime import RapidOCR
        engine = RapidOCR()
        # RapidOCR returns (result, elapse); result is None when nothing found.
        result, _ = engine(image)
        lines = []
        if result:
            for box, text, conf in result:
                quad = [[int(x), int(y)] for x, y in box][:4]
                while len(quad) < 4:
                    quad.append([0, 0])
                lines.append({"text": text, "confidence": float(conf), "box": quad})
        print(json.dumps({"lines": lines, "warning": None}, ensure_ascii=False))
        return 0
    except Exception as e:  # keep machine-readable reply; never traceback to caller
        print(json.dumps({"lines": [], "warning": f"rapidocr failed: {e}"}))
        return 0


if __name__ == "__main__":
    raise SystemExit(main())
