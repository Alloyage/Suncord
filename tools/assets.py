"""Prepare pinned browser assets. Requires fonttools and brotli."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil
import urllib.request

from fontTools import subset
from fontTools.ttLib import TTFont

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--sources", type=Path)
parser.add_argument("--manifest", type=Path)
parser.add_argument("--emoji", type=Path)
parser.add_argument("output", type=Path)
args = parser.parse_args()
root = Path(__file__).resolve().parent.parent
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=True)
manifest = json.loads((args.manifest or root / "assets/sources.json").read_text(encoding="utf-8"))
sources = json.loads(args.sources.read_text(encoding="utf-8")) if args.sources else {}

for name, source in manifest.items():
    if name not in sources:
        path = output / "downloads" / source["sha256"]
        if not path.exists():
            request = urllib.request.Request(source["url"], headers={"User-Agent": "Suncord asset builder"})
            data = urllib.request.urlopen(request, timeout=60).read()
            if hashlib.sha256(data).hexdigest() != source["sha256"]:
                raise ValueError(f"Asset checksum mismatch: {name}")
            path.parent.mkdir(exist_ok=True)
            path.write_bytes(data)
        sources[name] = str(path)
    if name.startswith(("licenses/", "sounds/")):
        target = output / name
        target.parent.mkdir(exist_ok=True)
        shutil.copyfile(sources[name], target)

(output / "fonts").mkdir(exist_ok=True)
shutil.copyfile(sources["inter"], output / "fonts/Inter.woff2")
font = TTFont(sources["jetbrains"], recalcTimestamp=False)
font.flavor = "woff2"
font.save(output / "fonts/JetBrainsMono.woff2")

# Keep sequences sharing their first codepoint together, including flags and skin tones.
sequences = {}
for line in (args.emoji or root / "assets/emoji/emojis.tsv").read_text(encoding="utf-8").splitlines():
    if line and not line.startswith("#"):
        value = line.split("\t")[0]
        sequences.setdefault(ord(value[0]), []).append(value)
chunks, codes, count = [], set(), 0
for values in sequences.values():
    if count >= 300:
        chunks.append(codes)
        codes, count = set(), 0
    codes.update(ord(char) for value in values for char in value)
    count += len(values)
if codes:
    chunks.append(codes)

(output / "emoji").mkdir(exist_ok=True)
css, assets = [], []
for index, codes in enumerate(chunks):
    font = TTFont(sources["apple"], recalcTimestamp=False)
    options = subset.Options()
    options.layout_features = ["*"]
    options.name_IDs = ["*"]
    subsetter = subset.Subsetter(options=options)
    subsetter.populate(unicodes=codes)
    subsetter.subset(font)
    font.flavor = "woff2"
    path = output / f"emoji/{index}.woff2"
    font.save(path)
    # VS16 in every face would load every chunk whenever any emoji uses it.
    ranges = ",".join(f"U+{code:X}" for code in sorted(codes - {0xFE0F}))
    version = hashlib.sha256(path.read_bytes()).hexdigest()[:16]
    css.append(f"@font-face{{font-family:'Suncord Emoji';src:url('/emoji/{index}.woff2?v={version}') format('woff2');font-display:swap;unicode-range:{ranges}}}")
    assets.append(f'("/emoji/{index}.woff2", "font/woff2", include_bytes!(concat!(env!("SUNCORD_ASSETS"), "/emoji/{index}.woff2")))')
(output / "emoji.css").write_text("\n".join(reversed(css)) + "\n")
assets.append('("/emoji.css", "text/css; charset=utf-8", EMOJI_CSS)')
(output / "emoji-assets.rs").write_text("&[\n" + ",\n".join(assets) + "\n]\n")
