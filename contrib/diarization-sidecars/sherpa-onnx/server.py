#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#     # Known to work with these versions; --threshold 1.1 suits sherpa-onnx 1.13.8.
#     "sherpa-onnx==1.13.8",
#     "numpy==2.5.4",
#     "soundfile==0.14.0",
# ]
# ///
"""Speaker diarization sidecar for whisrs, on sherpa-onnx.

pyannote segmentation-3.0 finds the speech and the speaker changes, a speaker
embedding model (3D-Speaker ERes2Net by default) tells the voices apart, and
the turns go back to whisrs in its diarization sidecar contract:

    POST /diarize   multipart form: file = 16 kHz mono WAV,
                    num_speakers = N (optional, 1 to 20; 0 or none: decide)
    200 {"segments": [{"start": 1.2, "end": 4.85, "speaker": "S0"}, ...]}
    GET  /health    {"ok": true, ...}

Without num_speakers, voices are found by a clustering threshold, which
also leaves a few small clusters on long recordings: a cough, two people at
once, one voice split in pieces. Each cluster holding less than --min-share of
the speech, smallest first, is merged into the cluster whose voice is closest
(cosine similarity of their embedding centroids). Only a small cluster with
--keep-seconds of speech or more that sounds like nobody else (below
--min-similarity) stays a voice of its own: someone who says little. A
cluster too short to embed goes to the voice next to it in time. With
num_speakers, sherpa-onnx groups the speech into that many clusters (one
can come back empty on short audio) and nothing is merged.

Memory: an upload is read once and decoded where it lies, about 3 times its
size while that happens (a 345 MB WAV, three hours of speech, about 1 GB),
then the diarization's own. One recording is diarized at a time: a request
that comes while one runs is answered HTTP 503 with Retry-After, and its
body is only read to be dropped. That takes under a second, except during
sherpa-onnx's first pass over the audio, which holds the GIL until it ends.

    uv run server.py --seg model.onnx --emb eres2net.onnx
    uv run server.py --seg model.onnx --emb eres2net.onnx --input talk.wav
"""

from __future__ import annotations

import argparse
import io
import json
import re
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import numpy as np
import sherpa_onnx
import soundfile as sf

RATE = 16000
# A voice's centroid: the mean embedding of its longest turns, each cut to
# PIECE_SECONDS (as long as the pipeline's own windows, so memory stays the
# same), at most PIECES of them.
PIECE_SECONDS = 8.0
PIECES = 4


class Diarizer:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.sd = sherpa_onnx.OfflineSpeakerDiarization(self._config(-1))
        self.extractor = sherpa_onnx.SpeakerEmbeddingExtractor(
            sherpa_onnx.SpeakerEmbeddingExtractorConfig(
                model=args.emb, num_threads=args.threads
            )
        )
        self.lock = threading.Lock()

    def _config(self, num_clusters: int) -> sherpa_onnx.OfflineSpeakerDiarizationConfig:
        a = self.args
        config = sherpa_onnx.OfflineSpeakerDiarizationConfig(
            segmentation=sherpa_onnx.OfflineSpeakerSegmentationModelConfig(
                pyannote=sherpa_onnx.OfflineSpeakerSegmentationPyannoteModelConfig(model=a.seg),
                num_threads=a.threads,
            ),
            embedding=sherpa_onnx.SpeakerEmbeddingExtractorConfig(
                model=a.emb, num_threads=a.threads
            ),
            clustering=sherpa_onnx.FastClusteringConfig(
                num_clusters=num_clusters, threshold=a.threshold
            ),
            min_duration_on=0.3,
            min_duration_off=0.5,
        )
        if not config.validate():
            raise SystemExit("invalid sherpa-onnx config: check --seg and --emb")
        return config

    def diarize(self, audio: np.ndarray, num_speakers: int | None) -> tuple[list[dict], dict]:
        """Turns as {start, end, speaker} labelled S0, S1, ... by first appearance."""
        with self.lock:
            segs = self.raw(audio, num_speakers)
            stats = {"voices_found": len({s for _, _, s in segs}), "merged": {}}
            if not num_speakers and self.args.min_share > 0:
                segs, stats["merged"] = merge_minor(
                    segs,
                    lambda spk: self.centroid(audio, segs, spk),
                    self.args.min_share,
                    self.args.min_similarity,
                    self.args.keep_seconds,
                )
        segs = _union(segs)
        labels: dict[int, str] = {}
        out = [
            {
                "start": round(s, 3),
                "end": round(e, 3),
                "speaker": labels.setdefault(spk, f"S{len(labels)}"),
            }
            for s, e, spk in segs
        ]
        stats["voices"] = len(labels)
        return out, stats

    def raw(self, audio: np.ndarray, num_speakers: int | None) -> list[tuple]:
        """sherpa-onnx's own turns: (start, end, cluster)."""
        if num_speakers:
            self.sd.set_config(self._config(num_speakers))
        try:
            result = self.sd.process(audio, callback=_let_others_run).sort_by_start_time()
        finally:
            if num_speakers:
                self.sd.set_config(self._config(-1))
        return [(r.start, r.end, r.speaker) for r in result]

    def centroid(self, audio: np.ndarray, segs: list[tuple], spk) -> np.ndarray | None:
        """The unit-length mean embedding of a voice's longest turns."""
        mine = sorted(((e - s, s) for s, e, k in segs if k == spk), reverse=True)
        vecs = []
        for d, s in mine[:PIECES]:
            piece = audio[int(s * RATE): int((s + min(d, PIECE_SECONDS)) * RATE)]
            stream = self.extractor.create_stream()
            stream.accept_waveform(sample_rate=RATE, waveform=piece)
            stream.input_finished()
            if self.extractor.is_ready(stream):
                v = np.asarray(self.extractor.compute(stream), dtype=np.float32)
                n = float(np.linalg.norm(v))
                if n > 0:
                    vecs.append(v / n)
        if not vecs:
            return None
        m = np.mean(vecs, axis=0)
        n = float(np.linalg.norm(m))
        return m / n if n > 0 else None


def _let_others_run(done: int, total: int) -> int:
    """process()'s progress callback, called between embedding chunks.
    sherpa-onnx holds the GIL for the whole diarization; sleeping 0 s here
    hands it to the other request threads, so a second request gets its 503
    within a second (its first pass, before any chunk, still blocks them)."""
    time.sleep(0)
    return 0  # non-zero would abort


def merge_minor(segs, centroid_of, min_share: float, min_similarity: float,
                keep_seconds: float):
    """Merge the small clusters (see the module doc). Returns the turns with
    their new clusters and, per merged or kept cluster, what happened."""
    talk: dict = {}
    for s, e, spk in segs:
        talk[spk] = talk.get(spk, 0.0) + (e - s)
    total = sum(talk.values())
    if total <= 0 or len(talk) < 2:
        return segs, {}
    small = sorted((k for k in talk if talk[k] / total < min_share), key=talk.get)
    if len(small) == len(talk):  # nothing is big: keep the biggest
        small.remove(max(talk, key=talk.get))
    if not small:
        return segs, {}
    cent = {k: centroid_of(k) for k in talk}
    into: dict = {}
    how: dict[str, str] = {}
    for c in small:
        live = [k for k in talk if k != c and k not in into]
        own = talk[c]
        scored = [(float(np.dot(cent[c], cent[k])), k) for k in live
                  if cent[c] is not None and cent[k] is not None]
        if scored:
            sim, target = max(scored)
            if sim < min_similarity and own >= keep_seconds:
                how[str(c)] = f"kept: closest voice {target} at cosine {sim:.2f}, {own:.1f} s"
                continue
            why = f"cosine {sim:.2f}"
            if cent[target] is not None:
                w_t, w_c = min(talk[target], PIECES * PIECE_SECONDS), min(own, PIECES * PIECE_SECONDS)
                v = w_t * cent[target] + w_c * cent[c]
                n = float(np.linalg.norm(v))
                cent[target] = v / n if n > 0 else cent[target]
        else:
            target, why = _time_neighbour(segs, c, live), "next in time"
        into[c] = target
        talk[target] += own
        how[str(c)] = f"-> {target} ({why}, {own:.1f} s)"

    def root(k):
        while k in into:
            k = into[k]
        return k

    return [(s, e, root(spk)) for s, e, spk in segs], how


def _time_neighbour(segs, spk, live):
    """The live voice whose turn comes nearest in time to one of spk's turns."""
    best = (float("inf"), live[0])
    for s, e, k in segs:
        if k != spk:
            continue
        for s2, e2, k2 in segs:
            if k2 in live:
                gap = max(s2 - e, s - e2, 0.0)
                best = min(best, (gap, k2))
    return best[1]


def _union(segs):
    """Join a voice's turns that touch or overlap (after a merge)."""
    out: list[list] = []
    last: dict[int, list] = {}
    for s, e, spk in sorted(segs):
        prev = last.get(spk)
        if prev is not None and s <= prev[1]:
            prev[1] = max(prev[1], e)
            continue
        cur = [s, e, spk]
        out.append(cur)
        last[spk] = cur
    return [tuple(x) for x in out]


class _View(io.RawIOBase):
    """A read-only file over a memoryview, so soundfile decodes the upload
    where it lies instead of from a copy (io.BytesIO copies a memoryview)."""

    def __init__(self, view: memoryview) -> None:
        self.view, self.pos = view, 0

    def readable(self) -> bool:
        return True

    def seekable(self) -> bool:
        return True

    def readinto(self, b) -> int:
        n = max(0, min(len(b), len(self.view) - self.pos))
        b[:n] = self.view[self.pos:self.pos + n]
        self.pos += n
        return n

    def seek(self, offset: int, whence: int = io.SEEK_SET) -> int:
        base = {io.SEEK_SET: 0, io.SEEK_CUR: self.pos, io.SEEK_END: len(self.view)}[whence]
        self.pos = max(0, base + offset)
        return self.pos

    def tell(self) -> int:
        return self.pos

    def __repr__(self) -> str:  # how soundfile names it in its errors
        return "the uploaded file"


def read_wav(data) -> np.ndarray:
    """A 16 kHz WAV (bytes or a memoryview) as float32 mono samples."""
    source = _View(data) if isinstance(data, memoryview) else io.BytesIO(data)
    audio, sr = sf.read(source, dtype="float32")
    if sr != RATE:
        raise ValueError(f"expected a {RATE} Hz WAV, got {sr} Hz")
    return audio if audio.ndim == 1 else np.ascontiguousarray(audio[:, 0])


_NAME = re.compile(rb'(?im)^content-disposition:[^\r\n]*?;\s*name="?([^";\r\n]+)')


def parse_multipart(buf: bytearray, boundary: bytes) -> dict[str, memoryview]:
    """The parts of a multipart/form-data body by name, as views into it: the
    file's bytes are not copied."""
    delim = b"--" + boundary
    view = memoryview(buf)
    parts: dict[str, memoryview] = {}
    pos = buf.find(delim)
    if pos < 0:
        raise ValueError("the body holds no multipart boundary")
    while True:
        pos += len(delim)
        if buf[pos:pos + 2] == b"--":  # the closing boundary
            return parts
        head = buf.find(b"\r\n", pos)
        head_end = buf.find(b"\r\n\r\n", head) if head >= 0 else -1
        if head_end < 0:
            raise ValueError("a multipart part has no end to its headers")
        data_start = head_end + 4
        nxt = buf.find(b"\r\n" + delim, data_start)
        if nxt < 0:
            raise ValueError("the multipart body has no closing boundary")
        name = _NAME.search(buf, head, head_end + 2)
        if name:
            parts.setdefault(name.group(1).decode("latin-1"), view[data_start:nxt])
        pos = nxt + 2


class BadRequest(Exception):
    def __init__(self, code: int, message: str) -> None:
        super().__init__(message)
        self.code = code


def make_handler(diarizer: Diarizer, args: argparse.Namespace):
    # One diarization at a time, and one upload in memory: a second request
    # is told 503 instead of having its body read and kept.
    busy = threading.Lock()
    max_bytes = args.max_upload_mb * 1024 * 1024

    class Handler(BaseHTTPRequestHandler):
        def _json(self, code: int, obj, headers=()) -> None:
            body = json.dumps(obj).encode()
            self.send_response(code)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            for k, v in headers:
                self.send_header(k, v)
            self.end_headers()
            self.wfile.write(body)

        def _discard(self, length: int) -> None:
            """Read and drop an unread body, so the client gets the answer
            instead of a reset connection. Large waiting reads: during a
            diarization each one waits for its turn at the GIL."""
            self.connection.settimeout(30)
            scratch = bytearray(min(length, 4 << 20))
            try:
                length -= len(self.rfile.read1(min(length, 1 << 16)))  # what it buffered
                while length > 0:
                    n = self.connection.recv_into(scratch, min(length, len(scratch)),
                                                  socket.MSG_WAITALL)
                    if not n:
                        break
                    length -= n
            except OSError:
                pass

        def do_GET(self):
            if self.path.rstrip("/") == "/health":
                return self._json(200, {"ok": True, "threshold": args.threshold,
                                        "min_share": args.min_share})
            self._json(404, {"error": "not found"})

        def _length(self) -> int:
            raw = self.headers.get("content-length")
            if raw is None:
                raise BadRequest(411, "a Content-Length is required")
            try:
                length = int(raw)
            except ValueError:
                raise BadRequest(400, "Content-Length is not a number") from None
            if length <= 0:
                raise BadRequest(400, "the body is empty")
            if length > max_bytes:
                raise BadRequest(413, f"the upload is over --max-upload-mb ({args.max_upload_mb})")
            return length

        def do_POST(self):
            # The headers are checked before a byte of the body is read.
            self.close_connection = True
            try:
                length = self._length()
            except BadRequest as e:
                return self._json(e.code, {"error": str(e)})
            ctype = self.headers.get("content-type", "")
            boundary = re.search(r'boundary="?([^";]+)"?', ctype)
            if self.path.split("?")[0].rstrip("/") != "/diarize":
                self._json(404, {"error": "POST /diarize"})
                return self._discard(length)
            if not ctype.lower().startswith("multipart/form-data") or not boundary:
                self._json(400, {"error": "send multipart/form-data with a file part"})
                return self._discard(length)
            if not busy.acquire(blocking=False):
                self._json(503, {"error": "busy with another recording"},
                           headers=[("retry-after", "30")])
                return self._discard(length)
            try:
                self._diarize(length, boundary.group(1).encode("latin-1"))
            except BadRequest as e:
                self._json(e.code, {"error": str(e)})
            except Exception as e:  # noqa: BLE001 - never a dropped connection
                print(f"diarization failed: {e!r}", file=sys.stderr, flush=True)
                self._json(500, {"error": f"diarization failed: {e}"})
            finally:
                busy.release()

        def _diarize(self, length: int, boundary: bytes) -> None:
            buf = bytearray(length)
            view = memoryview(buf)
            got = 0
            while got < length:
                n = self.rfile.readinto(view[got:])
                if not n:
                    raise BadRequest(400, f"the body ended after {got} of {length} bytes")
                got += n
            try:
                parts = parse_multipart(buf, boundary)
            except ValueError as e:
                raise BadRequest(400, str(e)) from None
            num = None
            if "num_speakers" in parts:
                text = bytes(parts["num_speakers"]).decode("utf-8", "replace").strip()
                try:
                    num = int(text or 0)
                except ValueError:
                    raise BadRequest(400, "num_speakers must be an integer") from None
                if not 0 <= num <= 20:
                    raise BadRequest(400, "num_speakers must be 1 to 20 (0 or none: decide)")
                num = num or None
            if "file" not in parts or len(parts["file"]) == 0:
                raise BadRequest(400, "missing the file part")
            try:
                audio = read_wav(parts["file"])
            except Exception as e:  # noqa: BLE001 - any unreadable upload
                raise BadRequest(400, f"bad WAV: {e}") from None
            # The upload is decoded: let it go before the long part.
            for v in parts.values():
                v.release()
            view.release()
            del parts, view, buf
            started = time.monotonic()
            segments, stats = diarizer.diarize(audio, num)
            took = time.monotonic() - started
            print(f"{len(audio) / RATE:.0f} s of audio in {took:.1f} s: {stats['voices']} voice(s)"
                  f" ({stats['voices_found']} found{', asked for ' + str(num) if num else ''})",
                  file=sys.stderr, flush=True)
            self._json(200, {"segments": segments})

        def log_message(self, fmt, *a):
            pass

    return Handler


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--seg", required=True, help="pyannote segmentation-3.0 model.onnx")
    ap.add_argument("--emb", required=True, help="speaker embedding .onnx (ERes2Net)")
    ap.add_argument("--threshold", type=float, default=1.1,
                    help="clustering threshold without num_speakers: higher finds fewer "
                         "voices (default 1.1, tuned for ERes2Net on meetings)")
    ap.add_argument("--min-share", type=float, default=0.05,
                    help="a voice holding less than this share of the speech is merged "
                         "into the closest other voice (default 0.05; 0 keeps all)")
    ap.add_argument("--min-similarity", type=float, default=0.6,
                    help="a small voice with --keep-seconds of speech or more stays apart "
                         "when its cosine similarity to every other voice is below this "
                         "(default 0.6)")
    ap.add_argument("--keep-seconds", type=float, default=10.0,
                    help="small voices with less speech than this are always merged "
                         "(default 10)")
    ap.add_argument("--threads", type=int, default=4)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--port", type=int, default=8770)
    ap.add_argument("--max-upload-mb", type=int, default=400,
                    help="largest upload accepted (default 400: 3.6 hours of 16 kHz speech); "
                         "reading one takes about 3 times its size in memory")
    ap.add_argument("--input", help="diarize this WAV once, print the JSON answer and exit")
    ap.add_argument("--num-speakers", type=int, default=0, help="with --input")
    args = ap.parse_args()
    if not 0 <= args.num_speakers <= 20:
        ap.error("--num-speakers must be 1 to 20 (0: decide)")

    diarizer = Diarizer(args)
    if args.input:
        with open(args.input, "rb") as f:
            audio = read_wav(f.read())
        started = time.monotonic()
        segments, stats = diarizer.diarize(audio, args.num_speakers or None)
        stats["seconds"] = round(time.monotonic() - started, 1)
        stats["audio_seconds"] = round(len(audio) / RATE, 1)
        print(json.dumps(stats), file=sys.stderr)
        print(json.dumps({"segments": segments}))
        return
    server = ThreadingHTTPServer((args.host, args.port), make_handler(diarizer, args))
    print(f"listening on http://{args.host}:{args.port}/diarize", file=sys.stderr, flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
