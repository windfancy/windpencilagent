"""逐页截图：点击左侧导航每一项，截一张图存到 target/pages/。

导航项是 Slint TouchArea，只能用真实鼠标消息（PostMessage 会被 winit 丢弃），
所以这里用 SendInput + SetCursorPos（见项目记忆里的 UI 自动化铁律）。

用法：python tools/shoot_pages.py [exe] [输出目录]
"""

import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import sys
import time

from PIL import Image

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32
user32.FindWindowW.restype = wt.HWND

TITLE = "windbot"

# (标签, 图像坐标 y) —— 由 0_对话.png 的深色像素行检测得到（含组标题行，已剔除）
NAV = [
    ("0_对话", 208),
    ("1_任务板", 245),
    ("2_树包", 313),
    ("3_工具", 352),
    ("4_知识", 390),
    ("5_技能", 428),
    ("6_空间", 496),
    ("7_记忆", 533),
    ("8_模型", 601),
    ("9_设置", 640),
]
NAV_X = 46  # 导航项中心 x（图像坐标）


def client_origin(hwnd):
    """客户区左上角的屏幕坐标。"""
    pt = wt.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(pt))
    return pt.x, pt.y


def click(x, y):
    user32.SetCursorPos(x, y)
    time.sleep(0.05)
    user32.mouse_event(0x0002, 0, 0, 0, 0)  # LEFTDOWN
    time.sleep(0.05)
    user32.mouse_event(0x0004, 0, 0, 0, 0)  # LEFTUP


def force_foreground(hwnd):
    """Slint 只在窗口是前台时处理鼠标（后台点击会被丢弃），先抢前台。"""
    fg = user32.GetForegroundWindow()
    tid_fg = user32.GetWindowThreadProcessId(fg, None)
    tid_me = ctypes.windll.kernel32.GetCurrentThreadId()
    user32.AttachThreadInput(tid_me, tid_fg, True)
    user32.SetForegroundWindow(hwnd)
    user32.BringWindowToTop(hwnd)
    user32.AttachThreadInput(tid_me, tid_fg, False)
    time.sleep(0.3)


def capture(hwnd, out_path):
    rect = wt.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    w, h = rect.right - rect.left, rect.bottom - rect.top
    hdc = user32.GetWindowDC(hwnd)
    mem = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    gdi32.SelectObject(mem, bmp)
    user32.PrintWindow(hwnd, mem, 2)

    class BIH(ctypes.Structure):
        _fields_ = [
            ("biSize", wt.DWORD), ("biWidth", wt.LONG), ("biHeight", wt.LONG),
            ("biPlanes", wt.WORD), ("biBitCount", wt.WORD),
            ("biCompression", wt.DWORD), ("biSizeImage", wt.DWORD),
            ("biXPelsPerMeter", wt.LONG), ("biYPelsPerMeter", wt.LONG),
            ("biClrUsed", wt.DWORD), ("biClrImportant", wt.DWORD),
        ]

    bi = BIH()
    bi.biSize = ctypes.sizeof(BIH)
    bi.biWidth, bi.biHeight, bi.biPlanes, bi.biBitCount = w, -h, 1, 32
    buf = ctypes.create_string_buffer(w * h * 4)
    gdi32.GetDIBits(mem, bmp, 0, h, buf, ctypes.byref(bi), 0)
    Image.frombuffer("RGBA", (w, h), buf, "raw", "BGRA", 0, 1).convert("RGB").save(out_path)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(hwnd, hdc)


def main():
    exe = sys.argv[1] if len(sys.argv) > 1 else "target/release/windbot-desktop.exe"
    outdir = sys.argv[2] if len(sys.argv) > 2 else "target/pages"
    os.makedirs(outdir, exist_ok=True)
    proc = subprocess.Popen([exe], cwd=os.path.dirname(os.path.abspath(exe)) or ".")
    hwnd = 0
    try:
        end = time.time() + 30
        while time.time() < end and not hwnd:
            hwnd = user32.FindWindowW(None, TITLE)
            time.sleep(0.4)
        if not hwnd:
            print("FAIL: 未找到窗口")
            return 3
        time.sleep(2.0)
        ox, oy = client_origin(hwnd)
        force_foreground(hwnd)
        print(f"hwnd={hwnd} client_origin=({ox},{oy})")
        for label, iy in NAV:
            force_foreground(hwnd)
            # 图像坐标 → 客户区坐标（图像含 8px 边框 + 43px 标题栏）
            click(ox + (NAV_X - 8), oy + (iy - 43))
            time.sleep(0.9)
            path = os.path.join(outdir, f"{label}.png")
            capture(hwnd, path)
            print("saved", path)
        return 0
    finally:
        if hwnd:
            user32.PostMessageW(hwnd, 0x0010, 0, 0)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
