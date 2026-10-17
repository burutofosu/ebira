"""Writes the Zstandard test streams in this directory with the reference implementation.

Run with Python 3.14 or later (its `compression.zstd` module wraps libzstd):

    python fixtures/zstd/generate.py

The text is generated, so the streams can be made again; the decoder's tests compare what
each stream decompresses to with the others, and the frames that carry a content checksum
check the decoded bytes against it.
"""

import random
from pathlib import Path

from compression import zstd

HERE = Path(__file__).resolve().parent

WORDS = (
    "the a to of and in that is for it on with as was this be at by from or have an they "
    "which one you were all we her she there would their will when who him been has more if "
    "no out so said what up its about into than them can only other new some could time these "
    "two may then do first any my now such like our over man me even most made after also did "
    "corpus record session sync search said context resume source offset frame block literal "
    "検索 記録 会話 圧縮 復元 同期 決定 変更 確認 作業 履歴 原文 引用 発言 範囲"
).split()


def text(seed: int, records: int) -> bytes:
    """JSONL with prose in two scripts, numbers, repeated passages, a long run of one byte, and
    a stretch of random letters."""
    rng = random.Random(seed)
    passage = " ".join(rng.choice(WORDS) for _ in range(60))
    lines = []
    for index in range(records):
        if index % 7 == 3:
            body = passage
        elif index % 40 == 5:
            body = "".join(rng.choice("ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/")
                           for _ in range(rng.randint(40, 200)))
        else:
            body = " ".join(rng.choice(WORDS[: 40 + index % 120]) for _ in range(rng.randint(3, 60)))
        if index == records // 2:
            body += " " + "=" * 5000
        lines.append(
            '{"timestamp":"2026-10-18T%02d:%02d:%02d.%03dZ","type":"response_item","payload":'
            '{"type":"message","role":"%s","content":[{"type":"output_text","text":"%s"}]},'
            '"n":%d}'
            % (
                index // 3600 % 24,
                index // 60 % 60,
                index % 60,
                rng.randint(0, 999),
                rng.choice(("user", "assistant")),
                body,
                index,
            )
        )
    return ("\n".join(lines) + "\n").encode("utf-8")


def compressed(data: bytes, level: int, checksum: bool = True) -> bytes:
    options = {
        zstd.CompressionParameter.compression_level: level,
        zstd.CompressionParameter.checksum_flag: int(checksum),
    }
    return zstd.compress(data, options=options)


def streamed(data: bytes, level: int) -> bytes:
    """A frame written in pieces without its size, so it has a window and no content size."""
    compressor = zstd.ZstdCompressor(
        options={
            zstd.CompressionParameter.compression_level: level,
            zstd.CompressionParameter.checksum_flag: 0,
            zstd.CompressionParameter.window_log: 17,
        }
    )
    out = b""
    for start in range(0, len(data), 10_000):
        out += compressor.compress(data[start : start + 10_000])
    return out + compressor.flush()


def skippable(payload: bytes) -> bytes:
    return (0x184D2A53).to_bytes(4, "little") + len(payload).to_bytes(4, "little") + payload


def repeating(seed: int, size: int) -> bytes:
    """A short random pattern repeated with a byte changed every fourth time: long matches a
    little way back, over more output than the decoder keeps at once, in blocks whose
    sequences are alike enough for single-symbol tables."""
    rng = random.Random(seed)
    pattern = bytearray(rng.randbytes(997))
    out = bytearray()
    count = 0
    while len(out) < size:
        if count % 4 == 0:
            pattern[rng.randrange(len(pattern))] = rng.randrange(256)
        out += pattern
        count += 1
    return bytes(out[:size])


def windowed(data: bytes) -> bytes:
    """A frame with a window smaller than its content and no content size, written in pieces."""
    compressor = zstd.ZstdCompressor(
        options={
            zstd.CompressionParameter.compression_level: 3,
            zstd.CompressionParameter.checksum_flag: 1,
            zstd.CompressionParameter.window_log: 17,
        }
    )
    out = b""
    for start in range(0, len(data), 100_000):
        out += compressor.compress(data[start : start + 100_000])
    return out + compressor.flush()


def main() -> None:
    sample = text(20261018, 900)
    other = text(7, 40)
    streams = {
        "text-3.zst": compressed(sample, 3),
        "text-19.zst": compressed(sample, 19),
        "text-fast.zst": compressed(sample[:150_000], -5),
        "text-streamed.zst": streamed(sample, 6),
        "frames.zst": compressed(other, 3) + skippable(b"not content") + compressed(other, 19),
        "run.zst": compressed(b"x" * 300_000, 3),
        "noise.zst": compressed(random.Random(1).randbytes(3_000), 3),
        "empty.zst": compressed(b"", 3),
        "window.zst": windowed(repeating(11, 2_500_000)),
    }
    for name, data in streams.items():
        (HERE / name).write_bytes(data)
        print(f"{name}: {len(data)} bytes")


if __name__ == "__main__":
    main()
