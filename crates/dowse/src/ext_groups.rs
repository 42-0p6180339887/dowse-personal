//! 浮窗筛选下拉用的预设文件类型分组。
//!
//! 图片分组是为里程碑 4 OCR 预留的：当前文本抽取管线（见 extract.rs）不产出
//! 图片文档，选中这组时索引里天然没有匹配项，是正常的空结果，不是错误——
//! schema v3 已经把 kind 字段捎带加上，就是为这天做准备（见 lib.rs 顶部注释）。

/// 文档类：常见办公 / 标记文档格式。
pub const DOC: &[&str] = &["md", "txt", "pdf", "docx", "xlsx", "pptx", "html"];

/// 代码类：参照 extract.rs 的纯文本白名单，挑出偏"编程语言/配置脚本"的那部分
/// （排掉 txt/md/csv 这类通用文本，它们已经在文档分组里）。
pub const CODE: &[&str] = &[
    "rs", "py", "go", "js", "ts", "jsx", "tsx", "java", "c", "h", "cpp", "hpp", "cs", "rb", "php",
    "lua", "vue", "json", "yaml", "yml", "toml", "sh", "ps1", "bat", "sql",
];

/// 图片类：里程碑 4 OCR 接入后才会真正入索引，现在选中这组只是搜不到东西。
pub const IMAGE: &[&str] = &["png", "jpg", "jpeg", "webp", "bmp"];

/// PDF 单独一组：本机资料里 PDF 占比通常最高（论文、扫描件、导出件），
/// 单独拎出来才有筛选意义。
pub const PDF: &[&str] = &["pdf"];

/// Word 类。刻意包含旧版 `.doc`/`.wps`——它们抽取不了正文，但会被索引成
/// `err:unsupported` 状态占位文档，按文件名仍可搜到。把它们纳入分组是为了
/// "这类文件确实存在"这件事可见，而不是让过滤后一片空白看起来像没收录。
pub const WORD: &[&str] = &["doc", "docx", "docm", "rtf", "odt", "wps"];

/// Excel 类。`csv` 归在这里而不是文本组——用户心智里它是"用表格软件打开的
/// 数据表"，且抽取管线同样按表格处理。
pub const EXCEL: &[&str] = &["xls", "xlsx", "xlsm", "xlsb", "csv", "ods"];

/// PPT 类。同 Word：旧版 `.ppt` 抽不了正文但会留下状态占位文档，一并纳入。
pub const PPT: &[&str] = &["ppt", "pptx", "pps", "ppsx", "odp"];

/// 纯文本类：笔记、日志、标记文档。跟 `CODE` 的区别是这组偏"人读的内容"，
/// `CODE` 偏编程语言与配置脚本；`md` 两边都不放重叠——它没有歧义地属于这里。
pub const TEXT: &[&str] = &["txt", "md", "log", "rst", "tex", "text"];

/// 按名字取预设分组的扩展名集合。`"all"`、`None`、未知名字都表示不筛选——
/// 浮窗前端传字符串过来，未知输入宽松地当"全部"处理，不报错。
///
/// # Examples
///
/// ```
/// use dowse::ext_group_by_name;
///
/// // "doc" 分组包含常见文档扩展名。
/// assert!(ext_group_by_name(Some("doc")).unwrap().contains(&"pdf"));
/// // "all"、未知名字、None 都表示不筛选。
/// assert_eq!(ext_group_by_name(Some("all")), None);
/// assert_eq!(ext_group_by_name(None), None);
/// ```
/// 可取的名字分两档：
/// - 粗粒度：`doc`（全部办公/标记文档）、`code`、`image`；
/// - 细粒度：`pdf`、`word`、`excel`、`ppt`、`text`——对应界面"类型"下拉里的
///   六个具体选项，比粗粒度"文档"更符合"我要找那份 Excel"的实际心智。
///
/// `"all"`、`None`、未知名字都表示不筛选——界面前端传字符串过来，未知输入宽松
/// 地当"全部"处理，不报错（新增分组也不会让旧前端炸掉）。
pub fn by_name(name: Option<&str>) -> Option<&'static [&'static str]> {
    match name {
        Some("doc") => Some(DOC),
        Some("code") => Some(CODE),
        Some("image") => Some(IMAGE),
        Some("pdf") => Some(PDF),
        Some("word") => Some(WORD),
        Some("excel") => Some(EXCEL),
        Some("ppt") => Some(PPT),
        Some("text") => Some(TEXT),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn by_name_maps_known_groups() {
        assert_eq!(by_name(Some("doc")), Some(DOC));
        assert_eq!(by_name(Some("code")), Some(CODE));
        assert_eq!(by_name(Some("image")), Some(IMAGE));
        assert_eq!(by_name(Some("pdf")), Some(PDF));
        assert_eq!(by_name(Some("word")), Some(WORD));
        assert_eq!(by_name(Some("excel")), Some(EXCEL));
        assert_eq!(by_name(Some("ppt")), Some(PPT));
        assert_eq!(by_name(Some("text")), Some(TEXT));
    }

    /// 界面"类型"下拉里的每个选项都必须真的能筛出东西：分组为空或名字拼错
    /// 会让下拉静默退化成"不筛选"，用户以为自己在筛 PDF 实际看到全部结果。
    #[test]
    fn every_ui_facing_group_is_non_empty() {
        for name in ["doc", "code", "image", "pdf", "word", "excel", "ppt", "text"] {
            let group = by_name(Some(name)).unwrap_or_else(|| panic!("{name} 应映射到某个分组"));
            assert!(!group.is_empty(), "{name} 分组不应为空");
            for ext in group {
                assert!(!ext.is_empty(), "{name} 分组里不应有空扩展名");
                assert_eq!(
                    *ext,
                    ext.to_ascii_lowercase(),
                    "{name} 分组的扩展名应统一小写（索引里存的是小写）"
                );
            }
        }
    }

    /// 旧格式的扩展名必须留在细分组里：它们是 `err:unsupported` 占位文档，
    /// 筛"Word"时应当可见（带状态提示），不能被过滤掉伪装成"没这个文件"。
    #[test]
    fn legacy_extensions_stay_visible_in_their_groups() {
        assert!(WORD.contains(&"doc"));
        assert!(EXCEL.contains(&"xls"));
        assert!(PPT.contains(&"ppt"));
    }

    #[test]
    fn by_name_falls_back_to_none_for_all_or_unknown() {
        assert_eq!(by_name(Some("all")), None);
        assert_eq!(by_name(Some("bogus")), None);
        assert_eq!(by_name(None), None);
    }
}
