"""테스트용 TXT 생성기 (SPEC §10 테스트 코퍼스).

만드는 파일 (testdata/out/, git 제외):
  utf8.txt         한글·영문·이모지·탭, 줄바꿈 CRLF/LF/CR 섞임
  cp949.txt        같은 내용, CP949 (FR-T01 chardetng 경로)
  utf16le.txt      같은 내용, UTF-16 LE BOM
  utf16be.txt      같은 내용, UTF-16 BE BOM
  longline.txt     5MB짜리 한 줄(minified 흉내) 앞뒤로 짧은 줄 (FR-T06)
  big.txt          약 64MB, 100만 줄 이상 (mmap·색인 경로, --big으로 크기 지정)
  huge.txt         --huge 지정 시 약 1GB (NFR-P04)

사용: python testdata/gen_txt.py [--huge]
"""

import sys
from pathlib import Path

OUT = Path(__file__).parent / "out"

BODY = [
    "LeafViewer TXT 보기 시험 문서입니다.",
    "가나다라마바사 아자차카타파하 — 한글 대체 폰트(맑은 고딕) 확인.",
    "The quick brown fox jumps over the lazy dog. 0123456789",
    "\t탭\t문자\t폭\t(기본 4칸)",
    "이모지 😀 🍃 와 결합 문자 é é",
    "찾기 시험: Needle needle NEEDLE 바늘 바늘",
    "",
]


def text_mixed(n: int) -> str:
    out = []
    ends = ["\r\n", "\n", "\r"]
    for i in range(n):
        line = f"{i + 1:05d}  " + BODY[i % len(BODY)]
        out.append(line + ends[i % 3 if i % 50 == 49 else 0])
    return "".join(out)


def write_big(path: Path, target_bytes: int) -> None:
    line_tmpl = "{:09d} 대용량 줄 색인 시험 — the quick brown fox jumps over the lazy dog needle\r\n"
    chunk_lines = 20000
    written = 0
    i = 0
    with open(path, "wb") as f:
        while written < target_bytes:
            buf = "".join(line_tmpl.format(i + k) for k in range(chunk_lines)).encode("utf-8")
            f.write(buf)
            written += len(buf)
            i += chunk_lines
    print(f"  {path.name}: {written / 1048576:.0f}MB, {i:,}줄")


def main() -> None:
    OUT.mkdir(exist_ok=True)
    t = text_mixed(2000)
    (OUT / "utf8.txt").write_bytes(t.encode("utf-8"))
    cp = t.encode("cp949", errors="replace").decode("cp949")  # CP949에 없는 글자는 ?로
    (OUT / "cp949.txt").write_bytes(cp.encode("cp949"))
    (OUT / "utf16le.txt").write_bytes(b"\xff\xfe" + t.encode("utf-16-le"))
    (OUT / "utf16be.txt").write_bytes(b"\xfe\xff" + t.encode("utf-16-be"))

    long_line = ("function f(a,b){return a+b;}var 변수=\"한글\";" * 140000)[: 5 * 1024 * 1024]
    (OUT / "longline.txt").write_bytes(
        ("첫 줄\n" + long_line + "\n마지막 줄 needle\n").encode("utf-8")
    )
    write_big(OUT / "big.txt", 64 << 20)
    if "--huge" in sys.argv:
        write_big(OUT / "huge.txt", 1 << 30)
    print(f"written to {OUT}")


if __name__ == "__main__":
    main()
