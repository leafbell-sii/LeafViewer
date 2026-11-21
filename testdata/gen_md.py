"""테스트용 MD 생성기 (SPEC §10 테스트 코퍼스: GFM 전 요소 MD, 깨진 MD, 대용량 MD).

만드는 파일 (testdata/out/md/, git 제외):
  gfm.md           GFM 전 요소: 제목·강조·목록(중첩·작업)·표·코드·인용·각주·자동 링크·원시 HTML·
                   front matter·이미지(PNG 투명·BMP·GIF·큰 그림·없는 그림·원격)·문서 간 링크
  other.md         gfm.md에서 링크로 여는 문서 (앵커 이동 확인용 제목들)
  sub/deep.md      하위 폴더 문서 (트리 펼치며 선택 동기화)
  img/*.png|bmp|gif 이미지 (표준 라이브러리만으로 생성)
  note.txt, data.zip  링크 대상: 지원 포맷 / 미지원 포맷(탐색기에서 보기)
  broken.md        닫히지 않은 코드 블록·깨진 표·이상한 중첩 (FR-V07)
  long.md          약 1MB 일반 문서 (NFR-P03: MD ≤ 100ms)
  big.md           약 6MB, 제목 수천 개 (스크롤·목차·가상화)
  huge.md          8MB 초과 (FR-M06: TXT 모드로 열고 Ctrl+U로 렌더링)

사용: python testdata/gen_md.py
"""

import struct
import zlib
from pathlib import Path

OUT = Path(__file__).parent / "out" / "md"


# ───── 이미지 (표준 라이브러리만) ─────

def png(path: Path, w: int, h: int, pixel) -> None:
    """pixel(x, y) -> (r, g, b, a)"""
    raw = bytearray()
    for y in range(h):
        raw.append(0)
        for x in range(w):
            raw.extend(pixel(x, y))

    def chunk(tag: bytes, data: bytes) -> bytes:
        c = tag + data
        return struct.pack(">I", len(data)) + c + struct.pack(">I", zlib.crc32(c) & 0xFFFFFFFF)

    data = b"\x89PNG\r\n\x1a\n"
    data += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    data += chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    data += chunk(b"IEND", b"")
    path.write_bytes(data)


def bmp(path: Path, w: int, h: int, pixel) -> None:
    row = (w * 3 + 3) & ~3
    body = bytearray()
    for y in range(h - 1, -1, -1):
        line = bytearray()
        for x in range(w):
            r, g, b = pixel(x, y)
            line.extend((b, g, r))
        line.extend(b"\0" * (row - len(line)))
        body.extend(line)
    header = struct.pack("<2sIHHI", b"BM", 54 + len(body), 0, 0, 54)
    info = struct.pack("<IiiHHIIiiII", 40, w, h, 1, 24, 0, len(body), 2835, 2835, 0, 0)
    path.write_bytes(header + info + body)


def gif(path: Path, w: int, h: int, frames) -> None:
    """2색 GIF 애니메이션 (첫 프레임만 표시되는지 확인). frames: [(x, y) -> 0|1]"""
    def lzw(indices):
        # 코드 크기 2, 매 픽셀 CLEAR로 끊는 단순 인코딩 (사전 없이도 유효)
        bits, nbits, out = 0, 0, bytearray()
        clear, end = 4, 5

        def emit(code, size=3):
            nonlocal bits, nbits
            bits |= code << nbits
            nbits += size
            while nbits >= 8:
                out.append(bits & 0xFF)
                bits >>= 8
                nbits -= 8

        for i in indices:
            emit(clear)
            emit(i)
        emit(end)
        if nbits:
            out.append(bits & 0xFF)
        blocks = bytearray()
        for k in range(0, len(out), 255):
            part = out[k:k + 255]
            blocks.append(len(part))
            blocks.extend(part)
        blocks.append(0)
        return bytes(blocks)

    data = bytearray(b"GIF89a")
    data += struct.pack("<HHBBB", w, h, 0x80, 0, 0)
    data += bytes((0x2E, 0x7D, 0x32, 0xFF, 0xD4, 0x00))  # 녹색, 노랑
    data += b"\x21\xFF\x0BNETSCAPE2.0\x03\x01\x00\x00\x00"
    for f in frames:
        data += b"\x21\xF9\x04\x00\x32\x00\x00\x00"
        data += b"\x2C" + struct.pack("<HHHHB", 0, 0, w, h, 0)
        data += b"\x02" + lzw([f(x, y) for y in range(h) for x in range(w)])
    data += b"\x3B"
    path.write_bytes(bytes(data))


def make_images() -> None:
    img = OUT / "img"
    img.mkdir(parents=True, exist_ok=True)
    # 투명 배경 위 잎 모양 원 (알파 확인)
    def leaf(x, y):
        dx, dy = x - 60, y - 60
        d = (dx * dx + dy * dy) ** 0.5
        if d < 50:
            return (46, 125, 50, 255)
        if d < 56:
            return (46, 125, 50, int(255 * (56 - d) / 6))
        return (0, 0, 0, 0)
    png(img / "leaf.png", 120, 120, leaf)
    # 2400px 폭 그림: 화면 폭으로 줄여 디코딩되는지 (FR-M03)
    png(img / "wide.png", 2400, 300, lambda x, y: ((x * 255) // 2400, (y * 255) // 300, 180, 255))
    # 배지처럼 작은 인라인 그림
    png(img / "badge.png", 80, 20, lambda x, y: (36, 41, 47, 255) if x < 40 else (31, 136, 61, 255))
    bmp(img / "grid.bmp", 64, 64, lambda x, y: (220, 220, 220) if (x // 8 + y // 8) % 2 else (90, 90, 160))
    gif(img / "anim.gif", 32, 32, [lambda x, y: 0, lambda x, y: 1])


GFM = """---
title: GFM 시험 문서
tags: [leaf, viewer]
---

# LeafViewer MD 시험 문서

이 문서는 **GFM 전 요소**를 담고 있습니다. *기울임*, ***굵은 기울임***, ~~취소선~~, `인라인 코드`,
그리고 한글·English·日本語·이모지 🍃😀 가 섞인 문단입니다. 줄 끝 공백 두 개로 만든{BR}
강제 줄바꿈과 백슬래시\\
줄바꿈도 있습니다.

[목차로 가는 링크](#목차-이동-시험) · [다른 문서](other.md) · [다른 문서의 앵커](other.md#세-번째-절) ·
[하위 폴더 문서](sub/deep.md) · [텍스트 파일](note.txt) · [ZIP 파일(탐색기)](data.zip) ·
[없는 파일](missing.md) · [외부 링크](https://example.com) · 자동 링크 https://github.com/rust-lang/rust 와
www.example.org 그리고 <mailto:leaf@example.com>

## 목록

- 첫째 항목
- 둘째 항목에 **굵게**와 [링크](https://example.com/a)
  - 중첩 항목 ◦
    - 더 깊은 항목 ▪
- [ ] 할 일
- [x] 끝낸 일

1. 순서 목록 하나
2. 둘
   1. 중첩 순서
   2. 중첩 순서 둘
10. 열 번째 (번호 이어짐 확인)

빈 줄 있는 목록:

- 느슨한 항목 A

  같은 항목의 두 번째 문단.

- 느슨한 항목 B

## 표

| 왼쪽 정렬 | 가운데 | 오른쪽 | 긴 설명 |
|:--|:--:|--:|--|
| 가 | 나 | 1,234 | 셀 안의 *강조*와 `코드` |
| 사과 | 🍎 | 56 | 아주 긴 셀 내용이 줄바꿈되어 여러 줄을 차지하는지 확인하는 문장입니다. 표 칸 폭은 내용에 맞춰 자동으로 나뉩니다. |
| | 빈 칸 | 7 | |

## 코드

```rust
fn main() {
\tlet greeting = "안녕하세요, LeafViewer!"; // 탭 들여쓰기
    println!("{greeting}");
}
```

    들여쓰기 코드 블록
    두 번째 줄

## 인용

> 인용문입니다. **강조**도 됩니다.
>
> > 중첩 인용.
>
> - 인용 안의 목록

## 이미지

투명 PNG(블록 이미지, 링크로 감쌈):

[![잎 그림](img/leaf.png)](https://example.com/leaf)

인라인 배지 ![badge](img/badge.png) 가 글 사이에 들어갑니다 ![badge](img/badge.png).

넓은 그림(화면 폭으로 축소):

![넓은 그림](img/wide.png)

BMP 와 GIF(첫 프레임):

![격자](img/grid.bmp) ![움직이는 그림](img/anim.gif)

없는 그림:

![없는 그림입니다](img/nothing.png)

원격 그림(기본 차단):

![원격 로고](https://example.com/logo.png)

## 원시 HTML

<div align="center">
  <b>HTML은 렌더링하지 않고</b> 원문을 표시합니다.
</div>

인라인 <kbd>Ctrl</kbd>+<kbd>U</kbd> 와 주석<!-- 보이지 않음 --> 처리.

## 각주

각주 참조[^note]와 두 번째[^2].

[^note]: 각주 내용입니다. **서식**도 됩니다.
[^2]: 두 번째 각주.

---

## 목차 이동 시험

### 셋째 단계

#### 넷째 단계

##### 다섯째 단계

###### 여섯째 단계

같은 제목 중복:

## 중복
## 중복

수식(렌더링하지 않음): $E = mc^2$

```math
\\int_0^1 x^2 dx
```
"""

OTHER = """# 다른 문서

[gfm.md로 돌아가기](gfm.md#목록)

## 첫 번째 절

{body}

## 두 번째 절

{body}

## 세 번째 절

앵커 링크로 이 제목이 화면 위에 와야 합니다.

{body}
"""

DEEP = """# 하위 폴더 문서

[상위 문서](../gfm.md) · [루트 기준 링크](/other.md#두-번째-절)

![상위 폴더 그림](../img/leaf.png)
"""

BROKEN = """# 깨진 문서

```
닫히지 않은 코드 블록

| a | b |
|---|
| 1 | 2 | 3 |

* [ ]
> > > - - - 1. # 이상한 중첩
[^없는각주]
<div>
*닫히지 않은 강조
"""


def para(i: int) -> str:
    return (
        f"문단 {i}: 가나다라마바사 아자차카타파하. The quick brown fox jumps over the lazy dog. "
        f"**굵게 {i}** 와 *기울임* 그리고 `code_{i}` 와 [링크](#제목-{i // 20}). "
        "한글과 영문이 섞인 문장이 자동 줄바꿈되는지 확인합니다. " * 2
    )


def make_long(target: int) -> str:
    out, size, i = [], 0, 0
    while size < target:
        if i % 20 == 0:
            s = f"## 제목 {i // 20}\n\n"
        elif i % 37 == 0:
            s = "```\n" + "\n".join(f"line {k}: let x = {k};" for k in range(12)) + "\n```\n\n"
        elif i % 53 == 0:
            s = "| a | b | c |\n|---|:-:|--:|\n" + "".join(f"| {k} | 셀 {k} | {k * 3} |\n" for k in range(8)) + "\n"
        elif i % 29 == 0:
            s = "".join(f"- 목록 항목 {k}\n" for k in range(6)) + "\n"
        else:
            s = para(i) + "\n\n"
        out.append(s)
        size += len(s.encode("utf-8"))
        i += 1
    return "".join(out)


def main() -> None:
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "sub").mkdir(exist_ok=True)
    make_images()
    # 줄 끝 공백 두 개(강제 줄바꿈)는 편집기가 지우기 쉬워 자리표시자로 넣는다
    (OUT / "gfm.md").write_text(GFM.replace("{BR}", "  "), encoding="utf-8")
    body = "\n\n".join(para(i) for i in range(30))
    (OUT / "other.md").write_text(OTHER.format(body=body), encoding="utf-8")
    (OUT / "sub" / "deep.md").write_text(DEEP, encoding="utf-8")
    (OUT / "note.txt").write_text("링크로 연 텍스트 파일입니다.\n", encoding="utf-8")
    (OUT / "data.zip").write_bytes(b"PK\x05\x06" + b"\0" * 18)
    (OUT / "broken.md").write_text(BROKEN, encoding="utf-8")
    (OUT / "long.md").write_text(make_long(1 << 20), encoding="utf-8")
    (OUT / "big.md").write_text(make_long(6 << 20), encoding="utf-8")
    (OUT / "huge.md").write_text(make_long(9 << 20), encoding="utf-8")
    for f in sorted(OUT.rglob("*")):
        if f.is_file():
            print(f"{f.relative_to(OUT)}  {f.stat().st_size:,} B")


if __name__ == "__main__":
    main()
