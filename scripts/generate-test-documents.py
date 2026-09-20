# -*- coding: utf-8 -*-
"""生成 dowse 自用版功能验收用的测试资料。

只依赖标准库（zipfile 手搓 OOXML，不装 python-docx / openpyxl / python-pptx），
这样在任何机器上都能复现。产出的 docx/xlsx/pptx 都是结构合法的包，Word/Excel/
PowerPoint 能直接打开，同时文件名/内容里的"哨兵词"用于逐条核对验收条件。

用法：
    python make_fixtures.py [输出目录]
默认输出目录：本脚本同级的 fixtures/
"""

import os
import sys
import zipfile

OUT = sys.argv[1] if len(sys.argv) > 1 else os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")

# ---------------------------------------------------------------------------
# 哨兵词：所有验收核对都按这些词去搜，改词要同步改验收脚本
# ---------------------------------------------------------------------------
DOCX_SENTINEL = "季度对账哨兵词"
XLSX_SENTINEL = "季度对账哨兵词"
XLSX_ORPHAN = "从未被引用的孤儿字符串"
XLSX_NUMBER = "1234.5"
TXT_SENTINEL = "国标编码哨兵词"
MD_SENTINEL = "笔记哨兵词"
LONG_FAR_SENTINEL = "长文深处哨兵词"
LONG_BOUNDARY_SENTINEL = "跨块边界词"
PDF_SENTINEL = "PDF-SENTINEL-2026"

CHUNK = 8000          # 前端全文阅读的分块大小（PreviewPane.CHUNK_CHARS）
PREVIEW = 1500        # 后端预览窗口（searcher.PREVIEW_MAX_CHARS）


def write_zip(path, entries):
    """把 [(条目名, 内容)] 写成 zip。字节和字符串都接受。"""
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in entries:
            if isinstance(data, str):
                data = data.encode("utf-8")
            z.writestr(name, data)


# ---------------------------------------------------------------------------
# 1) docx：中文正文关键词
# ---------------------------------------------------------------------------
CONTENT_TYPES = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>
</Types>"""

ROOT_RELS = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/>
</Relationships>"""


def docx_xml(paragraphs):
    body = "".join(
        "<w:p><w:r><w:t xml:space=\"preserve\">{}</w:t></w:r></w:p>".format(p) for p in paragraphs
    )
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">'
        "<w:body>{}</w:body></w:document>".format(body)
    )


def make_docx(path):
    write_zip(
        path,
        [
            ("[Content_Types].xml", CONTENT_TYPES),
            ("_rels/.rels", ROOT_RELS),
            (
                "word/document.xml",
                docx_xml(
                    [
                        "2026 年第三季度经营情况说明",
                        "{}核对完成，账实相符。".format(DOCX_SENTINEL),
                        "本季度营业收入 1,234,567 元，同比增长 12.3%。",
                        "下一步重点：应收账款回收与库存周转。",
                    ]
                ),
            ),
        ],
    )


# ---------------------------------------------------------------------------
# 2) xlsx：工作表声明顺序 + 数字 / 共享字符串 / 内联串 / 布尔 / 坐标
# ---------------------------------------------------------------------------
def make_xlsx(path):
    # 关键：workbook.xml 里"汇总表"声明在前但指向 sheet2.xml，"明细表"声明在后却
    # 指向 sheet1.xml。按文件名字典序/自然序都会把 sheet1 排前面，只有真的读
    # workbook.xml + rels 才能得到正确顺序。
    workbook = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="汇总表" sheetId="1" r:id="rId1"/>
    <sheet name="明细表" sheetId="2" r:id="rId2"/>
  </sheets>
</workbook>"""

    wb_rels = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"""

    # 共享字符串表：第 0 项被引用，第 1 项是"孤儿"（不该进正文），第 2 项是富文本串
    shared = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="3" uniqueCount="3">
  <si><t>{}</t></si>
  <si><t>{}</t></si>
  <si><r><t>富文本</t></r><r><t>拼接串</t></r></si>
</sst>""".format(XLSX_SENTINEL, XLSX_ORPHAN)

    # sheet2.xml = 汇总表（按声明应第一个输出）
    sheet2 = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="n"><v>{}</v></c>
      <c r="C1" t="b"><v>1</v></c>
      <c r="D1" t="inlineStr"><is><t>内联文本</t></is></c>
      <c r="E1" t="s"><v>2</v></c>
      <c r="F1"><f>SUM(B1,1)</f><v>1235.5</v></c>
    </row>
  </sheetData>
</worksheet>""".format(XLSX_NUMBER)

    # sheet1.xml = 明细表（按声明应第二个输出）
    sheet1 = """<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>明细表独有词-明细哨兵</t></is></c></row>
    <row r="2"><c r="A2" t="n"><v>98765</v></c></row>
  </sheetData>
</worksheet>"""

    write_zip(
        path,
        [
            (
                "[Content_Types].xml",
                CONTENT_TYPES.replace(
                    '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>',
                    '<Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>',
                ),
            ),
            ("_rels/.rels", ROOT_RELS.replace("word/document.xml", "xl/workbook.xml")),
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", wb_rels),
            ("xl/sharedStrings.xml", shared),
            ("xl/worksheets/sheet1.xml", sheet1),
            ("xl/worksheets/sheet2.xml", sheet2),
        ],
    )


# ---------------------------------------------------------------------------
# 3) pptx：10 页，声明页序刻意打乱（既非字典序也非数字序）
# ---------------------------------------------------------------------------
# 展示顺位 1..10 → 正文所在文件
PPT_ORDER = [
    ("第一顺位哨兵", 10),
    ("第二顺位哨兵", 2),
    ("第三顺位哨兵", 1),
    ("第四顺位哨兵", 3),
    ("第五顺位哨兵", 4),
    ("第六顺位哨兵", 5),
    ("第七顺位哨兵", 6),
    ("第八顺位哨兵", 7),
    ("第九顺位哨兵", 8),
    ("第十顺位哨兵", 9),
]


def slide_xml(text):
    return (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
        ' xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">'
        "<p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{}</a:t></a:r></a:p>"
        "</p:txBody></p:sp></p:spTree></p:cSld></p:sld>".format(text)
    )


def make_pptx(path):
    sld_ids = "".join(
        '<p:sldId id="{}" r:id="rId{}"/>'.format(256 + i, i + 1) for i in range(len(PPT_ORDER))
    )
    presentation = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"'
        ' xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"'
        ' xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">'
        "<p:sldIdLst>{}</p:sldIdLst></p:presentation>".format(sld_ids)
    )
    rels_items = "".join(
        '<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{}.xml"/>'.format(
            i + 1, file_no
        )
        for i, (_text, file_no) in enumerate(PPT_ORDER)
    )
    rels = (
        '<?xml version="1.0" encoding="UTF-8" standalone="yes"?>'
        '<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}</Relationships>'.format(
            rels_items
        )
    )

    entries = [
        (
            "[Content_Types].xml",
            CONTENT_TYPES.replace(
                '<Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/>',
                '<Override PartName="/ppt/presentation.xml" ContentType="application/vnd.openxmlformats-officedocument.presentationml.presentation.main+xml"/>',
            ),
        ),
        ("_rels/.rels", ROOT_RELS.replace("word/document.xml", "ppt/presentation.xml")),
        ("ppt/presentation.xml", presentation),
        ("ppt/_rels/presentation.xml.rels", rels),
    ]
    # 每页正文写进"文件号"对应的 slideN.xml：文本里同时带上顺位和文件号，
    # 核对时一眼能看出是不是按声明顺序读的。
    for idx, (text, file_no) in enumerate(PPT_ORDER):
        entries.append(
            ("ppt/slides/slide{}.xml".format(file_no), slide_xml("{}<文件 slide{}.xml>".format(text, file_no)))
        )
    write_zip(path, entries)


# ---------------------------------------------------------------------------
# 4) PDF：手写最小 PDF（含文字层）与扫描版（无文字层）
# ---------------------------------------------------------------------------
def build_pdf(text_or_none):
    """手写一个单页 PDF。text_or_none 为 None 时页面不含任何文字对象（模拟扫描版）。"""
    objects = []
    objects.append(b"<< /Type /Catalog /Pages 2 0 R >>")

    if text_or_none is None:
        content = b"q 200 0 0 200 100 400 cm /Im0 Do Q\n"
        objects.append(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
        objects.append(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
        )
        objects.append(b"<< /Length %d >>\nstream\n" % len(content) + content + b"endstream")
        # 一张 1x1 的占位图像流，只是让页面"有内容且没有文字层"
        img = b"\x00\x00\x00"
        objects.append(
            b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB "
            b"/BitsPerComponent 8 /Length %d >>\nstream\n" % len(img) + img + b"\nendstream"
        )
    else:
        escaped = text_or_none.replace("\\", r"\\").replace("(", r"\(").replace(")", r"\)")
        stream = ("BT /F1 24 Tf 72 700 Td ({}) Tj ET\n".format(escaped)).encode("latin-1")
        objects.append(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")
        objects.append(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] "
            b"/Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
        )
        objects.append(b"<< /Length %d >>\nstream\n" % len(stream) + stream + b"endstream")
        objects.append(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")

    out = bytearray(b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n")
    offsets = []
    for i, body in enumerate(objects, start=1):
        offsets.append(len(out))
        out += b"%d 0 obj\n" % i + body + b"\nendobj\n"

    xref_pos = len(out)
    out += b"xref\n0 %d\n" % (len(objects) + 1)
    out += b"0000000000 65535 f \n"
    for off in offsets:
        out += b"%010d 00000 n \n" % off
    out += b"trailer\n<< /Size %d /Root 1 0 R >>\nstartxref\n%d\n%%%%EOF\n" % (
        len(objects) + 1,
        xref_pos,
    )
    return bytes(out)


# ---------------------------------------------------------------------------
# 5) 长文档：超出预览窗口 + 关键词跨分块边界
# ---------------------------------------------------------------------------
def make_long_md(path):
    head_len = CHUNK - 3  # 让哨兵词正好从第 7997 个字符开始，横跨 8000 这个分块边界
    head = "起" * head_len
    after_boundary = "接" * (27000 - head_len - len(LONG_BOUNDARY_SENTINEL))
    tail = "尾" * 2000
    content = head + LONG_BOUNDARY_SENTINEL + after_boundary + LONG_FAR_SENTINEL + tail

    far_at = content.index(LONG_FAR_SENTINEL)
    boundary_at = content.index(LONG_BOUNDARY_SENTINEL)
    assert boundary_at < CHUNK < boundary_at + len(LONG_BOUNDARY_SENTINEL), (
        "跨块哨兵词必须横跨 %d 边界，实际 %d..%d" % (CHUNK, boundary_at, boundary_at + len(LONG_BOUNDARY_SENTINEL))
    )
    assert far_at > PREVIEW, "远端哨兵词必须在预览窗口(%d)之外，实际 %d" % (PREVIEW, far_at)
    assert far_at // CHUNK >= 3, "远端哨兵词应落在第 3 块之后，实际第 %d 块" % (far_at // CHUNK)

    with open(path, "w", encoding="utf-8") as f:
        f.write(content)

    print("  长文档：总字符 {}，跨块词@{}-{}（跨 {}），远端词@{}（第 {} 块）".format(
        len(content), boundary_at, boundary_at + len(LONG_BOUNDARY_SENTINEL), CHUNK,
        far_at, far_at // CHUNK,
    ))


def main():
    os.makedirs(OUT, exist_ok=True)
    print("输出目录: {}".format(OUT))

    make_docx(os.path.join(OUT, "季度报告.docx"))
    print("  docx：{} + 数字正文".format(DOCX_SENTINEL))

    make_xlsx(os.path.join(OUT, "销售数据.xlsx"))
    print("  xlsx：声明顺序 汇总表(→sheet2.xml) 在 明细表(→sheet1.xml) 之前；含数字/共享串/内联串/布尔/公式缓存")

    make_pptx(os.path.join(OUT, "产品演示.pptx"))
    print("  pptx：10 页，声明页序 = slide10,2,1,3,4,5,6,7,8,9")

    # txt 故意用 GBK 编码：验证编码探测（Windows 中文 txt 的常见情况）
    with open(os.path.join(OUT, "说明文件.txt"), "wb") as f:
        f.write(
            ("这是一份用于验证编码探测的说明文件。{}\n"
             "它按国标 GBK 编码保存，直接按 UTF-8 读会变乱码。\n").format(TXT_SENTINEL).encode("gbk")
        )
    print("  txt：GBK 编码，{}".format(TXT_SENTINEL))

    with open(os.path.join(OUT, "笔记.md"), "w", encoding="utf-8") as f:
        f.write("# 随手笔记\n\n{}：😀emoji 与中英混排 Rate-Limiter 都要能搜到。\n".format(MD_SENTINEL))
    print("  md：{} + emoji".format(MD_SENTINEL))

    make_long_md(os.path.join(OUT, "长文档.md"))

    with open(os.path.join(OUT, "文字版.pdf"), "wb") as f:
        f.write(build_pdf(PDF_SENTINEL))
    print("  pdf：文字版，{}".format(PDF_SENTINEL))

    with open(os.path.join(OUT, "扫描版.pdf"), "wb") as f:
        f.write(build_pdf(None))
    print("  pdf：扫描版（无文字层）→ 预期 err:no-text")

    with open(os.path.join(OUT, "旧版合同.doc"), "wb") as f:
        f.write(bytes([0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1]) + b"\x00" * 4096)
    print("  旧格式：.doc → 预期 err:unsupported")

    with open(os.path.join(OUT, "损坏的报告.docx"), "wb") as f:
        f.write(b"this is definitely not a zip archive")
    print("  损坏文件：损坏的报告.docx → 预期 err:corrupted")

    with open(os.path.join(OUT, "空文件.txt"), "wb") as f:
        f.write(b"")
    print("  空文件：空文件.txt → 预期 err:no-text")

    print("完成，共 {} 个文件".format(len(os.listdir(OUT))))


if __name__ == "__main__":
    main()
