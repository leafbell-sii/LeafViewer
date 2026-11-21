"""테스트용 이미지 생성기 (M5 이미지 포맷, SPEC §10 테스트 코퍼스).

만드는 파일 (testdata/out/img/, git 제외):
  big.png        6000×4000 격자 (원본 96MB → 고배율에서 화면 근처만 디코딩하는지)
  wide.png       8000×600 (가로 스크롤)
  pixel.png      16×16 도트 (확대 시 픽셀이 뭉개지지 않는지)
  alpha.png      투명 배경 (다크 테마에서 배경 위에 보이는지)
  grid.bmp, anim.gif   BMP, GIF 애니메이션(첫 프레임만)
  rot6.jpg       EXIF 방향 6(시계 90°) — 화살표가 위를 가리키면 정상. JPEG는 PowerShell(System.Drawing)로 만든다
  noext          PNG를 확장자 없이 (내용 판별 sniff)
  broken.png     잘린 PNG (오류 화면)

사용: python testdata/gen_img.py
"""

import struct
import subprocess
import zlib
from pathlib import Path

from gen_md import bmp, gif, png

OUT = Path(__file__).parent / "out" / "img"


def png_rows(path: Path, w: int, h: int, row) -> None:
    """row(y) -> RGB 바이트 (w*3). 큰 그림을 빨리 만들려고 행 단위로"""
    comp = zlib.compressobj(6)
    idat = bytearray()
    for y in range(h):
        idat += comp.compress(b"\0" + row(y))
    idat += comp.flush()

    def chunk(tag: bytes, data: bytes) -> bytes:
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    data = b"\x89PNG\r\n\x1a\n"
    data += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    data += chunk(b"IDAT", bytes(idat))
    data += chunk(b"IEND", b"")
    path.write_bytes(data)


def grid(w: int, h: int, cell: int, major: int):
    """칸마다 색이 다른 격자. major px마다 굵은 선 — 확대했을 때 세부가 선명한지 보인다"""
    cache = {}

    def row(y: int) -> bytes:
        on_major = y % major < 4
        on_minor = y % cell == 0
        key = (y // cell, on_major, on_minor)
        if key not in cache:
            by = y // cell
            line = bytearray()
            for x in range(w):
                if on_major or x % major < 4:
                    line += b"\x20\x20\x20"
                elif on_minor or x % cell == 0:
                    line += b"\x90\x90\x90"
                else:
                    bx = x // cell
                    line += bytes(((bx * 37) % 200 + 40, (by * 53) % 200 + 40, ((bx + by) * 29) % 200 + 40))
            cache[key] = bytes(line)
        return cache[key]

    return row


EXIF_JPEG_PS = r"""
Add-Type -AssemblyName System.Drawing
$b = New-Object System.Drawing.Bitmap 300, 200
$g = [System.Drawing.Graphics]::FromImage($b)
$g.Clear([System.Drawing.Color]::White)
$pts = [System.Drawing.Point[]]@((New-Object System.Drawing.Point 30,100),(New-Object System.Drawing.Point 130,30),(New-Object System.Drawing.Point 130,170))
$g.FillPolygon([System.Drawing.Brushes]::SeaGreen, $pts)
$g.FillRectangle([System.Drawing.Brushes]::SeaGreen, 130, 80, 140, 40)
$g.Dispose()
$b.Save('{path}', [System.Drawing.Imaging.ImageFormat]::Jpeg)
"""


# 저장 방향으로는 화살표가 왼쪽을 가리킨다 → 방향 6(시계 90°)을 적용하면 위를 가리킨다


def add_exif_orientation(jpeg: bytes, value: int) -> bytes:
    """SOI 뒤에 방향 태그(0x0112) 하나만 있는 APP1 EXIF를 넣는다"""
    tiff = b"II*\0" + struct.pack("<I", 8) + struct.pack("<H", 1)
    tiff += struct.pack("<HHIHH", 0x0112, 3, 1, value, 0) + struct.pack("<I", 0)
    app1 = b"Exif\0\0" + tiff
    seg = b"\xFF\xE1" + struct.pack(">H", len(app1) + 2) + app1
    assert jpeg[:2] == b"\xFF\xD8"
    return jpeg[:2] + seg + jpeg[2:]


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    png_rows(OUT / "big.png", 6000, 4000, grid(6000, 4000, 50, 500))
    png_rows(OUT / "wide.png", 8000, 600, grid(8000, 600, 40, 400))
    png(OUT / "pixel.png", 16, 16, lambda x, y: (200, 40, 40, 255) if (x + y) % 2 else (250, 230, 60, 255))

    def ring(x, y):
        d = ((x - 100) ** 2 + (y - 100) ** 2) ** 0.5
        return (30, 30, 30, 255) if 60 < d < 90 else (0, 0, 0, 0)

    png(OUT / "alpha.png", 200, 200, ring)
    bmp(OUT / "grid.bmp", 64, 64, lambda x, y: (220, 220, 220) if (x // 8 + y // 8) % 2 else (90, 90, 160))
    gif(OUT / "anim.gif", 32, 32, [lambda x, y: 0, lambda x, y: 1])

    tmp = OUT / "_plain.jpg"
    subprocess.run(
        ["powershell", "-NoProfile", "-Command", EXIF_JPEG_PS.replace("{path}", str(tmp))],
        check=True,
    )
    (OUT / "rot6.jpg").write_bytes(add_exif_orientation(tmp.read_bytes(), 6))
    tmp.unlink()

    (OUT / "noext").write_bytes((OUT / "pixel.png").read_bytes())
    (OUT / "broken.png").write_bytes((OUT / "alpha.png").read_bytes()[:200])
    for f in sorted(OUT.iterdir()):
        print(f"{f.name}  {f.stat().st_size:,} B")


if __name__ == "__main__":
    main()
