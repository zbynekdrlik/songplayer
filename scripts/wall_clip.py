"""SongPlayer's wall measurement clips (#272).

The owner (14.10.2026): whether Resolume puts out every frame, in order, must
be measured, never judged by eye. These clips carry their own frame index in
a form a capture of the LED wall can read back on every refresh, whatever
part of the picture the wall shows: SNV shows about a quarter of the height
and, since the operator's mapping of 18.10.2026, only the middle half of the
width (the clip at ~2x). The operator's mapping is the truth and is never
changed (#259), so the clip is read under ANY zoom of its width and height.

Every frame carries its 0-based clip index:

- **A Gray-code strip**, the primary code: ``CELLS`` equal cells. The outer
  two at each end are fixed references (black outside, white inside); the 16
  between them are the index in reflected Gray code, most significant bit
  first, white = 1, black = 0. Two consecutive indices differ in exactly ONE
  cell, so a boundary that blends two frames (#251's 25 / 24 fps blend on the
  30 fps grid) shows exactly one grey cell, and its level between the
  reference black and white IS the blend weight.
- **Copies across the width (v2).** Each band holds ``COPIES`` self-contained
  copies of the strip side by side, each with its own references. A copy is
  20 of the frame's 192 cell widths and the copies touch, so any window of
  40 cells holds a whole one: any quarter of the width (48 cells), at any
  offset, holds a whole copy with 8 cells to spare.
- **A ruler over each copy (v2).** A static row band above each copy's strip:
  cells 0 and 19 white, cells 1-18 black, in every frame. The strip's own
  references cannot place a copy: with copies side by side, the bit cells of
  most indices also form the ``W B B W`` around a junction at a wrong phase
  (863 of the 1800 indices of a 30 fps clip, #272 comment 6281619930). No
  run of bit cells draws 18 black cells between two white ones, and a ruler
  row is never read as a strip (its "black" references are white), so the
  reader takes a copy's place and cell width from its ruler and reads the
  bits in the strip right under it.
- The strip, the ruler and the index in digits are repeated in each of the
  ``BANDS`` horizontal bands, so any quarter of the height holds a whole
  band.
- **The index in digits** (and the clip's fps), in each copy's bit cells
  above its ruler, for the eye, dim grey on the background (too faint for
  the reader to take a glyph for a ruler or a strip). Drawn with a 5x7
  bitmap font in numpy: no font file, no PIL, so the drawing code also runs
  in CI's eval env.
- No QR code: the ``qrcode`` package is not in CI's eval env, and the strip
  already gives the exact index (#272 dispatch, lane 1).

The output is deterministic: the frames are pure functions of the index,
written as raw yuv420p (luma 16 / 235 / 40 in BT.709 limited range, chroma
128), encoded by libx264 on one thread with fixed parameters and a silent
48 kHz stereo AAC track (SongPlayer's test item import splits a clip into its
video and a FLAC of its audio, exactly as for camera-box's clip), muxed with
``+bitexact`` and no metadata. The same ffmpeg gives the same bytes; the
script prints each file's size and sha256, which SongPlayer pins
(``crates/sp-server/src/test_item_clips.rs``).

Usage::

    python scripts/wall_clip.py --out DIR [--fps 24 25 30] [--seconds 60]
                                [--size 3840x2160] [--ffmpeg ffmpeg]
"""

from __future__ import annotations

import argparse
import contextlib
import hashlib
import subprocess
import sys
import tempfile
from dataclasses import dataclass
from pathlib import Path

import numpy as np

CLIP_VERSION = 2
CLIP_SECONDS = 60
FPS_SET = (24, 25, 30)
WIDTH = 3840
HEIGHT = 2160

BITS = 16
REF_CELLS = 2
CELLS = BITS + 2 * REF_CELLS
BANDS = 8
COPIES = 9
# A cell is width // CELL_DIV px wide (20 at 3840): the copies take
# COPIES * CELLS = 180 of the 192 cells, 6 cells of margin on each side.
CELL_DIV = 192

Y_BLACK = 16
Y_WHITE = 235
Y_BACKGROUND = 40
# The digits stand only 40 over the background, under the reader's 25 % of
# full scale (64): no glyph edge can read as a ruler or a strip (#272 v2: at
# 200, glyph strokes 4x scaled formed both, a copy reading index 0).
Y_DIGIT = 80
C_NEUTRAL = 128

# Inside a band, as fractions of its height: the digits, the ruler, the
# strip. The ruler's gap to its strip is half the ruler's height; above it
# the digits end 10 % of the band earlier, and under the strip the next
# band's digits start 30 % later, so the reader never pairs other rows as a
# ruler and its strip (wall_frames.locate).
DIGITS_TOP = 0.06
DIGITS_BOTTOM = 0.30
RULER_TOP = 0.40
RULER_BOTTOM = 0.48
STRIP_TOP = 0.52
STRIP_BOTTOM = 0.76

AUDIO_RATE = 48_000
X264_PRESET = "veryfast"
X264_CRF = 16

FONT_W = 5
FONT_H = 7
FONT = {
    "0": ("01110", "10001", "10011", "10101", "11001", "10001", "01110"),
    "1": ("00100", "01100", "00100", "00100", "00100", "00100", "01110"),
    "2": ("01110", "10001", "00001", "00010", "00100", "01000", "11111"),
    "3": ("11111", "00010", "00100", "00010", "00001", "10001", "01110"),
    "4": ("00010", "00110", "01010", "10010", "11111", "00010", "00010"),
    "5": ("11111", "10000", "11110", "00001", "00001", "10001", "01110"),
    "6": ("00110", "01000", "10000", "11110", "10001", "10001", "01110"),
    "7": ("11111", "00001", "00010", "00100", "01000", "01000", "01000"),
    "8": ("01110", "10001", "10001", "01110", "10001", "10001", "01110"),
    "9": ("01110", "10001", "10001", "01111", "00001", "00010", "01100"),
    "F": ("11111", "10000", "10000", "11110", "10000", "10000", "10000"),
    "P": ("11110", "10001", "10001", "11110", "10000", "10000", "10000"),
    "S": ("01111", "10000", "10000", "01110", "00001", "00001", "11110"),
    " ": ("00000",) * FONT_H,
}


def gray(index: int) -> int:
    """The reflected Gray code of ``index``."""
    return index ^ (index >> 1)


def gray_inverse(code: int) -> int:
    """The index whose reflected Gray code is ``code``."""
    index = 0
    while code:
        index ^= code
        code >>= 1
    return index


def code_bits(index: int) -> list[int]:
    """The strip's 16 cells for ``index``, most significant bit first."""
    if not 0 <= index < 1 << BITS:
        raise ValueError(f"index {index} does not fit {BITS} bits")
    code = gray(index)
    return [(code >> (BITS - 1 - k)) & 1 for k in range(BITS)]


def bits_index(bits: list[int]) -> int:
    """The index whose strip cells are ``bits`` (most significant first)."""
    code = 0
    for bit in bits:
        code = (code << 1) | int(bit)
    return gray_inverse(code)


@dataclass(frozen=True)
class Layout:
    """Where everything sits in a ``width`` x ``height`` frame: the cell
    width, each copy's left edge, and per band the rows of the digits, the
    ruler and the strip."""

    width: int
    height: int
    cell: int
    copies: tuple[int, ...]
    digit_rows: tuple[tuple[int, int], ...]
    rulers: tuple[tuple[int, int], ...]
    strips: tuple[tuple[int, int], ...]

    def cell_edges(self, copy: int) -> tuple[int, ...]:
        """The ``CELLS + 1`` cell edges of copy ``copy`` (px)."""
        x0 = self.copies[copy]
        return tuple(x0 + k * self.cell for k in range(CELLS + 1))


def layout(width: int, height: int) -> Layout:
    """The layout of a ``width`` x ``height`` frame."""
    cell = width // CELL_DIV
    if cell < 1:
        raise ValueError(f"a {width} px wide frame has no room for its cells")
    margin = (width - COPIES * CELLS * cell) // 2
    copies = tuple(margin + k * CELLS * cell for k in range(COPIES))

    def rows(top: float, bottom: float) -> tuple[tuple[int, int], ...]:
        out = []
        for band in range(BANDS):
            y0 = band * height // BANDS
            size = (band + 1) * height // BANDS - y0
            out.append((y0 + int(size * top), y0 + int(size * bottom)))
        return tuple(out)

    return Layout(
        width,
        height,
        cell,
        copies,
        rows(DIGITS_TOP, DIGITS_BOTTOM),
        rows(RULER_TOP, RULER_BOTTOM),
        rows(STRIP_TOP, STRIP_BOTTOM),
    )


def label(index: int, fps: int) -> str:
    """The text in each copy's digit rows."""
    return f"{index:04d}  {fps} FPS"


def text_mask(text: str, unit: int) -> np.ndarray:
    """``text`` in the bitmap font, each font pixel ``unit`` x ``unit``."""
    columns = []
    for n, char in enumerate(text):
        glyph = np.array([[c == "1" for c in row] for row in FONT[char]])
        columns.append(glyph)
        if n + 1 < len(text):
            columns.append(np.zeros((FONT_H, 1), dtype=bool))
    small = np.concatenate(columns, axis=1)
    return np.kron(small, np.ones((unit, unit), dtype=bool))


def static_frame(lay: Layout) -> np.ndarray:
    """The luma plane every frame starts from: the background, each copy's
    ruler and its strip's reference cells."""
    plane = np.full((lay.height, lay.width), Y_BACKGROUND, dtype=np.uint8)
    for copy in range(COPIES):
        edges = lay.cell_edges(copy)
        for y0, y1 in lay.rulers:
            plane[y0:y1, edges[0] : edges[-1]] = Y_BLACK
            plane[y0:y1, edges[0] : edges[1]] = Y_WHITE
            plane[y0:y1, edges[-2] : edges[-1]] = Y_WHITE
        for y0, y1 in lay.strips:
            plane[y0:y1, edges[0] : edges[1]] = Y_BLACK
            plane[y0:y1, edges[1] : edges[2]] = Y_WHITE
            plane[y0:y1, edges[-3] : edges[-2]] = Y_WHITE
            plane[y0:y1, edges[-2] : edges[-1]] = Y_BLACK
    return plane


def draw_frame(
    index: int, fps: int, lay: Layout, base: np.ndarray | None = None
) -> np.ndarray:
    """The luma plane of clip frame ``index`` (``base`` = ``static_frame``)."""
    plane = (static_frame(lay) if base is None else base).copy()
    row = plane[lay.strips[0][0]].copy()
    bits = code_bits(index)
    for copy in range(COPIES):
        edges = lay.cell_edges(copy)
        for k, bit in enumerate(bits):
            row[edges[REF_CELLS + k] : edges[REF_CELLS + k + 1]] = (
                Y_WHITE if bit else Y_BLACK
            )
    for y0, y1 in lay.strips:
        plane[y0:y1] = row
    text = label(index, fps)
    inner = BITS * lay.cell
    units_wide = text_mask(text, 1).shape[1]
    for y0, y1 in lay.digit_rows:
        unit = min((y1 - y0) // FONT_H, inner // units_wide)
        if unit < 1:
            raise ValueError("the label does not fit inside a copy's bit cells")
        mask = text_mask(text, unit)
        height, width = mask.shape
        top = y0 + (y1 - y0 - height) // 2
        for copy in range(COPIES):
            inner0 = lay.cell_edges(copy)[REF_CELLS]
            x0 = inner0 + (inner - width) // 2
            region = plane[top : top + height, x0 : x0 + width]
            region[mask] = Y_DIGIT
    return plane


def frame_count(fps: int, seconds: int) -> int:
    """How many frames a clip of ``seconds`` at ``fps`` has."""
    return fps * seconds


def clip_name(fps: int, seconds: int = CLIP_SECONDS) -> str:
    """The clip's file name (the bench file SongPlayer pins)."""
    return f"wall-clip-v{CLIP_VERSION}-{fps}fps-{seconds}s.mp4"


def ffmpeg_args(
    fps: int, width: int, height: int, seconds: int, out: Path, ffmpeg: str
) -> list[str]:
    """ffmpeg's command line: raw yuv420p frames on stdin + silent stereo."""
    return [
        ffmpeg,
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "rawvideo",
        "-pix_fmt",
        "yuv420p",
        "-s",
        f"{width}x{height}",
        "-framerate",
        str(fps),
        "-i",
        "pipe:0",
        "-f",
        "lavfi",
        "-i",
        f"anullsrc=r={AUDIO_RATE}:cl=stereo",
        "-map",
        "0:v:0",
        "-map",
        "1:a:0",
        "-c:v",
        "libx264",
        "-preset",
        X264_PRESET,
        "-crf",
        str(X264_CRF),
        "-threads",
        "1",
        "-g",
        str(fps),
        "-keyint_min",
        str(fps),
        "-sc_threshold",
        "0",
        "-bf",
        "0",
        "-pix_fmt",
        "yuv420p",
        "-color_range",
        "tv",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-c:a",
        "aac",
        "-b:a",
        "128k",
        "-ar",
        str(AUDIO_RATE),
        "-ac",
        "2",
        "-t",
        str(seconds),
        "-fflags",
        "+bitexact",
        "-flags:v",
        "+bitexact",
        "-flags:a",
        "+bitexact",
        "-map_metadata",
        "-1",
        "-map_chapters",
        "-1",
        "-movflags",
        "+faststart",
        "-f",
        "mp4",
        "-y",
        str(out),
    ]


def sha256_file(path: Path) -> str:
    """The file's sha256, lowercase hex."""
    digest = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_clip(
    fps: int, width: int, height: int, seconds: int, out: Path, ffmpeg: str
) -> None:
    """Encode the clip into ``out`` (written to a temp name, then renamed)."""
    lay = layout(width, height)
    base = static_frame(lay)
    chroma = bytes([C_NEUTRAL]) * (2 * (width // 2) * (height // 2))
    temp = out.with_name(out.name + ".partial")
    args = ffmpeg_args(fps, width, height, seconds, temp, ffmpeg)
    # ffmpeg's messages go to a file, never a pipe nobody reads while the
    # frames are written.
    with tempfile.TemporaryFile() as log:
        proc = subprocess.Popen(args, stdin=subprocess.PIPE, stderr=log)
        assert proc.stdin is not None
        stopped = False
        try:
            for index in range(frame_count(fps, seconds)):
                proc.stdin.write(draw_frame(index, fps, lay, base).data)
                proc.stdin.write(chroma)
            proc.stdin.close()
        except BrokenPipeError:
            stopped = True  # ffmpeg quit early: its own message says why
            # Close the write end too; its flush meets the same broken pipe,
            # which the RuntimeError below already reports.
            with contextlib.suppress(BrokenPipeError):
                proc.stdin.close()
        except BaseException:
            # Any other failure (Ctrl-C): no ffmpeg left running, no partial
            # file left behind.
            proc.kill()
            proc.wait()
            temp.unlink(missing_ok=True)
            raise
        code = proc.wait()
        log.seek(0)
        err = log.read().decode(errors="replace").strip()
    if stopped or code != 0:
        temp.unlink(missing_ok=True)
        why = "stopped reading frames" if stopped else f"exited {code}"
        raise RuntimeError(f"ffmpeg {why}: {err}")
    temp.replace(out)


def parse_size(text: str) -> tuple[int, int]:
    """``WxH`` -> (W, H); the width a multiple of 960 (whole cells, the 4K
    layout scaled), the height even."""
    width_text, _, height_text = text.lower().partition("x")
    width, height = int(width_text), int(height_text)
    if width % 960 or width <= 0 or height <= 0 or height % 2:
        raise argparse.ArgumentTypeError(
            f"{text}: the width must be a multiple of 960 and the height even"
        )
    return width, height


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--fps", type=int, nargs="+", default=list(FPS_SET))
    parser.add_argument("--seconds", type=int, default=CLIP_SECONDS)
    parser.add_argument("--size", type=parse_size, default=(WIDTH, HEIGHT))
    parser.add_argument("--ffmpeg", default="ffmpeg")
    args = parser.parse_args(argv)
    args.out.mkdir(parents=True, exist_ok=True)
    width, height = args.size
    for fps in args.fps:
        name = clip_name(fps, args.seconds)
        if (width, height) != (WIDTH, HEIGHT):
            name = name.replace(".mp4", f"-{width}x{height}.mp4")
        out = args.out / name
        write_clip(fps, width, height, args.seconds, out, args.ffmpeg)
        print(f"{out}  {out.stat().st_size} B  sha256 {sha256_file(out)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
