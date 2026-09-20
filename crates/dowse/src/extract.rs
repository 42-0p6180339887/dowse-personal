use std::collections::HashMap;
use std::fs;
use std::io::Read;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::XmlVersion;
use quick_xml::events::Event;
use quick_xml::events::attributes::Attribute;

use crate::rules::IndexRules;

/// 按扩展名白名单认定的纯文本文件。
const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "log", "csv", "tsv", "json", "toml", "yaml", "yml", "ini", "cfg",
    "conf", "xml", "html", "htm", "sql", "sh", "ps1", "bat", "rs", "py", "go", "java", "js", "ts",
    "jsx", "tsx", "c", "h", "cpp", "hpp", "cs", "rb", "php", "lua", "vue",
];

/// 支持抽取的 Office Open XML 格式（docx/xlsx/pptx，本质都是 zip 包）。
const OFFICE_EXTS: &[&str] = &["docx", "xlsx", "pptx"];

/// 旧版二进制 Office 格式（OLE2 复合文档）：**尚未支持解析**，但和真正的
/// "不相干文件"（exe/dll 等）不同——用户会期待能搜到它们。所以这一类单独
/// 认定成"可抽取白名单"成员：占位入索引（kind=err:unsupported），按文件名
/// 能搜到，预览区给出"旧版格式暂不支持"的明确状态，而不是无声消失。
/// 对账/监听走 [`is_extractable`] 过滤时也会把它们算进来，索引里的占位
/// 文档和磁盘状态保持一致，不会被当孤儿删掉。
const LEGACY_OFFICE_EXTS: &[&str] = &["doc", "xls", "ppt"];

/// 抽取失败的分类原因。落到索引 `kind` 字段（`err:` 前缀 + 短码），前端
/// 预览区据此显示明确状态——不静默伪装成索引成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractFailure {
    /// 旧版二进制 Office 格式（.doc/.xls/.ppt），尚未支持，需另存为新格式。
    UnsupportedFormat,
    /// 单文件体积超过规则里 `max_file_mb` 的上限。
    Oversize,
    /// 文件被密码保护，读不出内容。
    Encrypted,
    /// 文件损坏/不是合法的对应格式，打不开或解析中断。
    Corrupted,
    /// 能解析，但没有任何可提取文字（如扫描版 PDF——图片 OCR 只处理图片
    /// 文件，扫描版 PDF 的文字提取不在第一阶段范围内）。
    NoText,
}

impl ExtractFailure {
    /// 写进索引 kind 字段的短码，`err:` 前缀让前端一眼区分状态文档和
    /// 正常的 "text"/"image" 文档。
    pub fn kind_code(self) -> &'static str {
        match self {
            Self::UnsupportedFormat => "err:unsupported",
            Self::Oversize => "err:oversize",
            Self::Encrypted => "err:encrypted",
            Self::Corrupted => "err:corrupted",
            Self::NoText => "err:no-text",
        }
    }
}

/// [`extract_text_detailed`] 的结果：抽到的文本，或者一个明确的失败原因。
#[derive(Debug, Clone, PartialEq)]
pub enum ExtractOutcome {
    /// 抽到的正文（已 trim/拼接成可索引的整篇文本）。
    Text(String),
    /// 抽不出正文，附带具体原因（不支持/超限/加密/损坏/无可提取文字）。
    Failure(ExtractFailure),
}

/// 这个文件是否属于"会尝试抽取文本"的类型（只看扩展名，不读文件，很便宜）。
/// 启动对账用它过滤：非文本类型的文件从来不会进索引，没必要参与三态比对——
/// 否则每次对账都把它们当"新增"白跑一趟、还把对账统计数字撑虚。
///
/// 读进程级当前生效规则：追加扩展名（`extra_text_exts`）里的类型也算可抽取。
/// 旧版 Office 格式（doc/xls/ppt）也算——它们会以占位文档（err:unsupported）
/// 的形式入索引，对账要能把它们和磁盘状态对上。
pub(crate) fn is_extractable(path: &Path) -> bool {
    is_extractable_with(path, &crate::rules::active_rules())
}

/// [`is_extractable`] 的纯函数版：判定逻辑接收显式规则，不碰进程级全局，便于
/// 单测。零参的 `is_extractable` 只是用当前生效规则调它。
pub(crate) fn is_extractable_with(path: &Path, rules: &IndexRules) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let ext = ext.to_ascii_lowercase();
    ext == "pdf"
        || TEXT_EXTS.contains(&ext.as_str())
        || OFFICE_EXTS.contains(&ext.as_str())
        || LEGACY_OFFICE_EXTS.contains(&ext.as_str())
        || rules.is_extra_text_ext(&ext)
}

/// 从文件里抽出可索引的纯文本。返回 Option 的老入口：抽不出来（不支持的
/// 格式/太大/损坏）不算错误，调用方只需要知道"这个文件没有文本"，跳过即可。
/// 需要区分失败原因的调用方（索引器写占位状态文档）用
/// [`extract_text_detailed`]。
///
/// 线上路径只剩索引器一处，且它已改用 [`extract_text_detailed`]（要拿到失败
/// 原因写占位状态文档）。这里保留只给本模块单测当"成功/失败"二分断言用，
/// 非测试构建下不再编译，避免变成不可达的死代码。
#[cfg(test)]
pub fn extract_text(path: &Path) -> Option<String> {
    match extract_text_detailed(path) {
        ExtractOutcome::Text(text) => Some(text),
        ExtractOutcome::Failure(_) => None,
    }
}

/// [`extract_text`] 的带失败原因版本：正文抽不出来时返回具体是
/// 不支持/超限/加密/损坏/无可提取文字中的哪一种，索引器据此写占位状态文档
/// （kind=err:xxx），用户按文件名仍能搜到这个文件并看到明确状态。
///
/// 体积上限和追加文本扩展名取自进程级当前生效规则（见 `rules` 模块）。
pub fn extract_text_detailed(path: &Path) -> ExtractOutcome {
    extract_text_detailed_with(path, &crate::rules::active_rules())
}

fn extract_text_detailed_with(path: &Path, rules: &IndexRules) -> ExtractOutcome {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return ExtractOutcome::Failure(ExtractFailure::UnsupportedFormat);
    };
    let ext = ext.to_ascii_lowercase();

    // 旧版二进制格式先于体积检查：不管多大都不支持，避免把 300MB 的 .ppt
    // 报成"超限"误导排查方向。
    if LEGACY_OFFICE_EXTS.contains(&ext.as_str()) {
        return ExtractOutcome::Failure(ExtractFailure::UnsupportedFormat);
    }

    let Ok(meta) = fs::metadata(path) else {
        return ExtractOutcome::Failure(ExtractFailure::Corrupted);
    };
    if meta.len() > rules.max_file_bytes() {
        return ExtractOutcome::Failure(ExtractFailure::Oversize);
    }

    // 各格式的抽取都可能在底层库里 panic：PDF 走的 `pdf_extract`（及其底层
    // `lopdf`）对某些畸形/深层嵌套的 PDF 不是返回 `Err`，而是内部 `unwrap`/`panic!`；
    // docx/xlsx/pptx 共用的 zip + XML 解析同样可能对畸形输入 panic。而抽取是在
    // 监听/OCR 线程持有共享 `IndexUpdater` 锁时调的：panic 一旦穿过持有的
    // `MutexGuard`，就会把这把共享锁**毒化**，之后所有 `.lock()` 都 panic，连锁
    // 拖垮整条监听 + OCR 管线（对常驻托盘程序是灾难性的）。
    //
    // 这里用 `catch_unwind` 把**任意**格式抽取过程中的 panic 兜成"损坏"，让畸形
    // 文件跟其它抽不出文本的文件一样带着明确状态进占位文档。（`updater.rs`/
    // `watch.rs`/`ocr_worker.rs` 那侧的锁也已改成抗毒化的
    // `unwrap_or_else(|e| e.into_inner())`，两层防御各自都能兜住。）
    //
    // 注意 `catch_unwind` 只能接住**栈展开式**的 panic（`unwrap`/`panic!` 这类）。
    // 若畸形输入触发深度递归把线程栈撑爆，那是栈溢出，走的是 SIGABRT/进程 abort，
    // `catch_unwind` 接不住，进程仍会整体退出——这一类只能靠上游解析库自身的
    // 递归深度限制来防，不在本函数的兜底范围内。
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match ext.as_str() {
        "pdf" => extract_pdf(path),
        "docx" => extract_docx(path),
        "xlsx" => extract_xlsx(path),
        "pptx" => extract_pptx(path),
        e if TEXT_EXTS.contains(&e) => text_outcome(read_text_smart(path)),
        // 追加白名单里的扩展名当纯文本读（自动探测编码），跟内建文本类型同路。
        e if rules.is_extra_text_ext(e) => text_outcome(read_text_smart(path)),
        _ => ExtractOutcome::Failure(ExtractFailure::UnsupportedFormat),
    }))
    .unwrap_or(ExtractOutcome::Failure(ExtractFailure::Corrupted));

    // 各格式统一兜一层：抽出来的正文全是空白视同"无可提取文字"，跟空文件
    // 一个状态，不索引一篇纯空白的文档冒充成功。
    match outcome {
        ExtractOutcome::Text(ref text) if text.trim().is_empty() => {
            ExtractOutcome::Failure(ExtractFailure::NoText)
        }
        other => other,
    }
}

/// [`read_text_smart`] 的 Option 结果折算成 [`ExtractOutcome`]：
/// None（读失败或空文件）统一按"无可提取文字"处理。
fn text_outcome(text: Option<String>) -> ExtractOutcome {
    match text {
        Some(text) => ExtractOutcome::Text(text),
        None => ExtractOutcome::Failure(ExtractFailure::NoText),
    }
}

/// 取一个 XML 属性的文本值（解实体转义 + 按 XML 1.0 规则归一化空白）。读失败
/// 返回 None，调用方按"该属性缺失"处理——畸形属性不该让整篇文档的抽取失败。
///
/// 用 `normalized_value(XmlVersion::Explicit1_0)` 而不是带 decoder 的
/// `decoded_and_normalized_value`：OOXML（docx/xlsx/pptx）规定内部 XML 一律
/// UTF-8 编码，没有换编码的需要；被读的属性值又都是 `Id`/`Target`/`name`/`r`/`t`
/// 这类只含 ASCII 的标识符，归一化（把 `\t\r\n` 折成空格）对它们没有副作用。
fn attr_text(attr: &Attribute<'_>) -> Option<String> {
    attr.normalized_value(XmlVersion::Explicit1_0)
        .ok()
        .map(|v| v.into_owned())
}

/// 从 PDF 抽文本。`pdf_extract` 返回 `Err`（结构损坏/加密）时按"损坏"处理；
/// 能读出但正文为空（典型：扫描版 PDF，页面只有图片没有文字层）由外层统一
/// 折算成"无可提取文字"。畸形 PDF 触发的 panic 由外层 `catch_unwind` 兜底。
fn extract_pdf(path: &Path) -> ExtractOutcome {
    match pdf_extract::extract_text(path) {
        Ok(text) => ExtractOutcome::Text(text),
        Err(_) => ExtractOutcome::Failure(ExtractFailure::Corrupted),
    }
}

/// zip 条目读取失败的分类。注意"整个 zip 打不开"（不是 zip 包/文件头损坏）
/// 不在这里：那种情况在 [`open_archive`] 那层就已经折成
/// [`ExtractFailure::Corrupted`] 了，轮不到按条目名读。
enum ZipEntryError {
    /// 条目不存在（zip 结构是好的，但没有我们找的那个条目）。
    Missing,
    /// 条目读取失败，带底层错误信息（含 "Password" 时判为加密）。
    Read(String),
}

/// 按 zip 条目读取失败的信息判定是加密还是损坏。
fn classify_entry_error(message: &str) -> ExtractFailure {
    if message.to_ascii_lowercase().contains("password") {
        ExtractFailure::Encrypted
    } else {
        ExtractFailure::Corrupted
    }
}

/// 打开一个 zip 包里的单个条目，读成字节。跟旧版 [`read_zip_entry`] 的区别：
/// 失败带上分类信息，供调用方把"加密"和"损坏"区分开——密码保护的 Office
/// 文件条目读取会报 Password 相关错误，损坏文件报 CRC/结构错误。
fn read_zip_entry_checked(
    archive: &mut zip::ZipArchive<fs::File>,
    entry_name: &str,
) -> Result<Vec<u8>, ZipEntryError> {
    let mut entry = archive
        .by_name(entry_name)
        .map_err(|_| ZipEntryError::Missing)?;
    let mut bytes = Vec::new();
    entry
        .read_to_end(&mut bytes)
        .map_err(|e| ZipEntryError::Read(e.to_string()))?;
    Ok(bytes)
}

fn open_archive(path: &Path) -> Result<zip::ZipArchive<fs::File>, ExtractFailure> {
    let file = fs::File::open(path).map_err(|_| ExtractFailure::Corrupted)?;
    zip::ZipArchive::new(file).map_err(|_| ExtractFailure::Corrupted)
}

/// docx 是一个 zip 包，正文全部在 word/document.xml 里，文本节点是 `<w:t>`。
/// 段落 `<w:p>` 闭合时补一个换行，避免不同段落的文字连成一整块无法阅读。
fn extract_docx(path: &Path) -> ExtractOutcome {
    let mut archive = match open_archive(path) {
        Ok(archive) => archive,
        Err(failure) => return ExtractOutcome::Failure(failure),
    };
    let xml = match read_zip_entry_checked(&mut archive, "word/document.xml") {
        Ok(xml) => xml,
        // 条目读取失败：错误信息里带 Password 的是加密文件，其余是损坏。
        Err(ZipEntryError::Read(message)) => {
            return ExtractOutcome::Failure(classify_entry_error(&message))
        }
        // 打得开 zip 但缺正文条目：按损坏处理。
        Err(_) => return ExtractOutcome::Failure(ExtractFailure::Corrupted),
    };
    let text = extract_tagged_text(&xml, b"t", Some(b"p"));
    ExtractOutcome::Text(text.trim().to_string())
}

// ---------------------------------------------------------------------------
// XLSX：按 工作表 → 行 → 单元格 的真实结构读取
// ---------------------------------------------------------------------------

/// 解析 xlsx。跟旧实现（共享字符串整表倒出 + 按文件名顺序扫 sheet）不同：
///
/// 1. **工作表顺序**来自 `xl/workbook.xml` 里的 `<sheet>` 声明顺序，经
///    `xl/_rels/workbook.xml.rels` 把 r:id 映射到具体的 sheet XML——不是
///    zip 里 `sheet1/sheet10/sheet2` 的字典序。
/// 2. **单元格逐个解析**：共享字符串（t="s"）、内联字符串（t="inlineStr"）、
///    数字、布尔（t="b"）、公式缓存结果（t="str" 或无 t 但带 `<f>`）都读，
///    并保留 `工作表名 + 单元格坐标`（如 `B3`），命中能追溯到具体位置。
/// 3. **共享字符串只输出被引用的**：表是去重存储，未被任何单元格引用的
///    条目不是正文。
/// 4. 公式不自行计算：只有 Excel 存下的缓存值（`<f>` 旁边的 `<v>`）会
///    输出；没有缓存值时标注"公式无缓存值"，不冒充计算结果。
///
/// workbook.xml 或关系映射缺失/损坏时退回"文件名里的序号数值排序"（仍修掉
/// sheet1/sheet10/sheet2 的字典序问题），工作表名用文件名兜底。
fn extract_xlsx(path: &Path) -> ExtractOutcome {
    let mut archive = match open_archive(path) {
        Ok(archive) => archive,
        Err(_) => {
            // Office 加密（整包加密）在 ZipArchive::new 这一步就失败，跟
            // 真损坏在这层区分不开；read 失败带信息的那层才能识别 Password。
            return ExtractOutcome::Failure(ExtractFailure::Corrupted);
        }
    };

    let shared = read_shared_strings(&mut archive);
    let sheets = resolve_xlsx_sheet_order(&mut archive);

    let mut text = String::new();
    for (index, (sheet_name, entry_path)) in sheets.iter().enumerate() {
        // 单个 sheet 坏了不拖垮整个工作簿：跳过它，其余 sheet 照常输出。
        let Ok(bytes) = read_zip_entry_checked(&mut archive, entry_path) else {
            continue;
        };
        if index > 0 {
            text.push('\n');
        }
        text.push_str(&format!("[工作表 {sheet_name}]\n"));
        text.push_str(&parse_sheet_cells(&bytes, &shared));
    }

    ExtractOutcome::Text(text)
}

/// 读共享字符串表 `xl/sharedStrings.xml`，保持索引顺序（sheet 里的 t="s"
/// 单元格用 `<v>` 里的数字引用这个表的第 N 项）。每个 `<si>` 可能是纯文本
/// （`<si><t>…</t></si>`）也可能是富文本串（多个 `<r><t>…</t></r>` run），
/// 都拼成一个完整字符串。
fn read_shared_strings(archive: &mut zip::ZipArchive<fs::File>) -> Vec<String> {
    let Ok(bytes) = read_zip_entry_checked(archive, "xl/sharedStrings.xml") else {
        return Vec::new();
    };
    parse_shared_strings(&bytes)
}

fn parse_shared_strings(xml: &[u8]) -> Vec<String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut items = Vec::new();
    let mut current = String::new();
    let mut in_si = false;
    let mut in_t = false;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"si" => {
                    in_si = true;
                    current.clear();
                }
                b"t" if in_si => in_t = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_si
                    && in_t
                    && let Ok(decoded) = t.decode()
                    && let Ok(unescaped) = quick_xml::escape::unescape(&decoded)
                {
                    current.push_str(&unescaped);
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"t" => in_t = false,
                b"si" => {
                    in_si = false;
                    items.push(std::mem::take(&mut current));
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break,
        }
    }
    items
}

/// 解析 `xl/_rels/workbook.xml.rels`：rId → Target（相对 xl/ 的路径）。
fn parse_relationships(xml: &[u8]) -> HashMap<String, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut map = HashMap::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() != b"Relationship" {
                    continue;
                }
                let mut id = None;
                let mut target = None;
                for attr in e.attributes() {
                    let Ok(attr) = attr else { continue };
                    match attr.key.as_ref() {
                        b"Id" => id = attr_text(&attr),
                        b"Target" => target = attr_text(&attr),
                        _ => {}
                    }
                }
                if let (Some(id), Some(target)) = (id, target) {
                    map.insert(id, target);
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    map
}

/// 把 rels 里的 Target（相对 xl/ 或绝对路径）规整成 zip 里的完整条目名。
/// Target 形如 `worksheets/sheet1.xml`（相对 xl/）或 `/xl/worksheets/sheet1.xml`
/// （绝对，少见）；带 `..` 的（外部引用）直接判无效。
fn resolve_office_target(base_dir: &str, target: &str) -> Option<String> {
    if target.contains("..") {
        return None;
    }
    let target = target.trim_start_matches('/');
    if target.is_empty() {
        return None;
    }
    // 已经带 base 前缀的（如 "xl/worksheets/sheet1.xml"）不再重复拼。
    if target.starts_with(base_dir) {
        return Some(target.to_string());
    }
    Some(format!("{base_dir}/{target}"))
}

/// 决定 xlsx 的工作表读取顺序：workbook.xml 的 `<sheet name=… r:id=…>`
/// 声明顺序 + rels 映射。全部解析失败时退回"按文件名内嵌序号的数值排序"。
fn resolve_xlsx_sheet_order(archive: &mut zip::ZipArchive<fs::File>) -> Vec<(String, String)> {
    let workbook = read_zip_entry_checked(archive, "xl/workbook.xml").ok();
    let rels = read_zip_entry_checked(archive, "xl/_rels/workbook.xml.rels").ok();

    if let (Some(workbook), Some(rels)) = (workbook, rels) {
        let id_to_target = parse_relationships(&rels);
        let declared = parse_workbook_sheets(&workbook);
        if !declared.is_empty() {
            let mut sheets = Vec::with_capacity(declared.len());
            for (name, rid) in declared {
                // 映射不到的 sheet 跳过（rels 缺项），不影响其余顺序。
                if let Some(target) = id_to_target.get(&rid)
                    && let Some(entry) = resolve_office_target("xl", target)
                {
                    sheets.push((name, entry));
                }
            }
            if !sheets.is_empty() {
                return sheets;
            }
        }
    }

    // 兜底：没有 workbook.xml/rels 可用时，按文件名里 "sheetN.xml" 的 N 数值
    // 排序（修掉 sheet1/sheet10/sheet2 的字典序问题），表名用文件名。
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("xl/worksheets/sheet") && n.ends_with(".xml"))
        .map(|n| n.to_string())
        .collect();
    names.sort_by_key(|n| embedded_number(n).unwrap_or(u64::MAX));
    names
        .into_iter()
        .map(|n| {
            let stem = n
                .rsplit('/')
                .next()
                .unwrap_or(&n)
                .trim_end_matches(".xml")
                .to_string();
            (stem, n)
        })
        .collect()
}

/// 解析 workbook.xml：按出现顺序收集 `<sheet name="…" r:id="…">`。
/// r:id 属性带命名空间前缀（`r:id`），本地名是 `id`。
fn parse_workbook_sheets(xml: &[u8]) -> Vec<(String, String)> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut sheets = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() != b"sheet" {
                    continue;
                }
                let mut name = None;
                let mut rid = None;
                for attr in e.attributes() {
                    let Ok(attr) = attr else { continue };
                    let key = attr.key.as_ref();
                    match () {
                        _ if key == b"name" => name = attr_text(&attr),
                        // r:id / relationships 命名空间下的 id
                        _ if key == b"r:id" || key.ends_with(b":id") => {
                            rid = attr_text(&attr)
                        }
                        _ => {}
                    }
                }
                if let (Some(name), Some(rid)) = (name, rid) {
                    sheets.push((name, rid));
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    sheets
}

/// "slides/slide10.xml" / "xl/worksheets/sheet2.xml" 这类名字里内嵌序号的
/// 数值提取，供兜底排序用。
fn embedded_number(name: &str) -> Option<u64> {
    let tail = name.rsplit(['/', '\\']).next()?;
    let stem = tail.strip_suffix(".xml").unwrap_or(tail);
    let digits: String = stem.chars().rev().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

/// 一个工作表里逐单元格解析 `<sheetData><row><c …>`，输出
/// `A1: 值` 形式的行（每个单元格一行，坐标取自单元格的 r 属性；缺 r 属性
/// 时只输出值）。行结束时补换行。
fn parse_sheet_cells(xml: &[u8], shared: &[String]) -> String {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    // 当前单元格的累积状态
    let mut cell_ref = String::new();
    let mut cell_type = String::new();
    let mut in_cell = false;
    let mut in_value = false; // <v> 内
    let mut in_inline = false; // <is> 内
    let mut in_inline_text = false; // <is><t> 内
    let mut has_formula = false;
    let mut value_text = String::new();
    let mut inline_text = String::new();

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match e.local_name().as_ref() {
                b"c" if !in_cell => {
                    in_cell = true;
                    cell_ref.clear();
                    cell_type.clear();
                    has_formula = false;
                    value_text.clear();
                    inline_text.clear();
                    for attr in e.attributes() {
                        let Ok(attr) = attr else { continue };
                        match attr.key.as_ref() {
                            b"r" => cell_ref = attr_text(&attr).unwrap_or_default(),
                            b"t" => cell_type = attr_text(&attr).unwrap_or_default(),
                            _ => {}
                        }
                    }
                }
                b"f" if in_cell => has_formula = true,
                b"v" if in_cell => in_value = true,
                b"is" if in_cell => in_inline = true,
                b"t" if in_inline => in_inline_text = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if let Ok(decoded) = t.decode()
                    && let Ok(unescaped) = quick_xml::escape::unescape(&decoded)
                {
                    if in_value {
                        value_text.push_str(&unescaped);
                    } else if in_inline_text {
                        inline_text.push_str(&unescaped);
                    }
                }
            }
            Ok(Event::End(e)) => match e.local_name().as_ref() {
                b"v" => in_value = false,
                b"t" if in_inline => in_inline_text = false,
                b"is" => in_inline = false,
                b"c" if in_cell => {
                    in_cell = false;
                    if let Some(rendered) =
                        render_cell_value(&cell_type, &value_text, &inline_text, has_formula, shared)
                    {
                        if cell_ref.is_empty() {
                            out.push_str(&rendered);
                        } else {
                            out.push_str(&format!("{cell_ref}: {rendered}"));
                        }
                        out.push('\n');
                    }
                }
                b"row" => {
                    // row 结束不再额外补行：每个单元格已各自成行。
                }
                _ => {}
            },
            Ok(Event::Empty(e)) => {
                // 空单元格 <c/> 或空行 <row/>：没有内容可输出。
                if e.local_name().as_ref() == b"c" && !in_cell {
                    // 自闭合的空单元格，跳过。
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    out
}

/// 把一个单元格的类型 + 原始文本折算成展示值；无可输出内容时返回 None
/// （空单元格整行不输出，避免满屏 "A1: "）。
fn render_cell_value(
    cell_type: &str,
    value_text: &str,
    inline_text: &str,
    has_formula: bool,
    shared: &[String],
) -> Option<String> {
    match cell_type {
        // 共享字符串：v 里是索引号
        "s" => {
            let index: usize = value_text.trim().parse().ok()?;
            shared
                .get(index)
                .cloned()
                .or_else(|| Some(format!("(共享字符串 #{index} 缺失)")))
        }
        // 布尔
        "b" => Some(if value_text.trim() == "1" { "TRUE" } else { "FALSE" }.to_string()),
        // 公式错误值（#DIV/0! 等）：原样输出，便于检索定位
        "e" => (!value_text.is_empty()).then(|| value_text.to_string()),
        // 公式的字符串结果
        "str" => (!value_text.is_empty()).then(|| value_text.to_string()),
        // 内联字符串
        "inlineStr" => (!inline_text.is_empty()).then(|| inline_text.to_string()),
        // 数字 / 公式缓存值（无 t 或 t="n"）
        _ => {
            if !value_text.is_empty() {
                Some(value_text.to_string())
            } else if has_formula {
                // 有公式但没有缓存结果：明确标注限制，不自行计算。
                Some("(公式无缓存值)".to_string())
            } else {
                None
            }
        }
    }
}

// ---------------------------------------------------------------------------
// PPTX：按 presentation.xml 声明的页序读取
// ---------------------------------------------------------------------------

/// 解析 pptx。幻灯片顺序来自 `ppt/presentation.xml` 的 `<p:sldIdLst>`
/// （`<p:sldId r:id=…>` 的出现顺序）经 `ppt/_rels/presentation.xml.rels`
/// 映射到具体 slide XML——不用 `slideN.xml` 的文件名字典序（那会把
/// slide10 排在 slide2 前面）。每页输出 `[幻灯片 N]` 页序标记，页内保留
/// 段落分隔。
fn extract_pptx(path: &Path) -> ExtractOutcome {
    let mut archive = match open_archive(path) {
        Ok(archive) => archive,
        Err(_) => return ExtractOutcome::Failure(ExtractFailure::Corrupted),
    };

    let order = resolve_pptx_slide_order(&mut archive);

    let mut text = String::new();
    for (index, entry_path) in order.iter().enumerate() {
        let Ok(bytes) = read_zip_entry_checked(&mut archive, entry_path) else {
            continue;
        };
        text.push_str(&format!("[幻灯片 {}]\n", index + 1));
        text.push_str(&extract_tagged_text(&bytes, b"t", Some(b"p")));
        text.push('\n');
    }

    ExtractOutcome::Text(text)
}

/// 决定 pptx 的幻灯片读取顺序：presentation.xml 的 sldIdLst + rels 映射。
/// 解析不出有效顺序时退回"按文件名内嵌序号的数值排序"。
fn resolve_pptx_slide_order(archive: &mut zip::ZipArchive<fs::File>) -> Vec<String> {
    let presentation = read_zip_entry_checked(archive, "ppt/presentation.xml").ok();
    let rels = read_zip_entry_checked(archive, "ppt/_rels/presentation.xml.rels").ok();

    if let (Some(presentation), Some(rels)) = (presentation, rels) {
        let id_to_target = parse_relationships(&rels);
        let rids = parse_presentation_slide_ids(&presentation);
        if !rids.is_empty() {
            let mut slides = Vec::with_capacity(rids.len());
            for rid in rids {
                if let Some(target) = id_to_target.get(&rid)
                    && let Some(entry) = resolve_office_target("ppt", target)
                {
                    slides.push(entry);
                }
            }
            if !slides.is_empty() {
                return slides;
            }
        }
    }

    // 兜底：按 slideN.xml 里 N 的数值排序。
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|n| n.starts_with("ppt/slides/slide") && n.ends_with(".xml"))
        .map(|n| n.to_string())
        .collect();
    names.sort_by_key(|n| embedded_number(n).unwrap_or(u64::MAX));
    names
}

/// 解析 presentation.xml 里 `<p:sldIdLst>` 的 r:id 出现顺序。
fn parse_presentation_slide_ids(xml: &[u8]) -> Vec<String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);

    let mut rids = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                if e.local_name().as_ref() != b"sldId" {
                    continue;
                }
                for attr in e.attributes() {
                    let Ok(attr) = attr else { continue };
                    let key = attr.key.as_ref();
                    // 用嵌套 if，不与 `let` 链混在同一个条件里：let 链只能和
                    // `&&` 组合，跟 `||` 并列必须加括号，拆开写更不容易出错。
                    if key == b"r:id" || key.ends_with(b":id") {
                        if let Some(value) = attr_text(&attr) {
                            rids.push(value);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    rids
}

/// 流式扫一遍 XML 事件，抠出本地标签名匹配 text_tag 的节点里的文本
/// （按本地名匹配、忽略命名空间前缀，`w:t`/`a:t`/无前缀的 `t` 都能命中）。
/// 不建 DOM 树：Office XML 展开后可能几十万字符，没必要整棵放内存。
/// para_tag 给定时，每闭合一个该标签就补换行，用来标记段落/幻灯片内的换行边界。
/// XML 本身损坏时中途跳出循环，返回已经攒到的文本——调用方那层再统一按
/// "抽不出内容就是无文本" 处理，不在这里 panic 或者往外抛错误。
fn extract_tagged_text(xml: &[u8], text_tag: &[u8], para_tag: Option<&[u8]>) -> String {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    let mut in_text = false;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => {
                if e.local_name().as_ref() == text_tag {
                    in_text = true;
                }
            }
            Ok(Event::Text(t)) => {
                if in_text
                    && let Ok(decoded) = t.decode()
                    && let Ok(unescaped) = quick_xml::escape::unescape(&decoded)
                {
                    out.push_str(&unescaped);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.local_name();
                if name.as_ref() == text_tag {
                    in_text = false;
                } else if para_tag.is_some_and(|p| name.as_ref() == p) {
                    out.push('\n');
                }
            }
            Ok(Event::Empty(e)) => {
                if para_tag.is_some_and(|p| e.local_name().as_ref() == p) {
                    out.push('\n');
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }

    out
}

/// 读文本文件，自动探测编码。
/// Windows 上的中文 txt 很多是 GBK 而不是 UTF-8，直接按 UTF-8 读会变乱码，
/// 所以先用 chardetng 猜编码，再用 encoding_rs 解码成 UTF-8。
fn read_text_smart(path: &Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    if bytes.is_empty() {
        return None;
    }

    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(&bytes, true);
    let encoding = detector.guess(None, chardetng::Utf8Detection::Allow);

    let (text, _, had_errors) = encoding.decode(&bytes);
    if had_errors && encoding != encoding_rs::UTF_8 {
        // 猜错编码时宁可退回 UTF-8 有损解码，也不索引一堆乱码词条
        return Some(String::from_utf8_lossy(&bytes).into_owned());
    }
    Some(text.into_owned())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use zip::write::SimpleFileOptions;

    use super::*;

    /// 程序化拼一个最小合法 zip 包，条目名到内容的映射即够——
    /// Office 文件本质就是 zip，抽取只关心我们读的那几个条目存在且是合法 XML。
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

    #[test]
    fn extracts_docx_paragraph_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.docx");
        let document_xml = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
  <w:body>
    <w:p><w:r><w:t>季度对账哨兵词</w:t></w:r></w:p>
    <w:p><w:r><w:t>quarterly-sentinel</w:t></w:r></w:p>
  </w:body>
</w:document>"#;
        write_zip(&path, &[("word/document.xml", document_xml)]);

        let text = extract_text(&path).expect("docx 应能抽出文本");
        assert!(text.contains("季度对账哨兵词"));
        assert!(text.contains("quarterly-sentinel"));
    }

    /// 构造一个带 workbook.xml + rels 的最小 xlsx，方便多个 xlsx 用例复用。
    fn write_xlsx(path: &Path, entries: &[(&str, &str)]) {
        write_zip(path, entries);
    }

    /// 共享字符串表：两项，第一项纯文本、第二项富文本（两个 run 拼成一个串）。
    const SHARED_STRINGS: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="3" uniqueCount="2">
  <si><t>季度对账哨兵词</t></si>
  <si><r><t>富文本</t></r><r><t>拼接</t></r></si>
</sst>"#;

    #[test]
    fn xlsx_resolves_shared_strings_numbers_bools_and_cell_refs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cells.xlsx");
        let workbook = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets>
    <sheet name="对账表" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
        let sheet = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" t="s"><v>0</v></c>
      <c r="B1" t="n"><v>1234.5</v></c>
      <c r="C1" t="b"><v>1</v></c>
      <c r="D1" t="inlineStr"><is><t>inline文本</t></is></c>
      <c r="E1"><f>SUM(B1,1)</f><v>1235.5</v></c>
      <c r="F1"><f>NOCACHE()</f></c>
      <c r="G1" t="e"><v>#DIV/0!</v></c>
    </row>
  </sheetData>
</worksheet>"#;
        write_xlsx(
            &path,
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/sharedStrings.xml", SHARED_STRINGS),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );

        let text = extract_text(&path).expect("xlsx 应能抽出文本");
        assert!(text.contains("[工作表 对账表]"), "应带工作表名: {text}");
        assert!(text.contains("A1: 季度对账哨兵词"), "共享字符串应按坐标输出: {text}");
        assert!(text.contains("B1: 1234.5"), "数字单元格应输出: {text}");
        assert!(text.contains("C1: TRUE"), "布尔单元格应输出 TRUE: {text}");
        assert!(text.contains("D1: inline文本"), "内联字符串应输出: {text}");
        assert!(text.contains("E1: 1235.5"), "公式缓存值应输出: {text}");
        assert!(text.contains("F1: (公式无缓存值)"), "无缓存的公式应明确标注: {text}");
        assert!(text.contains("G1: #DIV/0!"), "错误值应原样输出: {text}");
    }

    #[test]
    fn xlsx_unreferenced_shared_string_is_not_body_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unreferenced.xlsx");
        let workbook = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="x" Target="worksheets/sheet1.xml"/></Relationships>"#;
        // 共享字符串表里第二项从未被任何单元格引用，不应出现在正文里。
        let shared = r#"<sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><si><t>被引用的词</t></si><si><t>从未被引用的孤儿字符串</t></si></sst>"#;
        let sheet = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row></sheetData></worksheet>"#;
        write_xlsx(
            &path,
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/sharedStrings.xml", shared),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );

        let text = extract_text(&path).expect("xlsx 应能抽出文本");
        assert!(text.contains("被引用的词"));
        assert!(
            !text.contains("从未被引用的孤儿字符串"),
            "未被引用的共享字符串不是正文: {text}"
        );
    }

    #[test]
    fn xlsx_sheet_order_follows_workbook_declaration_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("order.xlsx");
        // rId1 → sheet2.xml（声明顺序第一），rId2 → sheet1.xml（第二）。
        // 旧实现按文件名字典序会把 sheet1 排在前面。
        let workbook = r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="甲" sheetId="1" r:id="rId1"/><sheet name="乙" sheetId="2" r:id="rId2"/></sheets></workbook>"#;
        let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="x" Target="worksheets/sheet2.xml"/><Relationship Id="rId2" Type="x" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let sheet1 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>乙表内容</t></is></c></row></sheetData></worksheet>"#;
        let sheet2 = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>甲表内容</t></is></c></row></sheetData></worksheet>"#;
        write_xlsx(
            &path,
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/worksheets/sheet1.xml", sheet1),
                ("xl/worksheets/sheet2.xml", sheet2),
            ],
        );

        let text = extract_text(&path).expect("xlsx 应能抽出文本");
        let first = text.find("甲表内容").expect("甲表内容应在正文里");
        let second = text.find("乙表内容").expect("乙表内容应在正文里");
        assert!(
            first < second,
            "工作表顺序应按 workbook.xml 声明（甲在乙前），实际: {text}"
        );
        assert!(text.contains("[工作表 甲]"));
        assert!(text.contains("[工作表 乙]"));
    }

    #[test]
    fn xlsx_without_workbook_falls_back_to_numeric_sheet_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fallback.xlsx");
        // 没有 workbook.xml：兜底按文件名里的序号数值排序——sheet10 要排在
        // sheet2 之后，不能按字典序排到前面。
        let sheet2 = r#"<worksheet><sheetData><row><c t="inlineStr"><is><t>第二页-marker</t></is></c></row></sheetData></worksheet>"#;
        let sheet10 = r#"<worksheet><sheetData><row><c t="inlineStr"><is><t>第十页-marker</t></is></c></row></sheetData></worksheet>"#;
        write_xlsx(
            &path,
            &[
                ("xl/worksheets/sheet2.xml", sheet2),
                ("xl/worksheets/sheet10.xml", sheet10),
            ],
        );

        let text = extract_text(&path).expect("xlsx 应能抽出文本");
        let second = text.find("第二页-marker").unwrap();
        let tenth = text.find("第十页-marker").unwrap();
        assert!(second < tenth, "数值序：sheet2 应在 sheet10 前: {text}");
        assert!(text.contains("[工作表 sheet2]"));
        assert!(text.contains("[工作表 sheet10]"));
    }

    #[test]
    fn pptx_slide_order_follows_presentation_not_filename() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("order.pptx");
        // sldIdLst 声明 rId1 → slide10.xml 在最前，rId2 → slide2.xml 第二。
        // 旧实现的文件名字典序会输出 slide1, slide10, slide2。
        let presentation = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<p:presentation xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"
                xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
                xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <p:sldIdLst>
    <p:sldId id="256" r:id="rId1"/>
    <p:sldId id="257" r:id="rId2"/>
    <p:sldId id="258" r:id="rId3"/>
  </p:sldIdLst>
</p:presentation>"#;
        let rels = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="x" Target="slides/slide10.xml"/>
  <Relationship Id="rId2" Type="x" Target="slides/slide2.xml"/>
  <Relationship Id="rId3" Type="x" Target="slides/slide1.xml"/>
</Relationships>"#;
        fn slide(text: &str) -> String {
            format!(
                r#"<p:sld xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#
            )
        }
        write_zip(
            &path,
            &[
                ("ppt/presentation.xml", presentation),
                ("ppt/_rels/presentation.xml.rels", rels),
                ("ppt/slides/slide1.xml", &slide("第三顺位-marker")),
                ("ppt/slides/slide2.xml", &slide("第二顺位-marker")),
                ("ppt/slides/slide10.xml", &slide("第一顺位-marker")),
            ],
        );

        let text = extract_text(&path).expect("pptx 应能抽出文本");
        let first = text.find("第一顺位-marker").unwrap();
        let second = text.find("第二顺位-marker").unwrap();
        let third = text.find("第三顺位-marker").unwrap();
        assert!(first < second && second < third, "页序应按 presentation.xml: {text}");
        assert!(text.contains("[幻灯片 1]\n第一顺位-marker"));
        assert!(text.contains("[幻灯片 3]\n第三顺位-marker"));
    }

    #[test]
    fn corrupted_docx_is_skipped_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broken.docx");
        fs::write(&path, b"this is not a zip file at all").unwrap();

        assert_eq!(
            extract_text_detailed(&path),
            ExtractOutcome::Failure(ExtractFailure::Corrupted)
        );
    }

    #[test]
    fn legacy_office_formats_report_unsupported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.doc");
        fs::write(&path, b"OLE2 compound document bytes").unwrap();

        assert!(
            is_extractable(&path),
            "旧版格式应算入可抽取白名单（占位入索引），否则对账会当孤儿删掉"
        );
        assert_eq!(
            extract_text_detailed(&path),
            ExtractOutcome::Failure(ExtractFailure::UnsupportedFormat)
        );
        assert_eq!(
            ExtractFailure::UnsupportedFormat.kind_code(),
            "err:unsupported"
        );
    }

    #[test]
    fn encrypted_entry_error_is_classified_as_encrypted() {
        assert_eq!(
            classify_entry_error("Password required to decrypt file"),
            ExtractFailure::Encrypted
        );
        assert_eq!(
            classify_entry_error("Invalid checksum: CRC32 mismatch"),
            ExtractFailure::Corrupted
        );
    }

    // --- 编码探测（read_text_smart）---

    #[test]
    fn gbk_encoded_chinese_txt_is_decoded_to_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gbk_sample.txt");
        // 一段足够长的中文，探测器才有把握判成 GBK 而不是别的单字节编码。
        let content = "这是一段用于编码探测的中文测试文本，\
包含足够多的汉字以便字符集识别器把它判定为国标编码而不是别的编码，\
季度对账哨兵词也在这里出现。";
        let (gbk_bytes, _, unmappable) = encoding_rs::GBK.encode(content);
        assert!(!unmappable, "测试文本应能完整编码成 GBK");
        fs::write(&path, &gbk_bytes).unwrap();

        let text = read_text_smart(&path).expect("GBK 文件应能解码出文本");
        assert!(text.contains("季度对账哨兵词"), "应还原出原始中文: {text}");
        assert!(text.contains("国标编码"));
    }

    #[test]
    fn utf8_bom_file_is_decoded_without_leading_bom_char() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bom_sample.txt");
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("带 BOM 的内容 marker-bom".as_bytes());
        fs::write(&path, &bytes).unwrap();

        let text = read_text_smart(&path).expect("带 BOM 的 UTF-8 文件应能解码");
        assert!(text.contains("marker-bom"));
        assert!(text.contains("带 BOM 的内容"));
        assert!(!text.starts_with('\u{FEFF}'), "BOM 应被剥掉，不进正文");
    }

    #[test]
    fn empty_text_file_reports_no_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty_sample.txt");
        fs::write(&path, b"").unwrap();
        assert_eq!(
            extract_text_detailed(&path),
            ExtractOutcome::Failure(ExtractFailure::NoText)
        );
    }

    #[test]
    fn file_over_size_limit_reports_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("oversized_sample.txt");
        let rules = IndexRules::default();
        // 用 set_len 造一个刚超过上限的稀疏文件，只撑大小、不真写 20MB 数据。
        let f = fs::File::create(&path).unwrap();
        f.set_len(rules.max_file_bytes() + 1).unwrap();
        drop(f);
        assert_eq!(
            extract_text_detailed(&path),
            ExtractOutcome::Failure(ExtractFailure::Oversize)
        );
    }

    #[test]
    fn custom_lower_size_limit_skips_file_default_would_index() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mid.txt");
        // 造一个 2MB 的文件：默认 20MB 上限下能抽取，但把上限调到 1MB 就该跳过。
        let f = fs::File::create(&path).unwrap();
        f.set_len(2 * 1024 * 1024).unwrap();
        drop(f);

        let default_rules = IndexRules::default();
        assert!(
            matches!(
                extract_text_detailed_with(&path, &default_rules),
                ExtractOutcome::Text(_)
            ),
            "默认 20MB 上限下 2MB 文件应能抽取"
        );

        let tight = IndexRules {
            max_file_mb: 1,
            ..IndexRules::default()
        };
        assert_eq!(
            extract_text_detailed_with(&path, &tight),
            ExtractOutcome::Failure(ExtractFailure::Oversize),
            "上限收紧到 1MB 后 2MB 文件应报超限"
        );
    }

    #[test]
    fn extra_text_ext_becomes_extractable_and_reads_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("note.rst");
        fs::write(&path, "追加扩展名内容 extra-ext-marker").unwrap();

        let default_rules = IndexRules::default();
        assert!(
            !is_extractable_with(&path, &default_rules),
            "默认规则下 .rst 不是可抽取类型"
        );
        assert!(
            matches!(
                extract_text_detailed_with(&path, &default_rules),
                ExtractOutcome::Failure(ExtractFailure::UnsupportedFormat)
            ),
            "默认规则下 .rst 抽不出文本"
        );

        let with_rst = IndexRules {
            extra_text_exts: vec!["rst".into()],
            ..IndexRules::default()
        };
        assert!(
            is_extractable_with(&path, &with_rst),
            "追加 rst 后 .rst 应算可抽取"
        );
        let ExtractOutcome::Text(text) = extract_text_detailed_with(&path, &with_rst) else {
            panic!("追加 rst 后应能抽出文本");
        };
        assert!(text.contains("extra-ext-marker"));
    }

    #[test]
    fn misdetected_encoding_falls_back_to_lossy_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("misdetect_sample.txt");
        // 先放一大段合法 GBK 中文，让探测器有把握判成 GBK；结尾附一个非法的 GBK
        // 多字节序列（0x81 是前导字节，0x20 不是合法后继），逼 encoding_rs 解码时
        // had_errors=true，从而走 read_text_smart 里"猜错后有损回退"那条分支。
        let (gbk, _, _) = encoding_rs::GBK
            .encode("编码探测有损回退分支覆盖用例需要一段足够长的国标中文来稳定命中 GBK 判定");
        let mut buf = b"FALLBACK_MARKER_".to_vec();
        buf.extend_from_slice(&gbk);
        buf.extend_from_slice(&[0x81, 0x20]);
        fs::write(&path, &buf).unwrap();

        let text = read_text_smart(&path).expect("回退分支也要返回 Some");
        // 有损回退把整段字节当 UTF-8 读：ASCII 前缀存活，GBK 中文变成替换符，
        // 因此不会出现正确解出来的中文——以此证明走的是回退而非正常 GBK 解码。
        assert!(text.contains("FALLBACK_MARKER_"));
        assert!(
            !text.contains("国标中文"),
            "若走了正常 GBK 解码就会出现真中文，说明没命中回退分支: {text}"
        );
    }

    // --- 畸形 Office 文件 ---

    #[test]
    fn docx_missing_document_entry_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("no_document.docx");
        // 合法 zip，但缺少 word/document.xml 这个正文条目。
        write_zip(&path, &[("docProps/core.xml", "<coreProperties/>")]);
        assert_eq!(
            extract_text_detailed(&path),
            ExtractOutcome::Failure(ExtractFailure::Corrupted)
        );
    }

    #[test]
    fn docx_with_truncated_xml_keeps_text_before_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.docx");
        // 正文先给一段合法内容，再接一个对不上的结束标签，触发 XML 解析中途出错
        // （extract_tagged_text 里的 Err(_) => break 兜底分支）。
        let document_xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>partial-before-corruption</w:t></w:r></w:p></w:bogusEnd>"#;
        write_zip(&path, &[("word/document.xml", document_xml)]);

        let text = extract_text(&path).expect("损坏前已抽到的文本应保留");
        assert!(text.contains("partial-before-corruption"));
    }

    #[test]
    fn whitespace_only_docx_reports_no_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blank.docx");
        let document_xml = r#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>    </w:t></w:r></w:p></w:body></w:document>"#;
        write_zip(&path, &[("word/document.xml", document_xml)]);
        assert_eq!(
            extract_text_detailed(&path),
            ExtractOutcome::Failure(ExtractFailure::NoText)
        );
    }

    #[test]
    fn xlsx_skips_unreadable_sheet_and_processes_the_rest() {
        use zip::CompressionMethod;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("partial_sheets.xlsx");

        // sheet1 用 Stored（不压缩）写入，稍后直接改坏它的明文字节，读取时 CRC
        // 校验失败 → read_to_end 返回 Err → 命中 extract_xlsx 里的 continue 分支。
        // sheet2 正常，验证坏掉一个 sheet 不影响其它 sheet 的抽取。
        {
            let file = fs::File::create(&path).unwrap();
            let mut zip = zip::ZipWriter::new(file);
            let stored = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
            zip.start_file("xl/worksheets/sheet1.xml", stored).unwrap();
            zip.write_all(
                br#"<worksheet><sheetData><row><c t="inlineStr"><is><t>CORRUPTME_SHEET1_UNIQUE</t></is></c></row></sheetData></worksheet>"#,
            )
            .unwrap();
            zip.start_file("xl/worksheets/sheet2.xml", SimpleFileOptions::default())
                .unwrap();
            zip.write_all(
                br#"<worksheet><sheetData><row><c t="inlineStr"><is><t>survivor-sheet2-text</t></is></c></row></sheetData></worksheet>"#,
            )
            .unwrap();
            zip.finish().unwrap();
        }

        // 把 sheet1 的 Stored 明文里某个字节改掉，让它和存档记录的 CRC 对不上。
        let mut raw = fs::read(&path).unwrap();
        let needle = b"CORRUPTME_SHEET1_UNIQUE";
        let pos = raw
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("应能在 zip 明文里找到 sheet1 的标记");
        raw[pos] ^= 0xFF;
        fs::write(&path, &raw).unwrap();

        let text = extract_text(&path).expect("坏掉一个 sheet 后其它 sheet 仍应产出文本");
        assert!(
            text.contains("survivor-sheet2-text"),
            "健康 sheet 的文本应被抽出"
        );
        assert!(
            !text.contains("CORRUPTME_SHEET1_UNIQUE"),
            "坏掉的 sheet 内容不应出现在结果里"
        );
    }
}
