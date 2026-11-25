"""windbot 桌面端冒烟自查：起进程 → 截窗口 → 关窗口。

仅用于开发期验证界面能正常渲染，不参与产品功能。

用法：
    python tools/smoke_ui.py [exe 路径] [输出 png 路径]
"""

import ctypes
import ctypes.wintypes as wt
import os
import subprocess
import sys
import time

user32 = ctypes.windll.user32
gdi32 = ctypes.windll.gdi32

user32.FindWindowW.restype = wt.HWND
user32.GetWindowRect.restype = wt.BOOL
user32.PrintWindow.restype = wt.BOOL

TITLE = "windbot"


def find_window(timeout=30.0):
    end = time.time() + timeout
    while time.time() < end:
        hwnd = user32.FindWindowW(None, TITLE)
        if hwnd:
            return hwnd
        time.sleep(0.4)
    return 0


def capture(hwnd, out_path):
    rect = wt.RECT()
    user32.GetWindowRect(hwnd, ctypes.byref(rect))
    w, h = rect.right - rect.left, rect.bottom - rect.top
    if w <= 0 or h <= 0:
        raise RuntimeError(f"窗口尺寸异常 {w}x{h}")
    hdc = user32.GetWindowDC(hwnd)
    mem_dc = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    gdi32.SelectObject(mem_dc, bmp)
    # PW_RENDERFULLCONTENT = 2：抓客户区实际渲染内容（含子窗口）
    ok = user32.PrintWindow(hwnd, mem_dc, 2)

    class BITMAPINFOHEADER(ctypes.Structure):
        _fields_ = [
            ("biSize", wt.DWORD), ("biWidth", wt.LONG), ("biHeight", wt.LONG),
            ("biPlanes", wt.WORD), ("biBitCount", wt.WORD),
            ("biCompression", wt.DWORD), ("biSizeImage", wt.DWORD),
            ("biXPelsPerMeter", wt.LONG), ("biYPelsPerMeter", wt.LONG),
            ("biClrUsed", wt.DWORD), ("biClrImportant", wt.DWORD),
        ]

    bi = BITMAPINFOHEADER()
    bi.biSize = ctypes.sizeof(BITMAPINFOHEADER)
    bi.biWidth, bi.biHeight = w, -h
    bi.biPlanes, bi.biBitCount = 1, 32
    bufsize = w * h * 4
    buf = ctypes.create_string_buffer(bufsize)
    gdi32.GetDIBits(mem_dc, bmp, 0, h, buf, ctypes.byref(bi), 0)

    from PIL import Image
    img = Image.frombuffer("RGBA", (w, h), buf, "raw", "BGRA", 0, 1)
    img.convert("RGB").save(out_path)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem_dc)
    user32.ReleaseDC(hwnd, hdc)
    return ok, w, h


def main():
    exe = sys.argv[1] if len(sys.argv) > 1 else "target/release/windbot-desktop.exe"
    out = sys.argv[2] if len(sys.argv) > 2 else "target/ui_smoke.png"
    delay = float(sys.argv[3]) if len(sys.argv) > 3 else 1.5
    if not os.path.exists(exe):
        print(f"缺少 exe：{exe}")
        return 2
    proc = subprocess.Popen([exe], cwd=os.path.dirname(os.path.abspath(exe)) or ".")
    hwnd = 0
    try:
        hwnd = find_window()
        if not hwnd:
            print("FAIL: 30s 内未找到窗口（进程可能已崩溃）")
            return 3
        time.sleep(delay)  # 等首帧渲染完成 / 等提示条自动收起
        ok, w, h = capture(hwnd, out)
        print(f"window={hwnd} size={w}x{h} printwindow={ok} -> {out}")
        return 0
    finally:
        # WM_CLOSE 走正常关闭流程（触发配置落盘），不要 TerminateProcess
        if hwnd:
            user32.PostMessageW(hwnd, 0x0010, 0, 0)
        try:
            proc.wait(timeout=15)
        except subprocess.TimeoutExpired:
            proc.kill()


if __name__ == "__main__":
    sys.exit(main())
