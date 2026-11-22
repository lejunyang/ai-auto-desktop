#!/usr/bin/env python3
"""Offline canvas grounding using a local OWL-ViT or OWLv2 safetensors checkpoint.

Set AAD_CANVAS_VISION_MODEL to the absolute snapshot directory containing the
weights, config, processor and tokenizer (e.g. google/owlvit-base-patch32).
Set AAD_CANVAS_VISION_PYTHON to the Python executable in a local environment
with torch, torchvision, Pillow and transformers. The model runs on CPU.
AAD_CANVAS_VISION_THRESHOLD optionally overrides the default score cutoff 0.15.

The CLI embeds this CPU provider. To use a different trusted offline provider,
set AAD_CANVAS_VISION_PROVIDER to the absolute path of a local executable or
Python script implementing this same one-shot protocol:

  stdin to EOF: {"format":"png","width":640,"height":480,"data":"<base64>",
                 "description":"a hand-drawn bicycle"}
  stdout: {"objects":[{"id":"v1","kind":"visual","label":"a hand-drawn bicycle",
           "box":{"fx":0.1,"fy":0.4,"fw":0.2,"fh":0.2},"confidence":0.71}],"edges":[]}

Box origins are top-left; all four numbers are fractions of the original PNG.
Supply one short noun phrase. A successful search can return zero or multiple
matches. Confidence, when present, is the detector's score in [0, 1].
Exit codes: 2 invalid input, 3 missing runtime/model or invalid host
configuration, 4 inference failure. Run the script directly for diagnostics.
"""

import base64
import io
import json
import math
import os
from pathlib import Path
import sys


def request():
    from PIL import Image

    raw = sys.stdin.buffer.read(12_010_001)
    if len(raw) > 12_010_000:
        raise ValueError("request exceeds byte limit")
    packet = json.loads(raw)
    width, height = packet["width"], packet["height"]
    description, data = packet["description"], packet["data"]
    if (type(width) is not int or type(height) is not int
            or not (1 <= width <= 2048 and 1 <= height <= 2048)
            or width * height > 2_000_000 or packet["format"] != "png"
            or not isinstance(description, str) or not description.strip()
            or len(description.encode("utf-8")) > 1024
            or not isinstance(data, str) or len(data) > 12_000_000):
        raise ValueError("invalid canvas PNG dimensions, data or description")
    with Image.open(io.BytesIO(base64.b64decode(data, validate=True)), formats=["PNG"]) as image:
        if image.size != (width, height) or image.is_animated:
            raise ValueError("PNG dimensions or frame count do not match the request")
        return image.convert("RGB"), description


def load():
    import torch
    from transformers import (
        OwlViTForObjectDetection, OwlViTProcessor, Owlv2ForObjectDetection, Owlv2Processor,
    )

    path = Path(os.environ.get("AAD_CANVAS_VISION_MODEL", ""))
    if not path.is_absolute() or not path.is_dir():
        raise ValueError("set AAD_CANVAS_VISION_MODEL to an absolute local checkpoint directory")
    model_type = json.loads((path / "config.json").read_bytes())["model_type"]
    families = {
        "owlvit": (OwlViTProcessor, OwlViTForObjectDetection),
        "owlv2": (Owlv2Processor, Owlv2ForObjectDetection),
    }
    if model_type not in families:
        raise ValueError("supported checkpoint families are owlvit and owlv2")
    threshold = float(os.environ.get("AAD_CANVAS_VISION_THRESHOLD", "0.15"))
    if not 0 < threshold <= 1:
        raise ValueError("AAD_CANVAS_VISION_THRESHOLD must be in (0, 1]")
    torch.set_num_threads(min(os.cpu_count() or 1, 4))
    processor_cls, model_cls = families[model_type]
    processor = processor_cls.from_pretrained(path, local_files_only=True)
    model = model_cls.from_pretrained(path, local_files_only=True, use_safetensors=True).eval()
    return processor, model, threshold


def iou(a, b):
    intersection = max(0, min(a[2], b[2]) - max(a[0], b[0])) * max(
        0, min(a[3], b[3]) - max(a[1], b[1])
    )
    area = (a[2] - a[0]) * (a[3] - a[1]) + (b[2] - b[0]) * (b[3] - b[1])
    return intersection / (area - intersection)


def detect(image, description, processor, model, threshold):
    import torch
    from PIL import Image

    # Never truncate a distinguishing word from the user's description.
    window = model.config.text_config.max_position_embeddings
    if len(processor.tokenizer(description, truncation=False)["input_ids"]) > window:
        raise ValueError(f"description exceeds the checkpoint's {window}-token text window")
    # OWL-ViT's processor stretches rectangular inputs to a square. Preserve
    # object shape on wide/tall canvases and invert this padding on every box.
    side = max(image.size)
    dx, dy = (side - image.width) // 2, (side - image.height) // 2
    view = image
    if image.width != image.height:
        view = Image.new("RGB", (side, side), "white")
        view.paste(image, (dx, dy))
    inputs = processor(images=view, text=[[description]], max_length=window, return_tensors="pt")
    with torch.inference_mode():
        outputs = model(**inputs)
        result = processor.image_processor.post_process_object_detection(
            outputs=outputs, threshold=threshold, target_sizes=[(side, side)]
        )[0]

    scores, boxes = result["scores"].tolist(), result["boxes"].tolist()
    selected = []
    for index in sorted(range(len(scores)), key=scores.__getitem__, reverse=True):
        box = boxes[index]
        if not all(math.isfinite(v) for v in [scores[index], *box]):
            raise ArithmeticError("detector returned non-finite scores or boxes")
        box = tuple(max(0.0, min(1.0, (v - offset) / size))
                    for v, offset, size in zip(box, (dx, dy, dx, dy), image.size * 2))
        if box[2] <= box[0] or box[3] <= box[1] or any(iou(box, other) > 0.7 for other, _ in selected):
            continue
        selected.append((box, scores[index]))
        if len(selected) == 64:
            break
    objects = [
        {"id": f"v{i}", "kind": "visual", "label": description, "confidence": score,
         "box": {"fx": left, "fy": top, "fw": right - left, "fh": bottom - top}}
        for i, ((left, top, right, bottom), score) in enumerate(selected, 1)
    ]
    return {"objects": objects, "edges": []}


def main():
    if sys.argv[1:] == ["--help"]:
        print(__doc__)
        return 0
    for key in ("HF_HUB_OFFLINE", "TRANSFORMERS_OFFLINE", "HF_HUB_DISABLE_TELEMETRY",
                "HF_HUB_DISABLE_IMPLICIT_TOKEN"):
        os.environ[key] = "1"
    try:
        image, description = request()
    except ImportError as exc:
        print(f"missing image runtime: {exc}", file=sys.stderr)
        return 3
    except Exception as exc:
        print(f"invalid image request: {exc}", file=sys.stderr)
        return 2
    try:
        processor, model, threshold = load()
    except Exception as exc:
        print(f"cannot load local detector: {exc}", file=sys.stderr)
        return 3
    try:
        result = detect(image, description, processor, model, threshold)
    except ValueError as exc:
        print(f"invalid model input: {exc}", file=sys.stderr)
        return 2
    except Exception as exc:
        print(f"inference failed: {exc}", file=sys.stderr)
        return 4
    sys.stdout.buffer.write(json.dumps(result, allow_nan=False, ensure_ascii=False,
                                       separators=(",", ":")).encode("utf-8"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
