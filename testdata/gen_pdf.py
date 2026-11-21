"""테스트용 PDF 생성기 (외부 패키지 없이 PDF 객체를 직접 쓴다).

만드는 파일 (testdata/out/, git 제외):
  outline.pdf   30쪽: 1쪽 차례(문서 내부 링크 + 외부 링크), 장/절 2단계 책갈피, 쪽마다 본문 텍스트
  large.pdf     500쪽: 스크롤·메모리 측정용 (NFR-P06, NFR-P07)
  mixed.pdf     크기가 다른 쪽 섞임 (A4 세로, A4 가로, Letter, 매우 큰 A0)

사용: python testdata/gen_pdf.py
"""

from pathlib import Path

OUT = Path(__file__).parent / "out"

LOREM = (
    "The quick brown fox jumps over the lazy dog. Pack my box with five dozen liquor jugs. "
    "Sphinx of black quartz, judge my vow. How vexingly quick daft zebras jump. "
    "LeafViewer renders this page with PDFium and draws it with Direct2D."
)


def esc(s: str) -> str:
    return s.replace("\\", "\\\\").replace("(", "\\(").replace(")", "\\)")


def wrap(text: str, width: int) -> list[str]:
    words, lines, cur = text.split(), [], ""
    for w in words:
        if len(cur) + len(w) + 1 > width:
            lines.append(cur)
            cur = w
        else:
            cur = f"{cur} {w}".strip()
    if cur:
        lines.append(cur)
    return lines


class Pdf:
    def __init__(self):
        self.objs: list[bytes | None] = []

    def reserve(self) -> int:
        self.objs.append(None)
        return len(self.objs)

    def set(self, num: int, body: str | bytes):
        self.objs[num - 1] = body.encode("latin-1") if isinstance(body, str) else body

    def add(self, body: str | bytes) -> int:
        n = self.reserve()
        self.set(n, body)
        return n

    def stream(self, data: str) -> int:
        raw = data.encode("latin-1")
        return self.add(b"<< /Length %d >>\nstream\n" % len(raw) + raw + b"\nendstream")

    def save(self, path: Path, root: int):
        out = bytearray(b"%PDF-1.7\n%\xe2\xe3\xcf\xd3\n")
        offsets = []
        for i, body in enumerate(self.objs, 1):
            offsets.append(len(out))
            out += b"%d 0 obj\n" % i + body + b"\nendobj\n"
        xref = len(out)
        out += b"xref\n0 %d\n0000000000 65535 f \n" % (len(self.objs) + 1)
        for off in offsets:
            out += b"%010d 00000 n \n" % off
        out += b"trailer\n<< /Size %d /Root %d 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (len(self.objs) + 1, root, xref)
        path.write_bytes(bytes(out))


def build(path: Path, sizes: list[tuple[float, float]], chapters_every: int, with_toc: bool):
    pdf = Pdf()
    catalog = pdf.reserve()
    pages_obj = pdf.reserve()
    outlines = pdf.reserve()
    font = pdf.add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")
    bold = pdf.add("<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica-Bold >>")
    page_ids = [pdf.reserve() for _ in sizes]
    n = len(sizes)

    for i, (w, h) in enumerate(sizes):
        chapter = i // chapters_every + 1
        ops = ["BT /F2 20 Tf 56 %.1f Td (%s) Tj ET" % (h - 72, esc(f"Chapter {chapter} - Page {i + 1}"))]
        y = h - 110
        for para in range(3):
            ops.append("BT /F2 13 Tf 56 %.1f Td (%s) Tj ET" % (y, esc(f"Section {chapter}.{para + 1}")))
            y -= 22
            for line in wrap(f"[p{i + 1}] " + LOREM + f" Unique marker page{i + 1:04d}.", int((w - 112) / 5.6)):
                ops.append("BT /F1 11 Tf 56 %.1f Td (%s) Tj ET" % (y, esc(line)))
                y -= 15
            y -= 12
        # 도형: 렌더링 확인용
        ops.append("0.2 0.5 0.9 rg 56 56 %.1f 8 re f" % (w - 112))
        annots = []
        if with_toc and i == 0:
            y -= 10
            ops.append("BT /F2 14 Tf 56 %.1f Td (Contents - click to jump) Tj ET" % y)
            y -= 20
            for c in range(min(8, (n - 1) // chapters_every + 1)):
                target = c * chapters_every
                label = f"Go to Chapter {c + 1} (page {target + 1})"
                ops.append("0 0 0.8 rg BT /F1 11 Tf 72 %.1f Td (%s) Tj ET 0 0 0 rg" % (y, esc(label)))
                annots.append(
                    pdf.add(
                        "<< /Type /Annot /Subtype /Link /Rect [70 %.1f 330 %.1f] /Border [0 0 0] "
                        "/Dest [%d 0 R /XYZ 0 %.1f null] >>" % (y - 3, y + 12, page_ids[target], sizes[target][1] - 60)
                    )
                )
                y -= 16
            label = "External: https://example.com"
            ops.append("0 0 0.8 rg BT /F1 11 Tf 72 %.1f Td (%s) Tj ET 0 0 0 rg" % (y, esc(label)))
            annots.append(
                pdf.add(
                    "<< /Type /Annot /Subtype /Link /Rect [70 %.1f 260 %.1f] /Border [0 0 0] "
                    "/A << /S /URI /URI (https://example.com) >> >>" % (y - 3, y + 12)
                )
            )
        content = pdf.stream("\n".join(ops))
        annot_str = (" /Annots [%s]" % " ".join(f"{a} 0 R" for a in annots)) if annots else ""
        pdf.set(
            page_ids[i],
            "<< /Type /Page /Parent %d 0 R /MediaBox [0 0 %.1f %.1f] /Contents %d 0 R "
            "/Resources << /Font << /F1 %d 0 R /F2 %d 0 R >> >>%s >>" % (pages_obj, w, h, content, font, bold, annot_str),
        )

    pdf.set(pages_obj, "<< /Type /Pages /Kids [%s] /Count %d >>" % (" ".join(f"{p} 0 R" for p in page_ids), n))

    # 책갈피: 장(Chapter) → 절(Section) 2단계
    chapters = list(range(0, n, chapters_every))
    ch_ids = [pdf.reserve() for _ in chapters]
    for ci, start in enumerate(chapters):
        sec_ids = [pdf.reserve() for _ in range(3)]
        for si, sid in enumerate(sec_ids):
            links = ""
            if si > 0:
                links += f" /Prev {sec_ids[si - 1]} 0 R"
            if si < 2:
                links += f" /Next {sec_ids[si + 1]} 0 R"
            sec_y = sizes[start][1] - 110 - si * 80
            pdf.set(
                sid,
                "<< /Title (%s) /Parent %d 0 R /Dest [%d 0 R /XYZ 0 %.1f null]%s >>"
                % (esc(f"Section {ci + 1}.{si + 1}"), ch_ids[ci], page_ids[start], sec_y, links),
            )
        links = ""
        if ci > 0:
            links += f" /Prev {ch_ids[ci - 1]} 0 R"
        if ci < len(chapters) - 1:
            links += f" /Next {ch_ids[ci + 1]} 0 R"
        pdf.set(
            ch_ids[ci],
            "<< /Title (%s) /Parent %d 0 R /Dest [%d 0 R /Fit] /First %d 0 R /Last %d 0 R /Count -3%s >>"
            % (esc(f"Chapter {ci + 1}"), outlines, page_ids[start], sec_ids[0], sec_ids[-1], links),
        )
    pdf.set(outlines, "<< /Type /Outlines /First %d 0 R /Last %d 0 R /Count %d >>" % (ch_ids[0], ch_ids[-1], len(ch_ids)))
    pdf.set(catalog, "<< /Type /Catalog /Pages %d 0 R /Outlines %d 0 R /PageMode /UseOutlines >>" % (pages_obj, outlines))
    pdf.save(path, catalog)
    print(f"{path} ({n} pages, {path.stat().st_size // 1024} KB)")


A4 = (595.0, 842.0)
A4_LAND = (842.0, 595.0)
LETTER = (612.0, 792.0)
A0 = (2384.0, 3370.0)

if __name__ == "__main__":
    OUT.mkdir(exist_ok=True)
    build(OUT / "outline.pdf", [A4] * 30, chapters_every=3, with_toc=True)
    build(OUT / "large.pdf", [LETTER] * 500, chapters_every=10, with_toc=True)
    build(OUT / "mixed.pdf", [A4, A4_LAND, LETTER, A0, A4], chapters_every=1, with_toc=True)
