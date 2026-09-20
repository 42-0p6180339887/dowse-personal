//! Office 文档（docx/xlsx/pptx）抽取 + 抽取失败状态的集成测试。
//!
//! 这里测的是**全链路**：程序化构造最小合法的 Office 包 → 走 `rebuild_index`
//! → 打开索引读回来核对。跟 `src/extract.rs` 里的单测互补——单测盯单个解析
//! 函数，这里盯"索引里到底存了什么、按路径读回来是什么"。
//!
//! 覆盖的关键契约：
//! 1. 三种 Office 格式的正文都能搜到（含中文哨兵词）；
//! 2. xlsx 工作表顺序按 `workbook.xml` 声明，不按文件名字典序；
//! 3. pptx 幻灯片顺序按 `presentation.xml` 声明，不按文件名字典序；
//! 4. 抽不出正文的文件仍以**状态占位文档**进索引：按文件名能搜到、预览带
//!    `err:xxx` 明确状态，既不伪装成功，也不无声消失。

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use dowse::{Searcher, rebuild_index};
use zip::write::SimpleFileOptions;

/// 逃生舱开关，跟 `tests/common/mod.rs::force_slow_lane_for_tests` 同一份文档、
/// 同一套 `Once` 理由——这个文件没有 `mod common`（不需要它的 tempdir 收尾
/// 辅助），本地补一份最小实现，避免为了一个函数把整个 common 模块引进来。
static FORCE_SLOW_LANE_INIT: std::sync::Once = std::sync::Once::new();

fn force_slow_lane_for_tests() {
    FORCE_SLOW_LANE_INIT.call_once(|| {
        // Safety: 全进程只有这一处写这个环境变量，`Once` 保证只执行一次；
        // 调用方约定"用 rebuild_index 之前先调这个函数"，不存在并发读者。
        unsafe {
            std::env::set_var("DOWSE_FORCE_SLOW_LANE", "1");
        }
    });
}

/// 建索引用的目标目录名不能带 "." 前缀——walk_index_files 会整棵跳过隐藏目录，
/// 而 tempfile 默认给临时目录起 ".tmpXXXX" 这种名字。
fn target_dir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("dowse-office-")
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

fn count_hits(index_dir: &Path, query: &str) -> usize {
    let searcher = Searcher::open(index_dir).unwrap();
    searcher.search(query, 50).unwrap().len()
}

/// 取索引里记录的某篇文档的精确路径：先用 `path:` 查一次再按文件名后缀挑。
///
/// **不要自己拼路径**（更不要用 `std::fs::canonicalize`）：Windows 上
/// `canonicalize` 会把路径规整成长名 + `\\?\` 前缀的 verbatim 写法，跟索引里
/// 存的那串（建索引时枚举目录得到的原样值）不是同一个字符串，按路径精确查询
/// 会一路查不到文档。
fn indexed_path(searcher: &Searcher, file_name: &str) -> Result<String> {
    let stem = file_name.split('.').next().unwrap_or(file_name);
    Ok(searcher
        .search(&format!("path:{stem}"), 20)?
        .into_iter()
        .find(|h| h.path.ends_with(file_name))
        .unwrap_or_else(|| panic!("索引里应按文件名找到 {file_name}"))
        .path)
}

/// 把一篇文档的整篇已索引正文读回来（分页上限给足，一次读完）。
fn indexed_content(searcher: &Searcher, path: &str) -> Result<String> {
    Ok(searcher
        .read_indexed_content_chunk(path, 0, 100_000)?
        .expect("文档应在索引里")
        .content)
}

// ---------------------------------------------------------------------------
// 1) 三种格式的正文都能搜到
// ---------------------------------------------------------------------------

const DOCX_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body><w:p><w:r><w:t>季度对账哨兵词 docx</w:t></w:r></w:p></w:body>
</w:document>"#;

const WORKBOOK_XML: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="数据表" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

const WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;

const SHARED_STRINGS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="1" uniqueCount="1">
  <si><t>季度对账哨兵词 xlsx</t></si>
</sst>"#;

const SHEET_WITH_SENTINEL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData>
</worksheet>"#;

const SLIDE_WITH_SENTINEL: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>季度对账哨兵词 pptx</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#;

#[test]
fn office_documents_are_indexed_and_searchable() -> Result<()> {
    force_slow_lane_for_tests();

    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    write_zip(
        &target.path().join("report.docx"),
        &[("word/document.xml", DOCX_XML)],
    );

    // xlsx/pptx 都给成真实包结构（workbook.xml + rels + sheet / presentation.xml +
    // rels + slide）：共享字符串表本身不是正文，只有被单元格引用的条目才算内容。
    write_zip(
        &target.path().join("data.xlsx"),
        &[
            ("xl/workbook.xml", WORKBOOK_XML),
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
            ("xl/sharedStrings.xml", SHARED_STRINGS),
            ("xl/worksheets/sheet1.xml", SHEET_WITH_SENTINEL),
        ],
    );

    write_zip(
        &target.path().join("slides.pptx"),
        &[("ppt/slides/slide1.xml", SLIDE_WITH_SENTINEL)],
    );

    let stats = rebuild_index(index_dir.path(), target.path())?;
    assert_eq!(stats.indexed, 3, "三个 Office 文件都应被索引");

    assert_eq!(
        count_hits(index_dir.path(), "季度对账哨兵词"),
        3,
        "中文哨兵词应三种格式都命中"
    );
    assert_eq!(
        count_hits(index_dir.path(), "不存在的词零零零"),
        0,
        "不存在的词不应命中"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 2) xlsx 工作表顺序按 workbook.xml 声明
// ---------------------------------------------------------------------------

#[test]
fn xlsx_workbook_declaration_order_is_respected() -> Result<()> {
    force_slow_lane_for_tests();

    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    // 声明顺序是"汇总表"在前，但它指向 sheet2.xml；"明细表"声明在后却指向
    // sheet1.xml。按文件名字典序/数值序都会把 sheet1 排前面——只有真的读
    // workbook.xml + rels 才能得到正确顺序。
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
    let sheet1 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>明细表内容</t></is></c></row></sheetData></worksheet>"#;
    let sheet2 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>汇总表内容</t></is></c></row></sheetData></worksheet>"#;

    write_zip(
        &target.path().join("order.xlsx"),
        &[
            ("xl/workbook.xml", workbook),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/worksheets/sheet1.xml", sheet1),
            ("xl/worksheets/sheet2.xml", sheet2),
        ],
    );

    rebuild_index(index_dir.path(), target.path())?;
    let searcher = Searcher::open(index_dir.path())?;
    let content = indexed_content(&searcher, &indexed_path(&searcher, "order.xlsx")?)?;

    let summary = content.find("汇总表内容").expect("汇总表内容应在正文里");
    let detail = content.find("明细表内容").expect("明细表内容应在正文里");
    assert!(
        summary < detail,
        "工作表顺序应按 workbook.xml 声明（汇总表在明细表前）: {content}"
    );
    assert!(content.contains("[工作表 汇总表]"), "应带工作表名: {content}");
    assert!(content.contains("[工作表 明细表]"), "应带工作表名: {content}");

    Ok(())
}

// ---------------------------------------------------------------------------
// 3) pptx 幻灯片顺序按 presentation.xml 声明
// ---------------------------------------------------------------------------

/// 展示顺位 1..10 的哨兵词（下标 0 是第 1 顺位）。
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

/// 展示顺位 1..10 → 正文所在的 slideN.xml 文件号。刻意既不是字典序
/// （那会是 1,10,2,3,…）也不是数值序（1,2,3,…,10）。
const SLIDE_FILES: [u32; 10] = [10, 2, 1, 3, 4, 5, 6, 7, 8, 9];

#[test]
fn pptx_presentation_order_is_respected() -> Result<()> {
    force_slow_lane_for_tests();

    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    let sld_ids: String = (0..SLIDE_FILES.len())
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

    let mut entries: Vec<(String, String)> = vec![
        ("ppt/presentation.xml".into(), presentation),
        ("ppt/_rels/presentation.xml.rels".into(), rels),
    ];
    for (i, file_no) in SLIDE_FILES.iter().enumerate() {
        entries.push((
            format!("ppt/slides/slide{file_no}.xml"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
       xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main">
  <p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{}&lt;文件 slide{file_no}.xml&gt;</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#,
                SLIDE_LABELS[i]
            ),
        ));
    }
    let entry_refs: Vec<(&str, &str)> = entries
        .iter()
        .map(|(name, body)| (name.as_str(), body.as_str()))
        .collect();
    write_zip(&target.path().join("deck.pptx"), &entry_refs);

    rebuild_index(index_dir.path(), target.path())?;
    let searcher = Searcher::open(index_dir.path())?;
    let content = indexed_content(&searcher, &indexed_path(&searcher, "deck.pptx")?)?;

    // 全部 10 页都在，且按展示顺位依次出现。
    assert_eq!(
        content.matches("[幻灯片 ").count(),
        10,
        "10 页都应输出: {content}"
    );
    let mut last = 0usize;
    for (i, label) in SLIDE_LABELS.iter().enumerate() {
        let at = content
            .find(label)
            .unwrap_or_else(|| panic!("第 {} 顺位（{label}）应在正文里: {content}", i + 1));
        assert!(
            at >= last,
            "页序应按 presentation.xml 声明依次递增，第 {} 顺位跑到前面去了: {content}",
            i + 1
        );
        last = at;
    }

    // 顺序真的来自声明：第 1 页来自 slide10.xml，第 2 页来自 slide2.xml。
    let slide10_at = content.find("文件 slide10.xml").expect("第 1 页来自 slide10.xml");
    let slide2_at = content.find("文件 slide2.xml").expect("第 2 页来自 slide2.xml");
    assert!(
        slide10_at < slide2_at,
        "若按文件名字典序，slide10 会排在 slide2 后面；正确结果相反: {content}"
    );
    assert!(
        content.starts_with("[幻灯片 1]\n第一顺位哨兵"),
        "第一页应是展示顺位第 1 页: {content}"
    );
    assert!(
        content.contains("[幻灯片 10]\n第十顺位哨兵"),
        "第 10 页应是展示顺位第 10 页: {content}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// 4) 抽不出正文的文件 → 状态占位文档
// ---------------------------------------------------------------------------

#[test]
fn corrupted_docx_is_indexed_as_status_placeholder() -> Result<()> {
    force_slow_lane_for_tests();

    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    fs::write(target.path().join("broken.docx"), b"not a real zip file")?;
    fs::write(
        target.path().join("good.md"),
        "正常文件应该照常入索引 alpha",
    )?;

    let stats = rebuild_index(index_dir.path(), target.path())?;
    assert_eq!(stats.indexed, 1, "只有 good.md 抽到了正文");
    assert_eq!(
        stats.skipped, 1,
        "损坏的 docx 没有正文可收录，仍计入跳过（但会写占位文档进索引）"
    );

    let searcher = Searcher::open(index_dir.path())?;
    assert_eq!(count_hits(index_dir.path(), "alpha"), 1);

    // 不无声消失：按文件名仍能搜到，预览给出明确状态而不是一页空白。
    let path = indexed_path(&searcher, "broken.docx")?;
    let preview = searcher
        .preview(&path, "broken")?
        .expect("占位文档应有预览结果");
    assert_eq!(preview.kind, "err:corrupted", "预览应带明确的失败状态");
    assert_eq!(preview.snippet, "", "占位文档没有正文");

    Ok(())
}

#[test]
fn legacy_office_format_is_indexed_as_unsupported_placeholder() -> Result<()> {
    force_slow_lane_for_tests();

    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    // 旧版二进制 Office（OLE2 复合文档）目前不支持解析，但对用户来说它是
    // "该能搜到的文档"，不能当不相干文件静默丢掉。
    fs::write(
        target.path().join("老合同.doc"),
        [0xD0u8, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1],
    )?;

    let stats = rebuild_index(index_dir.path(), target.path())?;
    assert_eq!(stats.indexed, 0, "旧版格式抽不出正文");
    assert_eq!(stats.skipped, 1);

    let searcher = Searcher::open(index_dir.path())?;
    let hits = searcher.search("老合同", 10)?;
    assert!(
        hits.iter().any(|h| h.path.ends_with("老合同.doc")),
        "旧版格式仍应能按文件名搜到: {:?}",
        hits.iter().map(|h| &h.path).collect::<Vec<_>>()
    );

    let path = indexed_path(&searcher, "老合同.doc")?;
    let preview = searcher.preview(&path, "老合同")?.expect("应有预览结果");
    assert_eq!(preview.kind, "err:unsupported");

    Ok(())
}

#[test]
fn empty_text_file_is_indexed_as_no_text_placeholder() -> Result<()> {
    force_slow_lane_for_tests();

    let index_dir = tempfile::tempdir()?;
    let target = target_dir();

    fs::write(target.path().join("空笔记.txt"), b"")?;
    fs::write(target.path().join("只有空白.md"), "   \n\t  ")?;

    let stats = rebuild_index(index_dir.path(), target.path())?;
    assert_eq!(stats.indexed, 0, "空白内容不算收录成功");
    assert_eq!(stats.skipped, 2);

    let searcher = Searcher::open(index_dir.path())?;
    for name in ["空笔记.txt", "只有空白.md"] {
        let path = indexed_path(&searcher, name)?;
        let preview = searcher.preview(&path, name)?.expect("应有预览结果");
        assert_eq!(preview.kind, "err:no-text", "{name} 应报无可提取文字");
    }

    Ok(())
}
