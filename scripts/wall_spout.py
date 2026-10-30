"""Pictures of a Spout sender, as ``wall_frames`` reads them (#272).

SNV's wall left the Windows desktop on 29.10.2026 (wall-bridge shows it on a
specialized display), so desktop duplication no longer sees it. The last
stage SongPlayer can read is Arena's own Spout output, ``Arena - Screen 2``,
the picture wall-bridge takes: the gate judges Arena's output before the
bridge, never what the bridge puts on the screens (owner, #272, 30.10.2026).

``NewPictures`` is the pure part. Arena's sender keeps no frame count, so a
Spout read answers on every poll: a poll that reads the same bytes as the one
before is a poll on which nothing new was presented, what an empty dxcam
grab means. ``GlSender`` is the Windows glue (SpoutGL, a hidden OpenGL
window; ``ops\\probe-venv``'s versions, ci.yml installs them in the e2e venv).

Reading only a few rows keeps a poll at ~0.6 ms on SNV; 150 rows take ~1.75 ms
and a whole 7680x1080 picture 16 ms (measured 30.10.2026 next to wall-bridge:
0 missed vblanks, 0 drops).
"""

from __future__ import annotations

import time
from typing import Protocol

import numpy as np

# How long a sender may take to answer before the capture fails by name (s).
OPEN_S = 5.0


class Sender(Protocol):
    """One sender's picture: BGRA rows, top first."""

    size: tuple[int, int]

    def read(self, region: tuple[int, int, int, int]) -> bytes: ...


class NewPictures:
    """``grab`` as dxcam's: the region's picture, ``None`` when nothing new
    was presented since the last grab of that region."""

    def __init__(self, sender: Sender) -> None:
        self.sender = sender
        self.last: tuple[tuple[int, int, int, int], bytes] | None = None

    def grab(self, region: tuple[int, int, int, int] | None = None):
        width, height = self.sender.size
        box = region or (0, 0, width, height)
        left, top, right, bottom = box
        if not (0 <= left < right <= width and 0 <= top < bottom <= height):
            raise ValueError(f"region {box} is not inside {width}x{height}")
        data = self.sender.read(box)
        if self.last == (box, data):
            return None
        self.last = (box, data)
        picture = np.frombuffer(data, dtype=np.uint8)
        return picture.reshape(bottom - top, right - left, 4)


class GlSender:
    """A named Spout sender through SpoutGL (Windows). Read only: the
    receiver copies the sender's shared texture into a texture of its own.
    The rows read back top first, as Arena drew them (measured 30.10.2026)."""

    def __init__(self, name: str) -> None:
        import glfw
        import SpoutGL
        from OpenGL import GL

        self.gl = GL
        self.name = name
        if not glfw.init():
            raise RuntimeError("glfw did not initialise")
        glfw.window_hint(glfw.VISIBLE, False)
        window = glfw.create_window(16, 16, "wall-frames", None, None)
        if not window:
            raise RuntimeError("glfw made no OpenGL window")
        glfw.make_context_current(window)
        self.receiver = SpoutGL.SpoutReceiver()
        self.receiver.setReceiverName(name)
        self.size = self._open()
        self.texture = GL.glGenTextures(1)
        GL.glBindTexture(GL.GL_TEXTURE_2D, self.texture)
        GL.glTexImage2D(
            GL.GL_TEXTURE_2D,
            0,
            GL.GL_RGBA8,
            *self.size,
            0,
            GL.GL_RGBA,
            GL.GL_UNSIGNED_BYTE,
            None,
        )
        self.framebuffer = GL.glGenFramebuffers(1)
        GL.glBindFramebuffer(GL.GL_FRAMEBUFFER, self.framebuffer)
        GL.glFramebufferTexture2D(
            GL.GL_FRAMEBUFFER,
            GL.GL_COLOR_ATTACHMENT0,
            GL.GL_TEXTURE_2D,
            self.texture,
            0,
        )
        GL.glPixelStorei(GL.GL_PACK_ALIGNMENT, 1)

    def _open(self) -> tuple[int, int]:
        deadline = time.perf_counter() + OPEN_S
        while time.perf_counter() < deadline:
            self.receiver.receiveTexture(0, self.gl.GL_TEXTURE_2D, False, 0)
            if self.receiver.isUpdated():
                return self.receiver.getSenderWidth(), self.receiver.getSenderHeight()
            time.sleep(0.002)
        listed = list(self.receiver.getSenderList())
        raise RuntimeError(f"no Spout sender {self.name!r}; listed: {listed}")

    def read(self, region: tuple[int, int, int, int]) -> bytes:
        gl = self.gl
        self.receiver.receiveTexture(self.texture, gl.GL_TEXTURE_2D, False, 0)
        if self.receiver.isUpdated():
            size = self.receiver.getSenderWidth(), self.receiver.getSenderHeight()
            raise RuntimeError(f"sender {self.name!r} changed to {size}")
        left, top, right, bottom = region
        gl.glBindFramebuffer(gl.GL_FRAMEBUFFER, self.framebuffer)
        return bytes(
            gl.glReadPixels(
                left,
                top,
                right - left,
                bottom - top,
                gl.GL_BGRA,
                gl.GL_UNSIGNED_BYTE,
            )
        )

    def close(self) -> None:
        self.receiver.releaseReceiver()
