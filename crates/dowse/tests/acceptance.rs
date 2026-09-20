//! 自用版全文搜索的**验收测试**：按任务书的验收条件逐条落成断言。
//!
//! 跟其它集成测试的区别：这里刻意覆盖"真实文件形态"而不是单个解析函数——
//! 程序化造出 docx/xlsx/pptx/txt(GBK)/md/长文/文字版与扫描版 PDF/旧版 .doc/
//! 损坏 docx 一整套资料，走 `rebuild_index` 建索引，再从索引侧读回来核对：
//!
//! | 验收条件 | 对应用例 |
//! |---|---|
//! | 中文关键词能搜到正文 | `chinese_keywords_are_searchable_in_all_supported_formats` |
//! | 六种格式各一例 | 同上 |
//! | 长文档超出预览范围仍可读可查 | `long_document_is_readable_and_searchable_beyond_preview_window` |
//! | 跨块/中文/emoji 坐标正确 | `char_coordinates_are_unicode_scalars_for_cjk_and_emoji`、上一条 |
//! | Excel 数字/共享串/多表可追溯 | `xlsx_numbers_sheets_and_cell_refs_are_traceable` |
//! | 十页且调序 PPT 顺序正确 | `pptx_ten_slides_follow_presentation_order` |
//! | 快速切换查找词结果不串 | `repeated_find_calls_do_not_leak_between_needles` |
//! | 增量与搜索一致、修好文件替换占位 | `fixing_a_broken_file_replaces_its_status_placeholder` |
//! | 重启后索引保留 | `index_survives_reopen` |
//! | 损坏/不支持/无文字有明确状态 | `unreadable_files_report_explicit_status` |

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use dowse::{Searcher, rebuild_index};
use zip::write::SimpleFileOptions;

// ---------------------------------------------------------------------------
// 公共脚手架
// ---------------------------------------------------------------------------

static FORCE_SLOW_LANE_INIT: std::sync::Once = std::sync::Once::new();

/// 强制走 walkdir 慢车道：MFT 快车道在没有管理员的测试环境里本来就会降级，
/// 显式开关能让"路径写法"在所有机器上一致，测试结果可复现。
fn force_slow_lane_for_tests() {
    FORCE_SLOW_LANE_INIT.call_once(|| {
        // Safety: 全进程只有这一处写这个环境变量，`Once` 保证只执行一次。
        unsafe {
            std::env::set_var("DOWSE_FORCE_SLOW_LANE", "1");
        }
    });
}

fn target_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("dowse-accept-")
        .tempdir()
        .unwrap()
}

fn write_zip(path: &Path, entries: &[(&str, &str)]) {
    let file = fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default();
    for (name, content) in entries {
        zip.start_file(*name, options).unwrap();
        zip.write_all(content.as_bytes()).unwrap();
    }
    zip.finish().unwrap();
}

/// 取索引里记录的精确路径。不要用 `std::fs::canonicalize` 自己拼：Windows 上
/// 它会换成"长名 + `\\?\`"的另一套写法，和索引里存的那串对不上，按路径精确
/// 查询会一路返回 None。
fn indexed_path(searcher: &Searcher, file_name: &str) -> Result<String> {
    let stem = file_name.split('.').next().unwrap_or(file_name);
    Ok(searcher
        .search(&format!("path:{stem}"), 20)?
        .into_iter()
        .find(|h| h.path.ends_with(file_name))
        .unwrap_or_else(|| panic!("索引里应按文件名找到 {file_name}"))
        .path)
}

fn content_of(searcher: &Searcher, file_name: &str) -> Result<String> {
    let path = indexed_path(searcher, file_name)?;
    Ok(searcher
        .read_indexed_content_chunk(&path, 0, 100_000)?
        .expect("文档应在索引里")
        .content)
}

/// 这个文件名是否能被搜到（按文件名/路径）。
fn searchable_by_name(searcher: &Searcher, file_name: &str) -> Result<bool> {
    let stem = file_name.split('.').next().unwrap_or(file_name);
    Ok(searcher
        .search(&format!("path:{stem}"), 20)?
        .iter()
        .any(|h| h.path.ends_with(file_name)))
}

// ---------------------------------------------------------------------------
// 资料：五种办公/文本格式 + 长文 + 各种"抽不出文字"的文件
// ---------------------------------------------------------------------------

const DOCX_SENTINEL: &str = "季度对账哨兵词";
const XLSX_SENTINEL: &str = "季度对账哨兵词";
const TXT_SENTINEL: &str = "国标编码哨兵词";
const MD_SENTINEL: &str = "笔记哨兵词";
const PDF_SENTINEL: &str = "PDF-SENTINEL-2026";
const LONG_FAR_SENTINEL: &str = "长文深处哨兵词";
const LONG_BOUNDARY_SENTINEL: &str = "跨块边界词";

/// 长文总长度与关键位置：跨块词横跨 8000 边界，远端词落在第 3 块之后。
const CHUNK: usize = 8000;
const LONG_FAR_AT: usize = 27_000;

fn make_docx(path: &Path) {
    let document_xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>2026 年第三季度经营情况说明</w:t></w:r></w:p>
    <w:p><w:r><w:t>{DOCX_SENTINEL}核对完成，账实相符。</w:t></w:r></w:p>
    <w:p><w:r><w:t>本季度营业收入 1,234,567 元。</w:t></w:r></w:p>
  </w:body>
</w:document>"#
    );
    write_zip(path, &[("word/document.xml", &document_xml)]);
}

fn make_xlsx(path: &Path) {
    // 声明顺序：汇总表在前但指向 sheet2.xml；明细表在后却指向 sheet1.xml。
    let workbook = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="汇总表" sheetId="1" r:id="rId1"/>
    <sheet name="明细表" sheetId="2" r:id="rId2"/>
  </sheets>
</workbook>"#;
    let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet2.xml"/>
  <Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
    let shared = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="3" uniqueCount="3">
  <si><t>{XLSX_SENTINEL}</t></si>
  <si><t>从未被引用的孤儿字符串</t></si>
  <si><r><t>富文本</t></r><r><t>拼接串</t></r></si>
</sst>"#
    );
    let sheet2 = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="n"><v>1234.5</v></c>
      <c r="C1" t="b"><v>1</v></c>
      <c r="D1" t="inlineStr"><is><t>内联文本</t></is></c>
      <c r="E1" t="s"><v>2</v></c>
      <c r="F1"><f>SUM(B1,1)</f><v>1235.5</v></c>
    </row>
  </sheetData>
</worksheet>"#;
    let sheet1 = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1"><c r="A1" t="inlineStr"><is><t>明细表独有标记</t></is></c></row>
    <row r="2"><c r="A2" t="n"><v>98765</v></c></row>
  </sheetData>
</worksheet>"#;
    write_zip(
        path,
        &[
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/sharedStrings.xml", &shared),
            ("xl/worksheets/sheet1.xml", sheet1),
            ("xl/worksheets/sheet2.xml", sheet2),
        ],
    );
}

/// 十页 PPT，展示顺位 1..10 分别落在 slide10,2,1,3,4,5,6,7,8,9 —— 既不是字典序
/// （1,10,2,…）也不是数值序（1,2,3,…）。
const SLIDE_LABELS: [&str; 10] = [
    "第一顺位哨兵",
    "第二顺位哨兵",
    "第三顺位哨兵",
    "第四顺位哨兵",
    "第五顺位哨兵",
    "第六顺位哨兵",
    "第七顺位哨兵",
    "第八顺位哨兵",
    "第九顺位哨兵",
    "第十顺位哨兵",
];
const SLIDE_FILES: [u32; 10] = [10, 2, 1, 3, 4, 5, 6, 7, 8, 9];

fn make_pptx(path: &Path) {
    let sld_ids: String = (0..10)
        .map(|i| format!(r#"<p:sldId id="{}" r:id="rId{}"/>"#, 256 + i, i + 1))
        .collect();
    let presentation = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <p:sldIdLst>{sld_ids}</p:sldIdLst>
</p:presentation>"#
    );
    let rels_items: String = SLIDE_FILES
        .iter()
        .enumerate()
        .map(|(i, file_no)| {
            format!(
                r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide{file_no}.xml"/>"#,
                i + 1
            )
        })
        .collect();
    let rels = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{rels_items}</Relationships>"#
    );

    let mut owned: Vec<(String, String)> = vec![
        ("ppt/presentation.xml".into(), presentation),
        ("ppt/_rels/presentation.xml.rels".into(), rels),
    ];
    for (i, file_no) in SLIDE_FILES.iter().enumerate() {
        owned.push((
            format!("ppt/slides/slide{file_no}.xml"),
            format!(
                r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{}文件slide{file_no}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#,
                SLIDE_LABELS[i]
            ),
        ));
    }
    let refs: Vec<(&str, &str)> = owned
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_str()))
        .collect();
    write_zip(path, &refs);
}

/// 造一段长正文：跨块词横跨 CHUNK 边界，远端词落在很远的地方（第 3 块之后）。
fn long_document_text() -> String {
    let head = "起".repeat(CHUNK - 3);
    let filler_len = LONG_FAR_AT - (CHUNK - 3) - LONG_BOUNDARY_SENTINEL.chars().count();
    let text = format!(
        "{head}{LONG_BOUNDARY_SENTINEL}{}{LONG_FAR_SENTINEL}{}",
        "接".repeat(filler_len),
        "尾".repeat(2_000)
    );
    // `str::find` 给的是**字节**偏移，而检索层用的是 Unicode 字符坐标（一个汉字
    // 3 个字节），必须再折成字符下标才是断言里那一套数字。
    let boundary_at = text[..text.find(LONG_BOUNDARY_SENTINEL).unwrap()].chars().count();
    let far_at = text[..text.find(LONG_FAR_SENTINEL).unwrap()].chars().count();
    assert!(boundary_at < CHUNK, "跨块词应在边界前起头");
    assert!(
        boundary_at + LONG_BOUNDARY_SENTINEL.chars().count() > CHUNK,
        "跨块词必须真的横跨 {CHUNK} 这个边界"
    );
    assert_eq!(far_at, LONG_FAR_AT, "远端词的字符位置应和常量一致");
    text
}

/// 手写一个单页 PDF。`text` 为 None 时页面只有一张图、没有文字层（模拟扫描版）。
fn build_pdf(text: Option<&str>) -> Vec<u8> {
    let mut objects: Vec<Vec<u8>> = Vec::new();
    objects.push(b"<< /Type /Catalog /Pages 2 0 R >>".to_vec());

    if let Some(text) = text {
        let escaped = text.replace('\\', r"\\").replace('(', r"\(").replace(')', r"\)");
        let mut stream = format!("BT /F1 24 Tf 72 700 Td ({escaped}) Tj ET\n").into_bytes();
        objects.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
        objects.push(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        );
        objects.push(
            [
                format!("<< /Length {} >>\nstream\n", stream.len()).into_bytes(),
                std::mem::take(&mut stream),
                b"endstream".to_vec(),
            ]
            .concat(),
        );
        objects.push(b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_vec());
    } else {
        let content = b"q 200 0 0 200 100 400 cm /Im0 Do Q\n".to_vec();
        objects.push(b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec());
        objects.push(
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
              /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>"
                .to_vec(),
        );
        objects.push(
            [
                format!("<< /Length {} >>\nstream\n", content.len()).into_bytes(),
                content,
                b"endstream".to_vec(),
            ]
            .concat(),
        );
        objects.push(
            b"<< /Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceRGB \
              /BitsPerComponent 8 /Length 3 >>\nstream\n\x00\x00\x00\nendstream"
                .to_vec(),
        );
    }

    let mut out: Vec<u8> = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offsets = Vec::new();
    for (i, body) in objects.iter().enumerate() {
        offsets.push(out.len());
        out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\nendobj\n");
    }
    let xref_pos = out.len();
    out.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
    out.extend_from_slice(b"0000000000 65535 f \n");
    for offset in &offsets {
        out.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{}\n%%EOF\n",
            objects.len() + 1,
            xref_pos
        )
        .as_bytes(),
    );
    out
}

/// 造一整套验收资料，返回目标目录。
fn build_all_fixtures(dir: &Path) {
    make_docx(&dir.join("季度报告.docx"));
    make_xlsx(&dir.join("销售数据.xlsx"));
    make_pptx(&dir.join("产品演示.pptx"));

    // GBK 编码的中文 txt（Windows 上很常见，直接按 UTF-8 读会乱码）。
    let plain = format!("这是一份说明文件。{TXT_SENTINEL}，按国标编码保存。\n");
    let (gbk, _, unmappable) = encoding_rs::GBK.encode(&plain);
    assert!(!unmappable, "测试文本应能完整编码成 GBK");
    fs::write(dir.join("说明文件.txt"), &gbk).unwrap();

    fs::write(
        dir.join("笔记.md"),
        format!("# 随手笔记\n\n{MD_SENTINEL}：😀emoji 与中英混排 Rate-Limiter。\n"),
    )
    .unwrap();

    fs::write(dir.join("长文档.md"), long_document_text()).unwrap();

    fs::write(dir.join("文字版.pdf"), build_pdf(Some(PDF_SENTINEL))).unwrap();
    fs::write(dir.join("扫描版.pdf"), build_pdf(None)).unwrap();

    // 旧版二进制 Office：OLE2 复合文档头。
    fs::write(
        dir.join("旧版合同.doc"),
        [0xD0u8, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
    )
    .unwrap();

    fs::write(dir.join("损坏的报告.docx"), b"not a zip archive at all").unwrap();
    fs::write(dir.join("空文件.txt"), b"").unwrap();
}

// ---------------------------------------------------------------------------
// 验收用例
// ---------------------------------------------------------------------------

#[test]
fn chinese_keywords_are_searchable_in_all_supported_formats() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();
    build_all_fixtures(target.path());
    rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;

    // 【条件 1】中文关键词能搜到正文——不是只命中文件名。
    for (sentinel, file_name) in [
        (DOCX_SENTINEL, "季度报告.docx"),
        (XLSX_SENTINEL, "销售数据.xlsx"),
        (TXT_SENTINEL, "说明文件.txt"),
        (MD_SENTINEL, "笔记.md"),
    ] {
        let hits = searcher.search(sentinel, 10)?;
        assert!(
            hits.iter().any(|h| h.path.ends_with(file_name)),
            "中文词 {sentinel:?} 应命中 {file_name} 的正文，实际 {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    // 【条件 2】六种格式各一例都能搜到（txt 是 GBK 编码，能搜到说明编码探测对了；
    // pdf 是文字层，能搜到说明 PDF 抽取通了）。
    for (query, file_name) in [
        (DOCX_SENTINEL, "季度报告.docx"),
        (XLSX_SENTINEL, "销售数据.xlsx"),
        (PDF_SENTINEL, "文字版.pdf"),
        (TXT_SENTINEL, "说明文件.txt"),
        (MD_SENTINEL, "笔记.md"),
        ("第一顺位哨兵", "产品演示.pptx"),
    ] {
        let hits = searcher.search(query, 10)?;
        assert!(
            hits.iter().any(|h| h.path.ends_with(file_name)),
            "{file_name} 应能被搜到（查询 {query:?}），实际 {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
    }

    // 扫描版 PDF（无文字层）里的 ASCII 词也该能命中——它确实没有文字层，
    // 所以这里反过来断言"搜不到正文"，状态由下一条用例核对。
    assert_eq!(
        searcher.search(PDF_SENTINEL, 10)?.len(),
        1,
        "PDF 哨兵词只应命中文字版那一篇"
    );

    Ok(())
}

#[test]
fn long_document_is_readable_and_searchable_beyond_preview_window() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();
    build_all_fixtures(target.path());
    rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;
    let path = indexed_path(&searcher, "长文档.md")?;

    // 【条件 3】长文档超出原预览范围仍可读可查：
    // 预览窗口只有 1500 字，命中的词在 27000 字处——preview 必须能把窗口挪过去，
    // 而不是只能看到文档开头。
    let preview = searcher
        .preview(&path, LONG_FAR_SENTINEL)?
        .expect("长文档应有预览");
    assert!(
        preview.snippet.contains(LONG_FAR_SENTINEL),
        "预览应能覆盖到 27000 字处的命中，而不是只给开头"
    );

    // 文内查找：远端词按 Unicode 字符坐标返回精确位置。
    let found = searcher
        .find_in_document(&path, LONG_FAR_SENTINEL)?
        .expect("长文档应在索引里");
    assert_eq!(found.matches.len(), 1);
    assert_eq!(found.matches[0].start, LONG_FAR_AT);

    // 分块阅读：一页页读完，总字符数必须等于全文；页尾用 next_offset_chars
    // 明确告知"还有内容/全文已加载"。
    let expected_total = long_document_text().chars().count();
    let mut offset = 0usize;
    let mut collected = String::new();
    let mut pages = 0usize;
    loop {
        let chunk = searcher
            .read_indexed_content_chunk(&path, offset, CHUNK)?
            .expect("文档应在索引里");
        assert_eq!(chunk.total_chars, expected_total, "每页都应带上全文总字符数");
        assert_eq!(chunk.offset_chars, offset);
        pages += 1;
        collected.push_str(&chunk.content);
        match chunk.next_offset_chars {
            Some(next) => offset = next,
            None => break,
        }
        assert!(pages < 100, "分页不该无限循环");
    }
    assert_eq!(
        collected.chars().count(),
        expected_total,
        "按块读完应还原整篇正文"
    );
    assert_eq!(collected, long_document_text(), "读回来的正文应与原文一致");
    assert!(pages >= 4, "约 {} 字、每页 {CHUNK} 字，至少 4 页", expected_total);

    Ok(())
}

#[test]
fn char_coordinates_are_unicode_scalars_for_cjk_and_emoji() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    // emoji 1 个 Unicode 字符 = 2 个 UTF-16 单元 = 4 个 UTF-8 字节。三种口径
    // 完全不同的字符摆在最前面，后面紧跟 ASCII 与中文，坐标一错就露馅。
    fs::write(target.path().join("emoji.md"), "😀abc中文内容")?;
    rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;
    let path = indexed_path(&searcher, "emoji.md")?;

    // 【条件 4】中文/emoji 的高亮跳转要落在正确的字符位置上。
    let abc = searcher
        .find_in_document(&path, "abc")?
        .expect("文档应在索引里");
    assert_eq!(abc.matches, vec![1..4], "abc 应从第 1 个字符开始（不是字节 4）");
    assert_eq!(abc.total_chars, 8, "😀 + abc + 中文内容 = 8 个字符");

    let cn = searcher
        .find_in_document(&path, "内容")?
        .expect("文档应在索引里");
    assert_eq!(cn.matches, vec![6..8], "中文命中按字符坐标（不是 UTF-16 的 8..12）");

    let emoji = searcher
        .find_in_document(&path, "😀")?
        .expect("文档应在索引里");
    assert_eq!(emoji.matches, vec![0..1], "emoji 自身占 1 个字符坐标");

    // 前端要按这套坐标切片：块内容拆成码点数组后按字符区间切出来，必须正好是命中词。
    let chunk = searcher
        .read_indexed_content_chunk(&path, 0, CHUNK)?
        .expect("文档应在索引里");
    let cps: Vec<char> = chunk.content.chars().collect();
    let sliced: String = cps[cn.matches[0].start..cn.matches[0].end].iter().collect();
    assert_eq!(sliced, "内容", "按字符坐标切片应正好切出命中词");

    Ok(())
}

#[test]
fn xlsx_numbers_sheets_and_cell_refs_are_traceable() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();
    build_all_fixtures(target.path());
    rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;

    // 【条件 5】Excel 的数字能被搜到。
    assert!(
        searcher
            .search("1234.5", 10)?
            .iter()
            .any(|h| h.path.ends_with("销售数据.xlsx")),
        "数字单元格 1234.5 应能被搜到"
    );

    let content = content_of(&searcher, "销售数据.xlsx")?;

    // 共享字符串按坐标输出，命中能追溯到具体单元格。
    assert!(content.contains("A1: 季度对账哨兵词"), "共享字符串应带坐标: {content}");
    assert!(content.contains("B1: 1234.5"), "数字应带坐标: {content}");
    assert!(content.contains("C1: TRUE"), "布尔应渲染成 TRUE: {content}");
    assert!(content.contains("D1: 内联文本"), "内联字符串应输出: {content}");
    assert!(content.contains("E1: 富文本拼接串"), "富文本共享串应拼成一个字符串: {content}");
    assert!(content.contains("F1: 1235.5"), "公式缓存值应输出: {content}");
    assert!(
        !content.contains("从未被引用的孤儿字符串"),
        "没被任何单元格引用的共享字符串不是正文: {content}"
    );

    // 多表顺序：按 workbook.xml 声明（汇总表在前），不是文件名字典序。
    let summary = content.find("[工作表 汇总表]").expect("应有汇总表");
    let detail = content.find("[工作表 明细表]").expect("应有明细表");
    assert!(summary < detail, "汇总表应排在明细表之前: {content}");
    assert!(content.contains("明细表独有标记"), "明细表内容也应被索引");

    Ok(())
}

#[test]
fn pptx_ten_slides_follow_presentation_order() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();
    build_all_fixtures(target.path());
    rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;
    let content = content_of(&searcher, "产品演示.pptx")?;

    // 【条件 6】十页都在，且顺序按 presentation.xml 声明而不是文件名。
    assert_eq!(content.matches("[幻灯片 ").count(), 10, "应输出 10 页: {content}");

    let mut last = 0usize;
    for label in SLIDE_LABELS {
        let at = content
            .find(label)
            .unwrap_or_else(|| panic!("{label} 应在正文里: {content}"));
        assert!(at >= last, "{label} 的顺序不对: {content}");
        last = at;
    }

    let slide10 = content.find("文件slide10").expect("第 1 页来自 slide10.xml");
    let slide2 = content.find("文件slide2").expect("第 2 页来自 slide2.xml");
    assert!(
        slide10 < slide2,
        "按文件名字典序 slide10 会排在 slide2 之后，正确顺序相反: {content}"
    );
    assert!(
        content.starts_with("[幻灯片 1]\n第一顺位哨兵"),
        "第 1 页应是展示顺位第 1 页: {content}"
    );
    assert!(
        content.contains("[幻灯片 10]\n第十顺位哨兵"),
        "最后一页应是展示顺位第 10 页: {content}"
    );

    Ok(())
}

#[test]
fn unreadable_files_report_explicit_status() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();
    build_all_fixtures(target.path());
    let stats = rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;

    // 【条件 9】损坏/不支持/无文字的文件有明确状态，不伪装成功也不无声消失。
    for (file_name, expected_kind) in [
        ("损坏的报告.docx", "err:corrupted"),
        ("旧版合同.doc", "err:unsupported"),
        ("扫描版.pdf", "err:no-text"),
        ("空文件.txt", "err:no-text"),
    ] {
        assert!(
            searchable_by_name(&searcher, file_name)?,
            "{file_name} 应仍能按文件名搜到（不能无声消失）"
        );
        let path = indexed_path(&searcher, file_name)?;
        let preview = searcher
            .preview(&path, file_name)?
            .unwrap_or_else(|| panic!("{file_name} 应有预览结果"));
        assert_eq!(preview.kind, expected_kind, "{file_name} 的状态码不对");
        assert_eq!(preview.snippet, "", "{file_name} 不该有正文");
    }

    // 有内容的文件仍是正常 text 文档，四篇"抽不出正文"都计入跳过。
    let good = indexed_path(&searcher, "笔记.md")?;
    assert_eq!(searcher.preview(&good, MD_SENTINEL)?.unwrap().kind, "text");
    assert_eq!(
        stats.indexed, 7,
        "docx/xlsx/pptx/txt/md/长文/文字版 pdf 共 7 篇有正文"
    );
    assert_eq!(stats.skipped, 4, "损坏/旧版/扫描版/空文件 4 篇按跳过计");

    Ok(())
}

#[test]
fn fixing_a_broken_file_replaces_its_status_placeholder() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    let docx = target.path().join("报告.docx");
    fs::write(&docx, b"not a zip").unwrap();
    rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;
    let path = indexed_path(&searcher, "报告.docx")?;
    assert_eq!(
        searcher.preview(&path, "报告")?.unwrap().kind,
        "err:corrupted",
        "先前的损坏状态应被记录"
    );
    drop(searcher);

    // 【条件 8】把文件修好再重建，占位文档应被正常文档替换（同一路径先删后加），
    // 状态和内容都要跟上——不是留着一篇旧的占位文档。
    make_docx(&docx);
    rebuild_index(index_dir.path(), target.path())?;
    let searcher = Searcher::open(index_dir.path())?;
    let path = indexed_path(&searcher, "报告.docx")?;
    let preview = searcher.preview(&path, DOCX_SENTINEL)?.unwrap();
    assert_eq!(preview.kind, "text", "修好后应变成正常文档");
    assert!(
        preview.snippet.contains(DOCX_SENTINEL),
        "修好后正文应可搜: {}",
        preview.snippet
    );

    Ok(())
}

#[test]
fn index_survives_reopen() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();
    build_all_fixtures(target.path());
    rebuild_index(index_dir.path(), target.path())?;

    // 【条件 9】重启后索引保留：重新打开同一个索引目录（模拟程序重启），
    // 搜索结果和正文都应还在。
    let before = Searcher::open(index_dir.path())?;
    let before_count = before.search("季度对账哨兵词", 50)?.len();
    let before_content = content_of(&before, "季度报告.docx")?;
    drop(before);

    let after = Searcher::open(index_dir.path())?;
    assert_eq!(
        after.search("季度对账哨兵词", 50)?.len(),
        before_count,
        "重开后搜索结果应一致"
    );
    assert_eq!(
        content_of(&after, "季度报告.docx")?,
        before_content,
        "重开后已索引正文应一致"
    );

    Ok(())
}

#[test]
fn repeated_find_calls_do_not_leak_between_needles() -> Result<()> {
    force_slow_lane_for_tests();
    let index_dir = tempfile::tempdir()?;
    let target = target_dir();
    build_all_fixtures(target.path());
    rebuild_index(index_dir.path(), target.path())?;

    let searcher = Searcher::open(index_dir.path())?;
    let path = indexed_path(&searcher, "长文档.md")?;

    // 【条件 7】快速切换查找词时不串结果：同一篇文档上交替查两个词，每次都必须
    // 返回该词自己的命中，不能把上一次的结果带出来。
    // （前端那侧的"切文件/换词就作废旧请求 + token 丢弃过期响应"由
    // PreviewPane 的 $effect 负责，这里钉住"后端本身无状态、不会串"。）
    let far = searcher.find_in_document(&path, LONG_FAR_SENTINEL)?;
    let boundary = searcher.find_in_document(&path, LONG_BOUNDARY_SENTINEL)?;
    let far_again = searcher.find_in_document(&path, LONG_FAR_SENTINEL)?;

    assert_eq!(far, far_again, "同一次查询重复执行结果应完全一致");
    let far = far.unwrap();
    let boundary = boundary.unwrap();
    assert_eq!(far.matches, vec![LONG_FAR_AT..LONG_FAR_AT + LONG_FAR_SENTINEL.chars().count()]);
    let boundary_start = CHUNK - 3;
    assert_eq!(
        boundary.matches,
        vec![boundary_start..boundary_start + LONG_BOUNDARY_SENTINEL.chars().count()]
    );
    assert_eq!(far.total_chars, boundary.total_chars, "同一篇文档总字符数应一致");

    // 跨块命中：把命中覆盖到的块都读出来拼在一起，必须包含完整关键词。
    let hit = boundary.matches[0].clone();
    let first_offset = (hit.start / CHUNK) * CHUNK;
    let mut offset = first_offset;
    let mut combined = String::new();
    loop {
        let chunk = searcher
            .read_indexed_content_chunk(&path, offset, CHUNK)?
            .expect("文档应在索引里");
        combined.push_str(&chunk.content);
        let covered_to = offset + chunk.returned_chars;
        match chunk.next_offset_chars {
            Some(next) if covered_to < hit.end => offset = next,
            _ => break,
        }
    }
    assert!(
        combined.contains(LONG_BOUNDARY_SENTINEL),
        "跨块关键词拼起来必须完整: {combined:?}"
    );

    Ok(())
}
