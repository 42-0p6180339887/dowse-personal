//! 只读搜索入口。[`Searcher::open`] 打开一个索引的只读句柄，
//! [`Searcher::search`]/[`Searcher::search_filtered`]/[`Searcher::search_advanced`]
//! 执行查询并返回 [`SearchHit`]（含 BM25 分数、命中片段、高亮区间），
//! [`SortMode`] 控制按相关性还是按 mtime/size 排序。也提供按路径取更大窗口
//! 预览内容（[`PreviewHit`]）和分块读取已索引正文（[`IndexedContentChunk`]）的能力。

use std::collections::{HashMap, VecDeque};
use std::ops::{Bound, Range};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, QueryParser, RangeQuery,
    RegexQuery, TermQuery,
};
use tantivy::schema::{Field, IndexRecordOption, Value};
use tantivy::snippet::SnippetGenerator;
use tantivy::tokenizer::{TokenStream, Tokenizer};
use tantivy::{DocAddress, Index, IndexReader, Order, TantivyDocument, Term};

use crate::query::{self, Cmp};
use crate::{Fields, build_schema, register_tokenizers};

/// 输入即搜的普通查询只接受不会触发 tantivy 查询语法的字符。引号、字段冒号、
/// 通配符等一律留给 QueryParser，避免为了前缀体验破坏既有高级语法。
#[derive(Debug, Clone, PartialEq, Eq)]
enum TypeaheadPart {
    Cjk(Vec<String>),
    Word(String),
}

/// 把“普通输入”编译成确定性的 autocomplete 片段，不调用 jieba。CJK 连续段按字
/// 组成短语，其他字母数字连续段组成词；空白、点、横线和下划线只是分隔符。
/// 这份结构只定义召回集合，jieba 稍后只能作为 Should 加分，不能新增命中。
fn plain_typeahead_parts(query: &str) -> Option<Vec<TypeaheadPart>> {
    if query.trim().is_empty()
        || !query
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch.is_whitespace() || matches!(ch, '-' | '_' | '.'))
    {
        return None;
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Cjk,
        Word,
    }

    let mut parts = Vec::new();
    let mut current = String::new();
    let mut kind: Option<Kind> = None;
    let flush = |parts: &mut Vec<TypeaheadPart>, current: &mut String, kind: &mut Option<Kind>| {
        if current.is_empty() {
            *kind = None;
            return;
        }
        let value = std::mem::take(current);
        match kind.take() {
            Some(Kind::Cjk) => parts.push(TypeaheadPart::Cjk(
                value.chars().map(String::from).collect(),
            )),
            Some(Kind::Word) => parts.push(TypeaheadPart::Word(value.to_lowercase())),
            None => {}
        }
    };

    for ch in query.trim().chars() {
        let next = if crate::tokenizer::is_cjk(ch) {
            Some(Kind::Cjk)
        } else if ch.is_alphanumeric() {
            Some(Kind::Word)
        } else {
            None
        };
        if next != kind {
            flush(&mut parts, &mut current, &mut kind);
        }
        if let Some(next) = next {
            kind = Some(next);
            current.push(ch);
        }
    }
    flush(&mut parts, &mut current, &mut kind);
    (!parts.is_empty()).then_some(parts)
}

fn typeahead_prefix_token(query: &str) -> Option<String> {
    if query.chars().last().is_some_and(char::is_whitespace) {
        return None;
    }
    match plain_typeahead_parts(query)?.pop()? {
        TypeaheadPart::Cjk(chars) => Some(chars.concat()),
        TypeaheadPart::Word(word) => Some(word),
    }
}

fn term_query(field: Field, text: &str) -> Box<dyn Query> {
    Box::new(TermQuery::new(
        Term::from_field_text(field, text),
        IndexRecordOption::WithFreqs,
    ))
}

fn cjk_phrase_query(field: Field, chars: &[String]) -> Box<dyn Query> {
    let terms: Vec<Term> = chars
        .iter()
        .map(|ch| Term::from_field_text(field, ch))
        .collect();
    if terms.len() == 1 {
        term_query(field, &chars[0])
    } else {
        Box::new(PhraseQuery::new(terms))
    }
}

/// 常见前缀直接查索引期 edge-ngram 的精确 term；超过索引上限的极端长词改走
/// Tantivy 的正则自动机。后者没有“最多展开 N 个词项”的截断，因此仍保持集合单调。
fn word_prefix_query(prefix_field: Field, word_field: Field, text: &str) -> Result<Box<dyn Query>> {
    if text.chars().count() <= crate::tokenizer::AUTOCOMPLETE_MAX_PREFIX_CHARS {
        return Ok(term_query(prefix_field, text));
    }
    Ok(Box::new(RegexQuery::from_pattern(
        &format!("{text}.*"),
        word_field,
    )?))
}

/// 结果排序方式。相关性是默认值——不传排序参数、或者从前端传来的字符串
/// 没解析出已知档位时都落回这里。其余三档对应浮窗"排序器"下拉的三个非默认项，
/// 底层用 tantivy `TopDocs::order_by_fast_field` 系列 API 在 mtime/size 这两个
/// v0.5.0 补了 FAST 属性的字段上排（见 lib.rs 的 build_schema）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortMode {
    /// 按 BM25 相关性分数排序（默认）。
    #[default]
    Relevance,
    /// 按修改时间降序：最新的排在前面。
    MtimeDesc,
    /// 按修改时间升序：最旧的排在前面。
    MtimeAsc,
    /// 按文件体积降序：最大的排在前面。
    SizeDesc,
    /// 按文件体积升序：最小的排在前面。结果表格点"大小"列头第二次用它——
    /// 只给降序的话，用户没法从"找那份大文件"切回"找那份小文件"。
    SizeAsc,
}

impl SortMode {
    /// 从字符串解析——浮窗前端和未来的 CLI 参数都用这个入口。未知值/None
    /// 一律落回相关性排序，不报错：排序档位是体验层面的偏好，不值得因为
    /// 一个拼错的字符串让整次搜索失败。
    ///
    /// # Examples
    ///
    /// ```
    /// use dowse::SortMode;
    ///
    /// assert_eq!(SortMode::parse(Some("mtime_desc")), SortMode::MtimeDesc);
    /// assert_eq!(SortMode::parse(Some("size_desc")), SortMode::SizeDesc);
    /// // 未知字符串和 None 都落回默认的相关性排序。
    /// assert_eq!(SortMode::parse(Some("bogus")), SortMode::Relevance);
    /// assert_eq!(SortMode::parse(None), SortMode::Relevance);
    /// ```
    pub fn parse(name: Option<&str>) -> Self {
        match name {
            Some("mtime_desc") => Self::MtimeDesc,
            Some("mtime_asc") => Self::MtimeAsc,
            Some("size_desc") => Self::SizeDesc,
            Some("size_asc") => Self::SizeAsc,
            _ => Self::Relevance,
        }
    }
}

/// 预览窗口目标字符数：命中词前后共约 1500 字，比搜索结果列表里的摘要长得多。
const PREVIEW_MAX_CHARS: usize = 1500;

/// 搜索结果列表里单条摘要的目标字符数（跟 tantivy `SnippetGenerator::set_max_num_chars` 对齐）。
const SNIPPET_MAX_CHARS: usize = 160;

/// 摘要生成的扫描窗口上限（字节）。
///
/// tantivy `SnippetGenerator` 的 `max_num_chars` 只决定“挑多长的片段来展示”，
/// 不决定“喂给分词器扫描的输入范围”——`search_fragments` 内部会把传入的整段
/// 文本从头到尾分词一遍，不会因为已经凑够展示长度就提前退出。如果直接把
/// 整篇 STORED content（可能几 MB）交给它，扫描耗时会随文档体积线性增长；
/// 短语/多词 AND 查询命中巨型文档时，单条摘要可能要花几百毫秒。更麻烦的是
/// BM25 打分偏爱词频更高的大文档，短语/多词查询恰好更容易把这类大文档挤进
/// Top-10，于是这个开销会稳定复现在整页结果上，不是个例。这里在调用
/// `snippet()`/`snippet_from_doc()` 之前手动把扫描输入截到这个字节数以内，
/// 把“分词扫描量”和“最终展示长度”解耦，从根上避免整篇重新分词。
const SNIPPET_SCAN_MAX_BYTES: usize = 128 * 1024;

/// 一条搜索命中。
///
/// [`Searcher::search`] 及其变体返回一批 `SearchHit`。`score` 只有在结果按相关性
/// 排序（[`SortMode::Relevance`]，也是默认排序）时才有意义——按 mtime/size 排序
/// 时它固定是 0.0，不要拿它做展示或二次排序。
pub struct SearchHit {
    /// 命中文件的绝对路径（Windows 上带 `\\?\` 扩展长度前缀，展示前用
    /// [`crate::display_path`] 剥掉）。
    pub path: String,
    /// BM25 相关性分数。只在 `SortMode::Relevance`（默认排序）下有意义；
    /// 按 mtime/size 排序时这里固定是 0.0，不要拿它做展示或二次排序。
    pub score: f32,
    /// 命中上下文片段，命中词的字节区间在 highlighted 里，渲染交给调用方
    pub snippet: String,
    /// 命中词的字节区间：已按起点排序且互不重叠，可直接顺序渲染。
    pub highlighted: Vec<Range<usize>>,
    /// 文件扩展名（不含点，小写），无扩展名时是空串。给 MCP 等消费方标注文件类型用。
    pub ext: String,
    /// 原文件体积（字节），来自建索引时存的 `size` 字段——**不是运行时 stat 文件系统**。
    /// 结果列表要显示体积/按体积排序时直接用这个，逐文件 stat 会让一页结果多出几十次
    /// 系统调用，而且文件被删/改名后就拿不到值了。
    pub size: u64,
    /// 原文件修改时间（毫秒级 Unix 时间戳），同样来自索引里的 `mtime` 字段，不是 stat。
    /// 取不到时为 0，调用方（前端）应对 0 显示为"—"而不是 1970-01-01。
    pub mtime: i64,
}

/// 按路径取的预览内容，字段契约和 SearchHit 的 snippet/highlighted 一致，
/// 只是窗口更大（约 1500 字而不是摘要用的 160 字）。
pub struct PreviewHit {
    /// 命中词周围的预览文本（约 1500 字），命中词区间见 `highlighted`。
    pub snippet: String,
    /// 命中词在 `snippet` 里的字节区间：已按起点排序且互不重叠，可直接顺序渲染。
    pub highlighted: Vec<Range<usize>>,
    /// 原文件体积（字节），来自建索引时存的 size 字段。
    pub size: u64,
    /// 原文件 mtime（毫秒级 Unix 时间戳），来自建索引时存的 mtime 字段。
    pub mtime: i64,
    /// 文件扩展名（不含点，小写），无扩展名时是空串。
    pub ext: String,
    /// 索引里这篇文档的 kind：正常文本是 "text"、图片是 "image"、抽取失败的
    /// 占位文档是 "err:xxx" 短码（见 [`crate::extract::ExtractFailure::kind_code`]）。
    /// 前端据此在预览区给出"不支持/加密/损坏/无可提取文字/超限"的明确状态。
    pub kind: String,
}

/// 按 Unicode 字符分页的一段已索引正文。
///
/// 内容来自 Tantivy 里的只读快照，不会重新打开原文件。这样既保持 MCP 的只读边界，
/// 也保证调用方只能读取已经明确纳入索引的文档。
pub struct IndexedContentChunk {
    /// 本页正文。
    pub content: String,
    /// 本页从全文第几个 Unicode 字符开始。
    pub offset_chars: usize,
    /// 本页实际返回的 Unicode 字符数。
    pub returned_chars: usize,
    /// 下一页起点；没有下一页时为 None。
    pub next_offset_chars: Option<usize>,
    /// 全文总 Unicode 字符数。和 `next_offset_chars == None` 一起，调用方可以
    /// 同时给出"还有内容/全文已加载"和整体进度。
    pub total_chars: usize,
}

/// 文内查找的结果。**坐标约定：所有区间一律按 Unicode 字符（标量值）计数**，
/// 与 [`Searcher::read_indexed_content_chunk`] 的 `offset_chars` 同一口径——
/// 调用方不要拿这些数字去切 Rust 侧的 UTF-8 字节串或 JS 侧的 UTF-16 下标，
/// 必须先转成各自的字符数组再定位，否则中文/emoji 会错位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindInDocResult {
    /// 全部命中的字符区间 `[start, end)`，按起点排序且互不重叠（贪心推进，
    /// 同一位置的重复命中只记一次）。
    pub matches: Vec<Range<usize>>,
    /// 被查文档的全文总字符数（Unicode 字符）。
    pub total_chars: usize,
}

/// 一页搜索结果：当前页命中列表加上"匹配总数"。
///
/// `hits` 是按 `sort` 排序、再按 `limit`/`offset` 截出的当前页；`total` 是整个查询
/// 匹配的文档数，不受 `limit`/`offset` 截断。分页调用方（MCP 的 search 工具）靠
/// `total` 判断"还有没有下一页"——`offset + hits.len() < total` 就说明后面还有，
/// 否则不必再翻，省得 agent 盲翻空页。
pub struct SearchPage {
    /// 当前页的命中列表，已按 `sort` 排序并按 `limit`/`offset` 截取。
    pub hits: Vec<SearchHit>,
    /// 满足查询的文档总数，不受 `limit`/`offset` 影响。
    pub total: usize,
}

/// 搜哪个字段——界面对应的就是"模式"下拉里的「内容 / 文件名」两项。
///
/// `Content` 是默认行为：**同时**查文件名和正文。这不是偷懒——用户搜"季度报告"
/// 时期望文件名叫"季度报告.docx"的排在前面，正文里出现该词的同样该出现。
/// `Name` 只在文件名上查，用来找"那个叫什么什么的文件"，此时正文里的同名词
/// 不该把一堆无关文档拉进结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchField {
    /// 文件名 + 正文（默认）。
    #[default]
    Content,
    /// 只查文件名。
    Name,
}

/// 正文缓存的字节预算。按**字节**而不是条目数算：一篇 5MB 的长文档和一篇
/// 5KB 的笔记不该占同样份额。32MB 足够"连着翻几篇长文"不重复解压，又不会让
/// 常驻内存无上限地涨。
const CONTENT_CACHE_BUDGET_BYTES: usize = 32 * 1024 * 1024;

/// 缓存的条目数上限。字节预算管的是"总共占多大"，但它挡不住**空正文和极短文**
/// ——那些条目字节数接近 0，只按字节算能堆到几十万条，`order` 队列和 HashMap 自身
/// 的开销（每条还要存一份路径字符串）照样能把内存吃满。所以两个上限一起卡，
/// 先到先触发。
const CONTENT_CACHE_MAX_ENTRIES: usize = 512;

/// 缓存里的一篇文档正文。
///
/// **只存一份 `Arc<str>`**——刻意不额外存完整的 `Vec<char>`、小写折叠副本等：
/// 那会让常驻内存翻两三倍，而它们都能在用到时现算。
#[derive(Clone)]
struct CachedDoc {
    content: Arc<str>,
    /// 全文字符数（Unicode 标量值）。每次分块读取都重新数一遍是纯浪费，
    /// 跟正文一起缓存。
    total_chars: usize,
}

/// 有界 LRU 正文缓存：按字节预算和条目数上限淘汰最久未用的文档。
///
/// 缓存绑定 Tantivy 的实际快照代号；自动重载与显式 reload 都会使旧正文失效。
/// 每次读取固定使用同一个快照查缓存和读正文，避免把不同版本混在一起。
///
/// 但"清空"和"回填"之间有个并发窗口：`cached_doc` 是在**释放锁之后**才去读索引
/// 的（读索引要走 Tantivy 文档存储，可能不快，不该压着锁做），如果这中间别人
/// reload 了，回填进去的就是**旧快照**的正文，而缓存已经认为自己是最新的了——
/// 那篇文档会一直返回过期内容直到下次 reload。用 `generation` 堵这个窗口：
/// reload 递增它，回填时比对读之前记下的代号，对不上就干脆不写（最多少缓存一次，
/// 下次读会补上）。桌面外层虽然用锁串行了这些调用，但 `Searcher` 是公开 API，
/// 不能指望调用方都这么做。
#[derive(Default)]
struct ContentCache {
    /// Tantivy may reload automatically without calling Searcher::reload.
    snapshot_generation: Option<u64>,
    map: HashMap<String, CachedDoc>,
    /// 最近使用顺序，队尾是最近用过的；淘汰从队头开始。
    order: VecDeque<String>,
    bytes: usize,
    /// 快照代号，每次 `clear` 递增。见结构体文档里的并发窗口说明。
    generation: u64,
}

impl ContentCache {
    fn get(&mut self, key: &str) -> Option<CachedDoc> {
        let doc = self.map.get(key)?.clone();
        // 命中就挪到队尾。用 position+remove 而不是 retain：这里的条目数最多
        // 几十个，O(n) 扫描比维护额外索引简单且足够快。
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
            self.order.push_back(key.to_string());
        }
        Some(doc)
    }

    fn put(&mut self, key: String, doc: CachedDoc) {
        let size = doc.content.len();
        // 单篇就超过整个预算的话干脆不缓存——否则它一进来就会把别的全挤掉，
        // 缓存退化成"只装得下一篇"还多一次拷贝开销。
        if size > CONTENT_CACHE_BUDGET_BYTES {
            return;
        }
        // 覆盖同一个 key 时**必须先把旧的位置从 order 里摘掉**，否则重复 put 同一
        // 篇会让 order 里堆满同一 key 的副本：队列长度和 map 条目数脱钩，后面淘汰
        // 时连着 pop 出来的都是已经不在 map 里的空转条目，真正的旧条目反而留着，
        // 内存就压不住了。
        if let Some(pos) = self.order.iter().position(|k| k == &key) {
            self.order.remove(pos);
        }
        if let Some(old) = self.map.remove(&key) {
            self.bytes = self.bytes.saturating_sub(old.content.len());
        }
        self.order.push_back(key.clone());
        self.map.insert(key, doc);
        self.bytes += size;
        // 两个上限一起卡：字节数（防长文吃满内存）和条目数（防大量空文/极短文
        // 靠"每条几乎不占字节"绕过字节预算）。
        while self.bytes > CONTENT_CACHE_BUDGET_BYTES
            || self.map.len() > CONTENT_CACHE_MAX_ENTRIES
        {
            let Some(victim) = self.order.pop_front() else { break };
            if let Some(old) = self.map.remove(&victim) {
                self.bytes = self.bytes.saturating_sub(old.content.len());
            }
        }
    }

    fn clear(&mut self) {
        self.snapshot_generation = None;
        self.map.clear();
        self.order.clear();
        self.bytes = 0;
        // 递增快照代号：让"清空之后才回填"的旧快照内容自己认不出来、不再写入。
        // 用 wrapping_add 而不是 +=：代号是拿来比对相等性的，溢出绕回去不会让
        // 它跟任何一个还在用的旧值撞上（要连续 reload 2^64 次才可能），真绕回去
        // 也不会 panic——release 下 += 溢出是静默回绕，debug 下直接崩，这不行。
        self.generation = self.generation.wrapping_add(1);
    }
}

/// 一个索引的只读搜索句柄。
///
/// `Searcher` 只读打开索引，可以和一个并发的写入端（[`crate::IndexUpdater`]、
/// 实时监听、全量重建）共存于同一份索引之上——tantivy 的段文件不可变，读写能
/// 跨线程甚至跨进程并行。reader 默认在 commit 后异步重载；需要立即读到最新
/// commit 时可显式调用 [`Searcher::reload`]。两种重载都会使旧正文缓存失效。
pub struct Searcher {
    reader: IndexReader,
    /// 默认解析器：不带字段前缀的词同时打到 `name` 和 `content` 上。
    parser: QueryParser,
    /// 只把词打到 `name` 一个字段上的解析器——"模式=文件名"时用它替换 `parser`。
    /// 预建而不是每次查询临时构造：`QueryParser::for_index` 要按字段名反查
    /// schema，搜索是热路径，不值得每敲一次键重做一遍。
    name_parser: QueryParser,
    /// 已索引正文的有界缓存。翻同一篇文档的第 2、3…块时不再重复从 Tantivy
    /// 文档存储读取并解压——`read_indexed_content_chunk` 原来每块都要把整篇
    /// 正文取出来再跳过 offset，越往后读重复工作越多。
    content_cache: Mutex<ContentCache>,
    fields: Fields,
}

/// 把调用方给的范围根路径，展开成索引里 `path` 字段**所有可能的写法**。
///
/// 索引里的 path 是 `canonicalize` 的产物：Windows 上是 `\\?\C:\a\b` 这种
/// verbatim 形态、分隔符是反斜杠。而调用方给的可能带 `\\?\` 也可能不带，手写
/// 命令行或前端拼串时还常写成正斜杠。这三件事不统一，前缀范围就一条都匹配不上，
/// 表现是"明明选了范围，结果却是空的"——比不过滤还糟。
///
/// 返回去重后的候选形态，由调用方用 Should 并起来（宁可多一个永不命中的分支）。
fn scope_root_variants(root: &str) -> Vec<String> {
    // 去掉结尾的分隔符：留着的话拼前缀时会多出一个分隔符，连 root 自身都匹配不到。
    let mut base = root.trim();
    while base.ends_with('/') || base.ends_with('\\') {
        base = &base[..base.len() - 1];
    }
    if base.is_empty() {
        return Vec::new();
    }
    // 正斜杠统一成本平台的分隔符（Windows 是 \，Unix 是 /）。
    let plain = if cfg!(windows) {
        base.replace('/', r"\")
    } else {
        base.to_string()
    };

    let mut out: Vec<String> = vec![plain.clone()];
    if cfg!(windows) {
        // 一边补 verbatim 前缀、一边剥掉它：调用方给不带前缀的就补，已经带了就剥，
        // 两种方向都要覆盖，不然总有一半调用方会踩空。
        let verbatim = format!(r"\\?\{plain}");
        if !out.contains(&verbatim) {
            out.push(verbatim);
        }
        if let Some(stripped) = plain.strip_prefix(r"\\?\")
            && !out.contains(&stripped.to_string())
        {
            out.push(stripped.to_string());
        }
    }
    out
}

impl Searcher {
    /// 打开一个已建好的索引的只读句柄。
    ///
    /// 索引目录不存在、或 schema 版本和当前库对不上（旧索引缺 mtime/size 字段）
    /// 时返回 `Err`，错误信息会提示先重建索引。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use dowse::Searcher;
    ///
    /// let searcher = Searcher::open(Path::new("./my-index"))?;
    /// for hit in searcher.search("关键词", 20)? {
    ///     println!("{}\t{}", hit.path, hit.snippet);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn open(index_dir: &Path) -> Result<Self> {
        let index = Index::open_in_dir(index_dir)
            .context("打不开索引目录，先跑 `dowse index <目录>` 建一次索引")?;
        // schema 版本对不上（旧索引缺 mtime/size 字段）直接报错、提示重建，
        // 不拿旧字段布局硬搜——搜出来的结果不可靠。
        crate::meta::ensure_schema_version(index_dir)?;
        register_tokenizers(&index);

        let (_, fields) = build_schema();
        // 不带字段前缀的查询词，默认同时查文件名和正文
        let mut parser = QueryParser::for_index(&index, vec![fields.name, fields.content]);
        // tantivy 默认词间是 OR，浮窗场景下多词应该收窄结果而不是放宽——
        // 用户敲"限流 中间件"是想要同时含两个词的文档，不是含任意一个词都行。
        // 带引号的短语查询不受影响，QueryParser 会先切出短语再对词间加 AND。
        parser.set_conjunction_by_default();
        // 只查文件名那份：字段列表只给 name，其余配置（AND 合取）跟默认解析器一致。
        let mut name_parser = QueryParser::for_index(&index, vec![fields.name]);
        name_parser.set_conjunction_by_default();
        let reader = index.reader()?;

        Ok(Self {
            reader,
            parser,
            name_parser,
            content_cache: Mutex::new(ContentCache::default()),
            fields,
        })
    }

    /// 执行一次全文搜索，返回至多 `limit` 条命中。
    ///
    /// 默认同时查 `name`（文件名）和 `content`（正文）两个字段。查询串里的多个词
    /// **默认按 AND 合取**：`"限流 中间件"` 只命中同时含这两个词的文档，不是含任
    /// 意一个即可（带引号的短语查询照常按相邻位置匹配）。结果按 BM25 相关性分数
    /// 排序。需要扩展名过滤或按 mtime/size 排序，改用
    /// [`search_filtered`](Self::search_filtered) / [`search_advanced`](Self::search_advanced)。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use dowse::Searcher;
    ///
    /// let searcher = Searcher::open(Path::new("./my-index"))?;
    /// let hits = searcher.search("分布式 限流", 10)?;
    /// for hit in &hits {
    ///     println!("{} ({:.3})", hit.path, hit.score);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn search(&self, query_str: &str, limit: usize) -> Result<Vec<SearchHit>> {
        self.search_advanced(query_str, limit, None, SortMode::Relevance)
    }

    /// 同 search()，多一个可选的扩展名过滤（不含点，大小写不敏感）。
    /// 给 MCP 的 search 工具用；单个扩展名是分组过滤的特例（长度为 1 的集合）。
    pub fn search_filtered(
        &self,
        query_str: &str,
        limit: usize,
        ext: Option<&str>,
    ) -> Result<Vec<SearchHit>> {
        let group: Option<&[&str]> = ext.as_ref().map(std::slice::from_ref);
        self.search_advanced(query_str, limit, group, SortMode::Relevance)
    }

    /// 浮窗"筛选/排序器"两件套的核心入口：query AND ext 分组过滤在 tantivy
    /// 查询层合取（分组内部是 Should 并集，跟 query 之间是 Must），排序按
    /// `sort` 选相关性打分或者 mtime/size 的 fast field 排序——都在查询层
    /// 完成，不是先拿结果再筛/再排，否则筛剩/排后的数量会少于调用方要求的 limit。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use dowse::{Searcher, SortMode, ext_group_by_name};
    ///
    /// let searcher = Searcher::open(Path::new("./my-index"))?;
    /// // 只在文档类扩展名里搜，按修改时间从新到旧排。
    /// let hits = searcher.search_advanced(
    ///     "季度 报告",
    ///     20,
    ///     ext_group_by_name(Some("doc")),
    ///     SortMode::MtimeDesc,
    /// )?;
    /// println!("命中 {} 条", hits.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn search_advanced(
        &self,
        query_str: &str,
        limit: usize,
        ext_group: Option<&[&str]>,
        sort: SortMode,
    ) -> Result<Vec<SearchHit>> {
        // 复用分页实现取第一页（offset=0），丢掉总数——不给这条老入口加参数，
        // 避免动到浮窗/CLI 等既有调用方的签名。多算的那次 Count 和 TopDocs 共用
        // 同一次检索（见 search_paged 里的 tuple 收集器），几乎不额外花代价。
        Ok(self
            .search_paged(query_str, limit, 0, ext_group, sort)?
            .hits)
    }

    /// 分页版 [`search_advanced`](Self::search_advanced)：多一个 `offset`（先跳过
    /// 多少条命中）并额外返回匹配总数（[`SearchPage::total`]）。query 与 ext 分组
    /// 过滤、排序都在 tantivy 查询层完成，`TopDocs` 和 `Count` 用同一次检索的
    /// tuple 收集器一起跑——两者本就都要遍历全部命中文档，合成一次遍历，总数几乎
    /// 是白捡的。`offset` 传 0 即取第一页。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use dowse::{Searcher, SortMode};
    ///
    /// let searcher = Searcher::open(Path::new("./my-index"))?;
    /// // 取第 2 页（每页 20 条）。
    /// let page = searcher.search_paged("季度 报告", 20, 20, None, SortMode::Relevance)?;
    /// println!("本页 {} 条，共 {} 条", page.hits.len(), page.total);
    /// # Ok(())
    /// # }
    /// ```
    pub fn search_paged(
        &self,
        query_str: &str,
        limit: usize,
        offset: usize,
        ext_group: Option<&[&str]>,
        sort: SortMode,
    ) -> Result<SearchPage> {
        self.search_scoped(
            query_str,
            limit,
            offset,
            ext_group,
            sort,
            None,
            SearchField::Content,
        )
    }

    /// 在 [`search_paged`](Self::search_paged) 之上多两个界面要用的维度：
    ///
    /// - `root`：把结果限定在某个索引根下（界面"范围"下拉）。走 path 字段的
    ///   **前缀范围**匹配，不是 `path:` 操作符那套分词子词 AND——后者会把
    ///   `D:\Other\Docs` 也算进"范围 = D:\Docs"里，筛选等于没生效。
    /// - `field`：查"内容"还是"只查文件名"（界面"模式"下拉）。
    ///
    /// 两者取默认值时与 `search_paged` 完全等价。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use dowse::{SearchField, Searcher, SortMode};
    ///
    /// let searcher = Searcher::open(Path::new("./my-index"))?;
    /// // 只在 D:\Docs 这个根下、只按文件名找。
    /// let page = searcher.search_scoped(
    ///     "季度", 20, 0, None, SortMode::Relevance,
    ///     Some("D:\\Docs"), SearchField::Name,
    /// )?;
    /// println!("{}", page.total);
    /// # Ok(())
    /// # }
    /// ```
    // 8 个入参确实到了 clippy 的上限，但这是"搜索"这一件事的全量维度（词、页码、
    // 类型、排序、范围、字段），拆成 builder 或参数结构体只是把同样的 8 项换个
    // 地方排——调用方要写的字一样多，却多了一层要记的类型名。这里保留平铺签名，
    // 靠默认值让多数调用方只填前 5 个。
    #[allow(clippy::too_many_arguments)]
    pub fn search_scoped(
        &self,
        query_str: &str,
        limit: usize,
        offset: usize,
        ext_group: Option<&[&str]>,
        sort: SortMode,
        root: Option<&str>,
        field: SearchField,
    ) -> Result<SearchPage> {
        let parser = match field {
            SearchField::Content => &self.parser,
            SearchField::Name => &self.name_parser,
        };
        // 把查询串解析成（检索用查询, 只含内容词的高亮查询）：普通输入走稳定的
        // autocomplete 召回；查询语法和结构化操作符仍按原 QueryParser/规则拼。摘要只
        // 认内容词那份，path:/mtime:/size: 这些操作符不进高亮（它们的 term 落在别的
        // 字段上，SnippetGenerator 绑定在 content 字段，本就不会拿它们去标原文，这里
        // 再用纯内容查询喂它是双保险，也让意图更清楚）。
        let typeahead_prefix = typeahead_prefix_token(query_str);
        let (retrieval_query, snippet_query) = self.build_queries(query_str, parser, field)?;

        // 过滤条件一律挂 Must：检索查询 AND 类型分组 AND 范围。
        let mut musts: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, retrieval_query)];
        if let Some(exts) = ext_group {
            if !exts.is_empty() {
                let ext_should: Vec<(Occur, Box<dyn Query>)> = exts
                    .iter()
                    .map(|ext| {
                        let term =
                            Term::from_field_text(self.fields.ext, &ext.to_ascii_lowercase());
                        (
                            Occur::Should,
                            Box::new(TermQuery::new(term, IndexRecordOption::Basic))
                                as Box<dyn Query>,
                        )
                    })
                    .collect();
                musts.push((
                    Occur::Must,
                    Box::new(BooleanQuery::new(ext_should)) as Box<dyn Query>,
                ));
            }
        }
        if let Some(root_query) = root.map(str::trim).filter(|r| !r.is_empty()).and_then(|r| self.build_root_query(r)) {
            musts.push((Occur::Must, root_query));
        }
        // 只有一个 Must 时不套 BooleanQuery——保持跟老代码一致的查询形状，
        // 也省一层 DocSet 组合开销。
        let query: Box<dyn Query> = if musts.len() == 1 {
            musts.pop().expect("刚判过长度为 1").1
        } else {
            Box::new(BooleanQuery::new(musts))
        };
        let searcher = self.reader.searcher();

        let mut snippet_gen =
            SnippetGenerator::create(&searcher, &snippet_query, self.fields.content)?;
        snippet_gen.set_max_num_chars(SNIPPET_MAX_CHARS);

        // 相关性排序保留真实 BM25 分数；其余三档按 fast field 排，doc 顺序
        // 就是最终顺序，score 字段填 0.0——非相关性排序下这个分数没有意义，
        // 调用方（前端/MCP）不应该拿它做展示或二次排序。
        //
        // 每档都把 `TopDocs`（带 offset）和 `Count` 打成 tuple 收集器一次跑完：
        // TopDocs 和 Count 本来都要遍历全部命中文档，合成一次遍历顺带拿到总数，
        // 供分页调用方判断还有没有下一页。
        let (addrs, total): (Vec<(f32, DocAddress)>, usize) = match sort {
            SortMode::Relevance => {
                let (top, total) = searcher.search(
                    &query,
                    &(
                        TopDocs::with_limit(limit)
                            .and_offset(offset)
                            .order_by_score(),
                        Count,
                    ),
                )?;
                (top, total)
            }
            SortMode::MtimeDesc => {
                let (top, total) = searcher.search(
                    &query,
                    &(
                        TopDocs::with_limit(limit)
                            .and_offset(offset)
                            .order_by_fast_field::<i64>("mtime", Order::Desc),
                        Count,
                    ),
                )?;
                (
                    top.into_iter().map(|(_, addr)| (0.0, addr)).collect(),
                    total,
                )
            }
            SortMode::MtimeAsc => {
                let (top, total) = searcher.search(
                    &query,
                    &(
                        TopDocs::with_limit(limit)
                            .and_offset(offset)
                            .order_by_fast_field::<i64>("mtime", Order::Asc),
                        Count,
                    ),
                )?;
                (
                    top.into_iter().map(|(_, addr)| (0.0, addr)).collect(),
                    total,
                )
            }
            SortMode::SizeDesc => {
                let (top, total) = searcher.search(
                    &query,
                    &(
                        TopDocs::with_limit(limit)
                            .and_offset(offset)
                            .order_by_fast_field::<u64>("size", Order::Desc),
                        Count,
                    ),
                )?;
                (
                    top.into_iter().map(|(_, addr)| (0.0, addr)).collect(),
                    total,
                )
            }
            SortMode::SizeAsc => {
                let (top, total) = searcher.search(
                    &query,
                    &(
                        TopDocs::with_limit(limit)
                            .and_offset(offset)
                            .order_by_fast_field::<u64>("size", Order::Asc),
                        Count,
                    ),
                )?;
                (
                    top.into_iter().map(|(_, addr)| (0.0, addr)).collect(),
                    total,
                )
            }
        };

        let mut hits = Vec::with_capacity(addrs.len());
        for (score, addr) in addrs {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let path = doc
                .get_first(self.fields.path)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            let content = doc
                .get_first(self.fields.content)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let ext = doc
                .get_first(self.fields.ext)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            // size/mtime 直接从索引的 fast field 读，跟 preview() 的 doc_meta 同一口径。
            // 结果列表要在表格里显示"修改时间/体积"并支持按这两列排序，若改成运行时
            // stat，一页 50 条就是 50 次系统调用，且文件被移动/删除后会拿不到值——
            // 索引里的值是建索引那一刻的快照，文件不在了也仍然可用。
            let size = doc
                .get_first(self.fields.size)
                .and_then(|v| v.as_u64())
                .unwrap_or(0);
            let mtime = doc
                .get_first(self.fields.mtime)
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            let (snippet, highlighted) = match typeahead_prefix.as_deref() {
                Some(prefix) => {
                    snippet_for_typeahead_prefix(&snippet_gen, content, prefix, SNIPPET_MAX_CHARS)
                }
                None => snippet_with_fallback(&snippet_gen, content, SNIPPET_MAX_CHARS),
            };
            hits.push(SearchHit {
                path,
                score,
                snippet,
                highlighted,
                ext,
                size,
                mtime,
            });
        }
        Ok(SearchPage { hits, total })
    }

    /// 把查询串翻成两份 tantivy 查询：`.0` 用于检索（含全部操作符条件），`.1` 只含
    /// 内容词、供摘要/预览高亮用。
    ///
    /// 分叉在这里：没有操作符且只含普通输入字符时走 autocomplete 查询计划；引号等
    /// QueryParser 语法仍交回原解析器；`path:` / `mtime:` / `size:` / `OR` / `NOT`
    /// 等结构化操作符走下面的显式拼装。
    /// `parser` 由调用方按"模式"选定：内容模式传 `self.parser`（name+content），
    /// 文件名模式传 `self.name_parser`（只 name）。两个解析器共用同一套
    /// 分词/合取配置，换掉它只改变词打到哪些字段上。
    fn build_queries(
        &self,
        query_str: &str,
        parser: &QueryParser,
        field: SearchField,
    ) -> Result<(Box<dyn Query>, Box<dyn Query>)> {
        let parsed = query::parse(query_str)?;
        if !parsed.has_operators {
            if let Some(parts) = plain_typeahead_parts(query_str) {
                let prefix_last = !query_str.chars().last().is_some_and(char::is_whitespace);
                return self.build_typeahead_queries(query_str, &parts, prefix_last, parser, field);
            }
            return Ok((
                parser.parse_query(query_str)?,
                parser.parse_query(query_str)?,
            ));
        }
        self.build_from_parsed(&parsed, parser)
    }

    /// 普通输入走独立 autocomplete 召回通道：每个 CJK 连续段是逐字短语，普通词的
    /// 最后一段查索引期前缀，之前的段精确 AND。主 jieba 查询只是 Should 加分，
    /// 所以分词边界无论怎样变化，都只能改变排序，不能让更长查询凭空新增文档。
    fn build_typeahead_queries(
        &self,
        query_str: &str,
        parts: &[TypeaheadPart],
        prefix_last: bool,
        parser: &QueryParser,
        field: SearchField,
    ) -> Result<(Box<dyn Query>, Box<dyn Query>)> {
        let last = parts.len() - 1;
        let mut recall = Vec::with_capacity(parts.len());
        let mut snippet = Vec::with_capacity(parts.len());

        for (index, part) in parts.iter().enumerate() {
            let (name_query, content_query, snippet_query) = match part {
                TypeaheadPart::Cjk(chars) => (
                    cjk_phrase_query(self.fields.name_chars, chars),
                    cjk_phrase_query(self.fields.content_chars, chars),
                    cjk_phrase_query(self.fields.content_chars, chars),
                ),
                TypeaheadPart::Word(word) if prefix_last && index == last => (
                    word_prefix_query(self.fields.name_prefixes, self.fields.name, word)?,
                    word_prefix_query(self.fields.content_prefixes, self.fields.content, word)?,
                    word_prefix_query(self.fields.content_prefixes, self.fields.content, word)?,
                ),
                TypeaheadPart::Word(word) => (
                    term_query(self.fields.name, word),
                    term_query(self.fields.content, word),
                    term_query(self.fields.content, word),
                ),
            };
            // 文件名模式下只保留 name 那一半。recall 里若还挂着 content 的 Should
            // 分支，"只查文件名"就退化成"两个字段任一命中"——这条路径不走
            // QueryParser，光换 parser 是拦不住的，必须在这里显式收窄。
            let recall_clauses = match field {
                SearchField::Content => vec![
                    (Occur::Should, name_query),
                    (Occur::Should, content_query),
                ],
                SearchField::Name => vec![(Occur::Should, name_query)],
            };
            recall.push((
                Occur::Must,
                Box::new(BooleanQuery::new(recall_clauses)) as Box<dyn Query>,
            ));
            // 高亮查询始终打在正文上：即使用户按文件名搜到了文件，点开预览时
            // 正文里出现该词的地方也该标出来。
            snippet.push((Occur::Must, snippet_query));
        }

        let stable_recall = Box::new(BooleanQuery::new(recall)) as Box<dyn Query>;
        let relevance_boost = BoostQuery::new(parser.parse_query(query_str)?, 2.0);
        Ok((
            Box::new(BooleanQuery::new(vec![
                (Occur::Must, stable_recall),
                (Occur::Should, Box::new(relevance_boost)),
            ])),
            Box::new(BooleanQuery::new(snippet)),
        ))
    }

    /// 把结构化的 [`query::Parsed`] 拼成（检索查询, 纯内容高亮查询）。
    ///
    /// 组内各条件 AND（`Occur::Must`，排除词走 `Occur::MustNot`），组间 OR
    /// （`Occur::Should`），落实"AND 优先级高于 OR"。纯内容查询把所有组的正内容词
    /// 以 `Should` 并起来喂给 SnippetGenerator——高亮只关心"哪些词出现在原文里"，
    /// 不需要复刻 AND/OR 的结构。
    fn build_from_parsed(
        &self,
        parsed: &query::Parsed,
        parser: &QueryParser,
    ) -> Result<(Box<dyn Query>, Box<dyn Query>)> {
        let mut group_queries: Vec<Box<dyn Query>> = Vec::new();
        // 所有组的正内容词并集，只给高亮用。
        let mut content_only: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        for group in &parsed.groups {
            let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();

            for term in &group.content {
                // 内容词原样交给指定的 parser（内容模式同时查 name+content，
                // 文件名模式只查 name；短语/子词都交给 QueryParser 处理）。
                clauses.push((Occur::Must, parser.parse_query(term)?));
                // 再解析一份进高亮集合——Box<dyn Query> 不能克隆，只能重解析一次。
                content_only.push((Occur::Should, parser.parse_query(term)?));
            }
            for term in &group.excluded {
                clauses.push((Occur::MustNot, parser.parse_query(term)?));
            }
            for path in &group.paths {
                clauses.push((Occur::Must, self.build_path_query(path)?));
            }
            for bound in &group.mtimes {
                clauses.push((Occur::Must, self.build_mtime_query(bound)));
            }
            for bound in &group.sizes {
                clauses.push((Occur::Must, self.build_size_query(bound)));
            }

            // 只有排除条件、没有任何正向条件的组（如 `-草稿` 单独成组）：tantivy 的
            // BooleanQuery 只有 MustNot 会匹配空集，补一个 AllQuery 当正向底座，
            // 语义才是"全部里排掉这些"。
            let has_positive = clauses.iter().any(|(occ, _)| *occ != Occur::MustNot);
            if !has_positive {
                clauses.push((Occur::Must, Box::new(AllQuery)));
            }
            group_queries.push(Box::new(BooleanQuery::new(clauses)));
        }

        let retrieval: Box<dyn Query> = if group_queries.len() == 1 {
            // 单组不套 OR 外壳，省一层 BooleanQuery。
            group_queries.pop().expect("已判定非空")
        } else {
            let shoulds = group_queries
                .into_iter()
                .map(|q| (Occur::Should, q))
                .collect();
            Box::new(BooleanQuery::new(shoulds))
        };

        // 纯内容查询可能为空（整串只有操作符、没有内容词）：空 BooleanQuery 不匹配任何
        // term，SnippetGenerator 拿不到高亮词，摘要走"文档开头无高亮"兜底，符合预期。
        let snippet: Box<dyn Query> = Box::new(BooleanQuery::new(content_only));
        Ok((retrieval, snippet))
    }

    /// 把一条 `path:` 操作数拼成 path_text 字段上的查询。
    ///
    /// 用跟索引侧同一个 [`crate::tokenizer::MixedTokenizer`] 把操作数切成 token，
    /// 保证"查"和"建"两侧口径一致（否则大小写/子词边界对不上就搜不到）。带引号的
    /// 操作数（`path:"my docs"`）且切出多个 token 时按短语（相邻位置）匹配，落实
    /// "按整体处理"；否则多个 token 之间 AND、单 token 直接一个 TermQuery。
    fn build_path_query(&self, path: &query::PathTerm) -> Result<Box<dyn Query>> {
        let mut tokenizer = crate::tokenizer::MixedTokenizer::new();
        let mut stream = tokenizer.token_stream(&path.operand);
        let mut terms: Vec<Term> = Vec::new();
        while stream.advance() {
            terms.push(Term::from_field_text(
                self.fields.path_text,
                &stream.token().text,
            ));
        }
        if terms.is_empty() {
            bail!(
                "path: 操作数 \"{}\" 里没有可检索的内容（全是标点/空白？）",
                path.operand
            );
        }
        if path.phrase && terms.len() >= 2 {
            return Ok(Box::new(PhraseQuery::new(terms)));
        }
        if terms.len() == 1 {
            let term = terms.pop().expect("刚判过长度为 1");
            return Ok(Box::new(TermQuery::new(term, IndexRecordOption::Basic)));
        }
        let clauses: Vec<(Occur, Box<dyn Query>)> = terms
            .into_iter()
            .map(|t| {
                (
                    Occur::Must,
                    Box::new(TermQuery::new(t, IndexRecordOption::Basic)) as Box<dyn Query>,
                )
            })
            .collect();
        Ok(Box::new(BooleanQuery::new(clauses)))
    }

    /// 把"只在某个索引根下搜"翻译成 path 字段上的**前缀范围**查询。
    ///
    /// 根路径的写法差异（正斜杠 / 结尾分隔符 / 有无 `\\?\` 前缀）交给
    /// [`scope_root_variants`] 归一化，这里只管把每种形态翻译成子树查询。
    ///
    /// 刻意不用 `path:` 操作符那套分词子词 AND（见 [`Self::build_path_query`]）：
    /// 那是把路径切成 token 各自匹配再合取，选了"范围 = D:\Docs"会把
    /// `D:\Other\Docs` 也捞进来——筛选等于没生效。这里在 path 字段（STRING，
    /// 整条路径一个 term）上按前缀扫 term 字典，语义才是真正的"路径以此开头"。
    fn build_root_query(&self, root: &str) -> Option<Box<dyn Query>> {
        let variants = scope_root_variants(root);
        // 空串 / 纯分隔符说明调用方没真的给范围，退回"不限"，不要生成一个永远
        // 匹配不到的查询把结果全过滤掉。
        if variants.is_empty() {
            return None;
        }
        let clauses: Vec<(Occur, Box<dyn Query>)> = variants
            .into_iter()
            .map(|root| (Occur::Should, self.path_subtree_query(&root)))
            .collect();
        Some(Box::new(BooleanQuery::new(clauses)))
    }

    /// "root 这一条 ∪ root 下的子树"：精确项 OR 前缀区间。
    ///
    /// **前缀必须补上路径分隔符**——否则"范围 = D:\Docs"会把 `D:\DocsOther\...`
    /// 也捞进来，同前缀的兄弟目录被误伤。这里跟 `count_under`、
    /// `updater.rs::delete_tree` 用同一套思路，刻意保持口径一致：否则会出现
    /// "托盘显示这个根 2 篇、界面筛出来 3 篇"这种两处对不上的现象。
    fn path_subtree_query(&self, root: &str) -> Box<dyn Query> {
        let mut prefix = root.to_string();
        if !prefix.ends_with(std::path::MAIN_SEPARATOR) {
            prefix.push(std::path::MAIN_SEPARATOR);
        }
        // 上界追加两个 U+10FFFF（它的 UTF-8 首字节 0xF4 是合法 UTF-8 里最大的
        // 首字节），取 Excluded，既盖住子树里的全部路径也不排掉 root 自身。
        let upper = format!("{prefix}\u{10FFFF}\u{10FFFF}");
        let exact = TermQuery::new(
            Term::from_field_text(self.fields.path, root),
            IndexRecordOption::Basic,
        );
        let subtree = RangeQuery::new(
            Bound::Included(Term::from_field_text(self.fields.path, &prefix)),
            Bound::Excluded(Term::from_field_text(self.fields.path, &upper)),
        );
        Box::new(BooleanQuery::new(vec![
            (Occur::Should, Box::new(exact) as Box<dyn Query>),
            (Occur::Should, Box::new(subtree) as Box<dyn Query>),
        ]))
    }

    /// 把一条 `mtime:` 过滤拼成 mtime 字段上的 [`RangeQuery`]。mtime 是 i64 FAST 字段，
    /// tantivy 0.26 的 RangeQuery 直接走 fast field 扫描，不需要额外索引属性。
    /// 日期已在 [`query`] 层归约成"当天/次日 0 点"两个毫秒边界，这里只按比较符
    /// 选开闭端（见 [`query::DateBound`] 对"整天"语义的说明）。
    fn build_mtime_query(&self, bound: &query::DateBound) -> Box<dyn Query> {
        let field = self.fields.mtime;
        let start = Term::from_field_i64(field, bound.start_ms);
        let next = Term::from_field_i64(field, bound.next_ms);
        let (lower, upper) = match bound.cmp {
            Cmp::Gt => (Bound::Included(next), Bound::Unbounded),
            Cmp::Ge => (Bound::Included(start), Bound::Unbounded),
            Cmp::Lt => (Bound::Unbounded, Bound::Excluded(start)),
            Cmp::Le => (Bound::Unbounded, Bound::Excluded(next)),
        };
        Box::new(RangeQuery::new(lower, upper))
    }

    /// 把一条 `size:` 过滤拼成 size 字段上的 [`RangeQuery`]。size 是 u64 FAST 字段，
    /// 同样走 fast field 范围扫描。阈值已在 [`query`] 层折算成字节。
    fn build_size_query(&self, bound: &query::SizeBound) -> Box<dyn Query> {
        let field = self.fields.size;
        let term = Term::from_field_u64(field, bound.bytes);
        let (lower, upper) = match bound.cmp {
            Cmp::Gt => (Bound::Excluded(term), Bound::Unbounded),
            Cmp::Ge => (Bound::Included(term), Bound::Unbounded),
            Cmp::Lt => (Bound::Unbounded, Bound::Excluded(term)),
            Cmp::Le => (Bound::Unbounded, Bound::Included(term)),
        };
        Box::new(RangeQuery::new(lower, upper))
    }

    /// 索引里的文档总数，给 CLI 的状态输出用。
    ///
    /// 读的是这个 searcher 当前 reader 快照里的数字——是打开（或上一次
    /// [`reload`](Self::reload)）那一刻的定格计数，不是实时值。并发写入端提交的
    /// 新文档要等下一次 `reload` 才会反映进来。
    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// `root` 前缀下的文档数——多根索引（里程碑 7）托盘"索引文件夹"子菜单
    /// 每根一项要显示"路径 + N 篇"。查询构造跟 `updater.rs::delete_tree`
    /// 是同一套"精确项 ∪ 前缀区间"思路（同前缀不误伤兄弟目录），这里只读
    /// 计数、不删文档。
    pub fn count_under(&self, root: &Path) -> Result<u64> {
        let exact = root.to_string_lossy().into_owned();
        let mut prefix = exact.clone();
        if !prefix.ends_with(std::path::MAIN_SEPARATOR) {
            prefix.push(std::path::MAIN_SEPARATOR);
        }
        let upper = format!("{prefix}\u{10FFFF}");

        let exact_query = TermQuery::new(
            Term::from_field_text(self.fields.path, &exact),
            IndexRecordOption::Basic,
        );
        let subtree_query = RangeQuery::new(
            Bound::Included(Term::from_field_text(self.fields.path, &prefix)),
            Bound::Excluded(Term::from_field_text(self.fields.path, &upper)),
        );
        let query = BooleanQuery::new(vec![
            (Occur::Should, Box::new(exact_query) as Box<dyn Query>),
            (Occur::Should, Box::new(subtree_query) as Box<dyn Query>),
        ]);

        let searcher = self.reader.searcher();
        Ok(searcher.search(&query, &Count)? as u64)
    }

    /// 重新加载 reader，读到最新一次 commit 的段。
    ///
    /// MCP server 是独立进程，只读打开浮窗侧持有写权的同一份索引：tantivy 的段文件
    /// 不可变、读写可跨进程共存，但 reader 不会自己感知到别的进程提交了新段。
    /// 调用方（MCP 工具处理函数）应在每次请求前调用一次，保证读到浮窗侧最新的索引状态。
    pub fn reload(&self) -> Result<()> {
        self.reader.reload().context("索引 reader 重载失败")?;
        // 快照换了，缓存必须整体作废——这是缓存唯一的失效点。放在 reload 里
        // 保证"换了快照就一定清空"，不会漏掉某条路径返回旧正文。
        let mut cache = self
            .content_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        cache.clear();
        Ok(())
    }

    /// 按精确路径分块读取已索引正文。
    ///
    /// `offset_chars` 和 `max_chars` 都按 Unicode 字符计数，避免调用方处理 UTF-8 字节
    /// 边界。路径不在索引里时返回 `None`；offset 超过全文长度时返回空的末页。
    pub fn read_indexed_content_chunk(
        &self,
        path: &str,
        offset_chars: usize,
        max_chars: usize,
    ) -> Result<Option<IndexedContentChunk>> {
        if max_chars == 0 {
            bail!("max_chars 必须大于 0");
        }
        let Some(doc) = self.cached_doc(path)? else {
            return Ok(None);
        };
        let total_chars = doc.total_chars;
        let mut chars = doc.content.chars().skip(offset_chars);
        let chunk: String = chars.by_ref().take(max_chars).collect();
        let returned_chars = chunk.chars().count();
        let end = offset_chars.saturating_add(returned_chars);
        let has_more = chars.next().is_some();

        Ok(Some(IndexedContentChunk {
            content: chunk,
            offset_chars,
            returned_chars,
            next_offset_chars: has_more.then_some(end),
            total_chars,
        }))
    }

    /// 取一篇文档的已索引正文，优先命中缓存。
    ///
    /// 命中时**完全不碰 Tantivy 文档存储**——翻长文的第 2、3…块时，这就是省掉的
    /// 主要开销（原来每读一块都要把整篇正文取出来再解压一遍）。快照变化或
    /// 显式 reload 时清空缓存，自动重载同样不会返回旧正文。
    fn cached_doc(&self, path: &str) -> Result<Option<CachedDoc>> {
        let snapshot = self.reader.searcher();
        let snapshot_generation = snapshot.generation().generation_id();
        // 命中/未命中这一段要在锁里做，顺便记下当前快照代号，然后**释放锁**再去读
        // 索引——读索引要走 Tantivy 文档存储，不该压着锁做。回填时再比对代号，
        // 期间有人 reload 过就放弃这次回填（详见 ContentCache 的文档）。
        let generation = {
            // 锁被毒化（上一个持锁线程 panic）时照旧取内部数据：缓存脏了最多是
            // 多读一次索引，不该让整个搜索线程跟着崩。
            let mut cache = self
                .content_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if cache.snapshot_generation != Some(snapshot_generation) {
                cache.clear();
                cache.snapshot_generation = Some(snapshot_generation);
            }
            if let Some(doc) = cache.get(path) {
                return Ok(Some(doc));
            }
            cache.generation
        };
        let Some(content) = self.stored_content_in(&snapshot, path)? else {
            return Ok(None);
        };
        let total_chars = content.chars().count();
        let doc = CachedDoc {
            content: Arc::from(content.as_str()),
            total_chars,
        };
        let mut cache = self
            .content_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 代号对不上说明这中间换过快照：手里这篇是旧的，写进去会让这篇文档一直
        // 返回过期正文，宁可不缓存。
        if cache.generation == generation {
            cache.put(path.to_string(), doc.clone());
        }
        Ok(Some(doc))
    }

    /// 按路径取该文档已索引的整篇正文（私有辅助，供 [`Self::cached_doc`]
    /// 和 [`find_in_document`](Self::find_in_document) 共用同一次按路径取文档
    /// 的查询逻辑）。路径不在索引里时返回 `None`。
    fn stored_content(&self, path: &str) -> Result<Option<String>> {
        let searcher = self.reader.searcher();
        self.stored_content_in(&searcher, path)
    }

    fn stored_content_in(&self, searcher: &tantivy::Searcher, path: &str) -> Result<Option<String>> {
        let path_query = TermQuery::new(
            Term::from_field_text(self.fields.path, path),
            IndexRecordOption::Basic,
        );
        let docs = searcher.search(&path_query, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, addr)) = docs.into_iter().next() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(addr)?;
        Ok(Some(
            doc.get_first(self.fields.content)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned(),
        ))
    }

    /// 文内字面查找：在指定文档的**已索引全文**上找 `needle` 的所有出现。
    ///
    /// 语义约定（自用版全文阅读的"文内查找"）：
    /// - 纯字面匹配，`needle` 里的正则符号（`. * + ? ( ) [ ]` 等）一律当普通
    ///   文字处理，不编译成任何模式；
    /// - 默认忽略**英文**大小写（ASCII 折叠），中文/emoji 不受影响；
    /// - 命中区间按 Unicode 字符坐标返回（见 [`FindInDocResult`]），跟
    ///   [`read_indexed_content_chunk`](Self::read_indexed_content_chunk) 的分块
    ///   坐标同一口径，调用方可以拿命中起点直接定位到对应正文块；
    /// - 全文一次性在字符数组上扫描，不存在分块边界，跨块关键词天然不漏；
    ///   贪心推进保证区间互不重叠、无重复计数；
    /// - 路径不在索引里时返回 `None`；`needle` 为空返回空命中列表。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use dowse::Searcher;
    ///
    /// let searcher = Searcher::open(Path::new("./my-index"))?;
    /// if let Some(result) = searcher.find_in_document("D:\\docs\\note.md", "限流")? {
    ///     println!("全文 {} 字，命中 {} 处", result.total_chars, result.matches.len());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn find_in_document(&self, path: &str, needle: &str) -> Result<Option<FindInDocResult>> {
        let Some(content) = self.stored_content(path)? else {
            return Ok(None);
        };
        let total_chars = content.chars().count();
        if needle.is_empty() {
            return Ok(Some(FindInDocResult {
                matches: Vec::new(),
                total_chars,
            }));
        }

        // 把全文和查找词都折成"ASCII 小写化"的字符数组再逐位比对：大小写不敏感
        // 只对英文字母生效，其它字符（中文/emoji/符号）原样保留，跟前端展示层
        // 的大小写折叠语义一致。
        let needle_folded: Vec<char> = needle.chars().map(fold_ascii_case).collect();
        let haystack: Vec<char> = content.chars().map(fold_ascii_case).collect();

        let n = needle_folded.len();
        let mut matches = Vec::new();
        let mut i = 0usize;
        while i + n <= haystack.len() {
            if haystack[i..i + n] == needle_folded[..] {
                matches.push(i..i + n);
                // 贪心推进到命中末尾：同一处文本不被重复计数（"aaa" 查 "aa"
                // 只记 1 次），相邻命中区间天然互不重叠。
                i += n;
            } else {
                i += 1;
            }
        }
        Ok(Some(FindInDocResult {
            matches,
            total_chars,
        }))
    }

    /// 按路径取该文档更长的预览上下文（约 1500 字），命中词区间跟 search() 同一契约。
    /// 用于浮窗右侧预览区：用户在结果列表里选中一行后，用它的 path 和当前查询词
    /// 换一份比列表摘要（160 字）长得多的窗口。
    ///
    /// path 找不到、或者该文档已不在索引里（比如原文件被删除后索引还没重建），返回 None。
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn main() -> anyhow::Result<()> {
    /// use std::path::Path;
    /// use dowse::Searcher;
    ///
    /// let searcher = Searcher::open(Path::new("./my-index"))?;
    /// let hits = searcher.search("限流器", 10)?;
    /// if let Some(hit) = hits.first() {
    ///     // 路径存在则拿到更长的预览窗口；文档已不在索引里则是 None。
    ///     match searcher.preview(&hit.path, "限流器")? {
    ///         Some(preview) => println!("{}", preview.snippet),
    ///         None => println!("该文档已不在索引里"),
    ///     }
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn preview(&self, path: &str, query_str: &str) -> Result<Option<PreviewHit>> {
        let searcher = self.reader.searcher();
        let typeahead_prefix = typeahead_prefix_token(query_str);
        let path_term = Term::from_field_text(self.fields.path, path);
        let path_query = TermQuery::new(path_term, IndexRecordOption::Basic);

        // path 精确匹配 AND 用户查询词的**内容词部分**：既锁定这一篇文档，又让
        // SnippetGenerator 拿到内容词去定位高亮位置。用内容词那份而不是整串，是为了
        // 让预览查询词里若带了 path:/mtime:/size: 等操作符时不会把预览搞挂——操作符
        // 交给搜索层过滤即可，预览只关心"高亮哪些词"。普通输入下这份对应正文侧的
        // autocomplete 召回条件。
        // 预览高亮只认正文那份查询——这里固定用内容解析器：即使搜索时选了
        // "模式=文件名"，预览里也该把正文中的命中词标出来（用户点开文件就是要看正文）。
        let (_retrieval, content_query) =
            self.build_queries(query_str, &self.parser, SearchField::Content)?;
        let combined = BooleanQuery::new(vec![
            (Occur::Must, Box::new(path_query.clone()) as Box<dyn Query>),
            (Occur::Must, content_query),
        ]);

        let top_docs = searcher.search(&combined, &TopDocs::with_limit(1).order_by_score())?;
        if let Some((_, addr)) = top_docs.into_iter().next() {
            let mut snippet_gen =
                SnippetGenerator::create(&searcher, &combined, self.fields.content)?;
            snippet_gen.set_max_num_chars(PREVIEW_MAX_CHARS);

            let doc: TantivyDocument = searcher.doc(addr)?;
            let content = doc
                .get_first(self.fields.content)
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let (snippet, highlighted) = match typeahead_prefix.as_deref() {
                Some(prefix) => {
                    snippet_for_typeahead_prefix(&snippet_gen, content, prefix, PREVIEW_MAX_CHARS)
                }
                None => snippet_with_fallback(&snippet_gen, content, PREVIEW_MAX_CHARS),
            };
            let (size, mtime, ext, kind) = self.doc_meta(&doc);
            return Ok(Some(PreviewHit {
                snippet,
                highlighted,
                size,
                mtime,
                ext,
                kind,
            }));
        }

        // 查询词和该文档实际不匹配（比如查询语法变了，或者调用方传了不相关的 query_str）：
        // 退回纯路径匹配，给文档开头一段没有高亮的预览，而不是彻底失败。
        let fallback = searcher.search(&path_query, &TopDocs::with_limit(1).order_by_score())?;
        let Some((_, addr)) = fallback.into_iter().next() else {
            return Ok(None);
        };
        let doc: TantivyDocument = searcher.doc(addr)?;
        let content = doc
            .get_first(self.fields.content)
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let window: String = content.chars().take(PREVIEW_MAX_CHARS).collect();
        let (size, mtime, ext, kind) = self.doc_meta(&doc);
        Ok(Some(PreviewHit {
            snippet: window,
            highlighted: vec![],
            size,
            mtime,
            ext,
            kind,
        }))
    }

    /// 从命中文档里取 (size, mtime, ext, kind) 四元组，preview() 的两条分支共用。
    fn doc_meta(&self, doc: &TantivyDocument) -> (u64, i64, String, String) {
        let size = doc
            .get_first(self.fields.size)
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let mtime = doc
            .get_first(self.fields.mtime)
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let ext = doc
            .get_first(self.fields.ext)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let kind = doc
            .get_first(self.fields.kind)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        (size, mtime, ext, kind)
    }
}

/// 文内查找的大小写折叠：只把 ASCII 大写字母折成小写，其余字符（中文、emoji、
/// 符号）原样保留。刻意不做完整 Unicode case folding——一是全量折叠会让某些
/// 字符折叠成一串（如 `İ`），字符坐标会对不上；二是自用场景关心的就是
/// "英文大小写不敏感、中文原样匹配"这一条。
fn fold_ascii_case(c: char) -> char {
    if c.is_ascii_uppercase() {
        c.to_ascii_lowercase()
    } else {
        c
    }
}

/// 把 tantivy SnippetGenerator 吐出的命中区间整理成有序且互不重叠的序列。
///
/// jieba 分词器会产出重叠 token（比如"分布式"同时切出"分布"和"分布式"），
/// SnippetGenerator 按 token 逐个给区间，顺序和是否重叠都不保证。
/// 这里按起点排序，再把重叠或相邻（下一个的 start <= 当前的 end）的区间合并，
/// 让调用方可以假设区间有序不重叠、按顺序切片渲染。
///
/// 对外暴露：dowse-app 的文件名高亮（highlight.rs）自己算出的匹配区间也要走
/// 同一套归并，两处共用这一份而不是各留一份。
pub fn normalize_ranges(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|r| r.start);

    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => {
                if r.end > last.end {
                    last.end = r.end;
                }
            }
            _ => merged.push(r),
        }
    }
    merged
}

/// 把 content 截到不超过 `SNIPPET_SCAN_MAX_BYTES` 字节的安全前缀，边界落在字符边界上，
/// 保证截出来的 `&str` 本身合法（不会切在 UTF-8 多字节字符中间导致 panic）。
fn truncate_scan_window(content: &str) -> &str {
    if content.len() <= SNIPPET_SCAN_MAX_BYTES {
        return content;
    }
    let mut end = SNIPPET_SCAN_MAX_BYTES;
    while end > 0 && !content.is_char_boundary(end) {
        end -= 1;
    }
    &content[..end]
}

/// 在 UTF-8 字符边界上查找用户仍在输入的最后一段。中文按原字面匹配，ASCII
/// 忽略大小写；返回的仍是原文 byte range，可直接交给前端切片。
fn literal_prefix_ranges(text: &str, prefix: &str) -> Vec<Range<usize>> {
    if prefix.is_empty() || prefix.len() > text.len() {
        return Vec::new();
    }

    text.char_indices()
        .filter_map(|(start, _)| {
            let end = start + prefix.len();
            (end <= text.len()
                && text.is_char_boundary(end)
                && text[start..end].eq_ignore_ascii_case(prefix))
            .then_some(start..end)
        })
        .collect()
}

/// autocomplete 辅助字段不属于正文原字段，SnippetGenerator 不会据此计算高亮。
/// 若扫描窗口里有字面前缀，主动
/// 取它周围的窗口并只高亮用户已经输入的部分；命中只在文件名或超长正文后段时，
/// 仍复用原来的安全兜底，保持截断边界不变。
fn snippet_for_typeahead_prefix(
    snippet_gen: &SnippetGenerator,
    content: &str,
    prefix: &str,
    max_chars: usize,
) -> (String, Vec<Range<usize>>) {
    let scan_window = truncate_scan_window(content);
    let Some(first) = literal_prefix_ranges(scan_window, prefix)
        .into_iter()
        .next()
    else {
        return snippet_with_fallback(snippet_gen, content, max_chars);
    };

    let hit_char = scan_window[..first.start].chars().count();
    let start_char = hit_char.saturating_sub(max_chars / 3);
    let start_byte = scan_window
        .char_indices()
        .nth(start_char)
        .map_or(scan_window.len(), |(index, _)| index);
    let fragment: String = scan_window[start_byte..].chars().take(max_chars).collect();
    let highlighted = normalize_ranges(literal_prefix_ranges(&fragment, prefix));
    (fragment, highlighted)
}

/// 在截断后的扫描窗口内生成摘要；如果窗口内一个命中区间都没有（比如命中词
/// 只出现在文档中后段、被截断切掉了），退回“文档开头一段、无高亮”的摘录，
/// 跟 `preview()` 里查询词和文档完全不匹配时的回退写法保持一致——
/// 都是“有文件但没有预览片段”的可接受降级，而不是给用户一个空字符串。
fn snippet_with_fallback(
    snippet_gen: &SnippetGenerator,
    content: &str,
    fallback_chars: usize,
) -> (String, Vec<Range<usize>>) {
    let scan_window = truncate_scan_window(content);
    let snippet = snippet_gen.snippet(scan_window);
    if !snippet.highlighted().is_empty() {
        return (
            snippet.fragment().to_owned(),
            normalize_ranges(snippet.highlighted().to_vec()),
        );
    }

    let head: String = content.chars().take(fallback_chars).collect();
    (head, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tantivy::doc;

    /// 取索引里记录的某篇文档的**精确路径**：先用 `path:` 操作符查一次，再从命中里
    /// 按文件名后缀挑出目标。
    ///
    /// **不要用 `std::fs::canonicalize` 自己拼这个路径。** Windows 上
    /// `canonicalize` 会把路径规整成长名 + `\\?\` 前缀的 verbatim 形式，而
    /// `tempfile` 的根目录本身可能还是 `TEMP` 的 8.3 短名——两侧拼法不一致，
    /// 拼出来的字符串跟索引里 `path` 字段存的那串对不上，按路径精确查询就会
    /// 一路返回 `None`（`preview` / `read_indexed_content_chunk` /
    /// `find_in_document` 全查不到文档）。索引里的 path 就是建索引时枚举目录
    /// 得到的原样值，从查询结果里取才和它同一口径。
    fn indexed_path(searcher: &Searcher, file_name: &str) -> String {
        // 查询操作数用不带扩展名的词干：`path:` 的解析器对带点的操作数处理起来
        // 更绕，而词干已经能唯一定位到这篇文档，后缀过滤那一步再卡死全名。
        let stem = file_name.split('.').next().unwrap_or(file_name);
        let hits = searcher
            .search(&format!("path:{stem}"), 20)
            .expect("按文件名查索引应成功");
        hits.into_iter()
            .find(|h| h.path.ends_with(file_name))
            .unwrap_or_else(|| panic!("索引里应按文件名找到 {file_name}"))
            .path
    }

    #[test]
    fn plain_typeahead_plan_is_deterministic_across_scripts() {
        assert_eq!(
            plain_typeahead_parts("北极 north-star_2026"),
            Some(vec![
                TypeaheadPart::Cjk(vec!["北".into(), "极".into()]),
                TypeaheadPart::Word("north".into()),
                TypeaheadPart::Word("star".into()),
                TypeaheadPart::Word("2026".into()),
            ])
        );
    }

    #[test]
    fn normalize_ranges_empty() {
        assert_eq!(normalize_ranges(vec![]), Vec::<Range<usize>>::new());
    }

    #[test]
    fn normalize_ranges_out_of_order() {
        let input = vec![10..13, 0..2, 5..7];
        assert_eq!(normalize_ranges(input), vec![0..2, 5..7, 10..13]);
    }

    #[test]
    fn normalize_ranges_fully_overlapping() {
        // "分布式" 和 "分布" 起点相同，"分布式" 覆盖更长
        let input = vec![0..9, 0..6];
        assert_eq!(normalize_ranges(input), vec![0..9]);
    }

    #[test]
    fn normalize_ranges_partially_overlapping() {
        let input = vec![0..6, 3..9];
        assert_eq!(normalize_ranges(input), vec![0..9]);
    }

    #[test]
    fn normalize_ranges_adjacent() {
        let input = vec![0..3, 3..6];
        assert_eq!(normalize_ranges(input), vec![0..6]);
    }

    #[test]
    fn search_jieba_overlapping_tokens_yields_sorted_nonoverlapping_ranges() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        // rebuild_index 会整棵跳过以 "." 开头的目录，tempfile 默认给临时目录
        // 起 ".tmpXXXXXX" 这样的名字，所以待索引目录要用不带点前缀的名字。
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(
            target_dir.path().join("note.md"),
            "系统采用分布式限流器保护后端服务。",
        )?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;

        let searcher = Searcher::open(index_dir.path())?;
        let hits = searcher.search("分布式", 10)?;

        assert!(!hits.is_empty(), "应该能搜到刚建的文档");
        for hit in &hits {
            for w in hit.highlighted.windows(2) {
                assert!(
                    w[0].end <= w[1].start,
                    "区间应互不重叠且按起点排序: {:?}",
                    hit.highlighted
                );
            }
            for r in &hit.highlighted {
                assert!(r.start <= r.end);
                assert!(r.end <= hit.snippet.len());
            }
        }
        Ok(())
    }

    #[test]
    fn find_in_document_uses_unicode_char_coordinates_not_bytes_or_utf16() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 头部放一个 emoji（1 个 Unicode 字符 = 4 个 UTF-8 字节 = 2 个 UTF-16 单元），
        // 后面紧跟 ASCII：三者坐标完全不同，正好用来钉死"按 Unicode 字符计数"。
        std::fs::write(
            target_dir.path().join("emoji.md"),
            "😀abc中文内容",
        )?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let path = indexed_path(&searcher, "emoji.md");
        let found = searcher
            .find_in_document(&path, "abc")?
            .expect("文档应在索引里");
        // "😀" 是第 0 个字符，"abc" 从第 1 个字符开始；若按字节算是 4，按 UTF-16 算是 2。
        assert_eq!(found.matches, vec![1..4], "命中应按 Unicode 字符坐标");
        assert_eq!(found.total_chars, 8, "总字符数：😀 + abc + 中文内容");

        // 中文命中同样走字符坐标（"中文内容" 从第 4 个字符开始）。
        let cn = searcher
            .find_in_document(&path, "内容")?
            .expect("文档应在索引里");
        assert_eq!(cn.matches, vec![6..8]);

        // emoji 自身也能被查到（折叠不改变它）。
        let emoji = searcher
            .find_in_document(&path, "😀")?
            .expect("文档应在索引里");
        assert_eq!(emoji.matches, vec![0..1]);
        Ok(())
    }

    #[test]
    fn find_in_document_is_literal_and_ascii_case_insensitive() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 正文里含正则元字符和大小写混排的英文。
        std::fs::write(
            target_dir.path().join("literal.md"),
            "限流器 Rate-Limiter 配置 a.b*c (v2) 结束",
        )?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let path = indexed_path(&searcher, "literal.md");

        // 英文大小写不敏感。
        let upper = searcher.find_in_document(&path, "RATE-LIMITER")?.unwrap();
        assert_eq!(upper.matches.len(), 1, "大写查询应命中: {upper:?}");
        let lower = searcher.find_in_document(&path, "rate-limiter")?.unwrap();
        assert_eq!(lower.matches, upper.matches, "大小写不应改变命中位置");

        // 正则元字符按普通文字处理：`a.b` 只匹配字面的 a.b，不把 "." 当通配。
        let literal_dot = searcher.find_in_document(&path, "a.b")?.unwrap();
        assert_eq!(literal_dot.matches.len(), 1, "a.b 应按字面命中一次");
        let wildcard_lookalike = searcher.find_in_document(&path, "aXb")?.unwrap();
        assert!(
            wildcard_lookalike.matches.is_empty(),
            "aXb 不应命中：查找词里的 \".\" 只能按字面匹配，不能当通配符"
        );

        // 中文按字面匹配，大小写折叠不影响它。
        let cn = searcher.find_in_document(&path, "限流器")?.unwrap();
        assert_eq!(cn.matches.len(), 1);

        // 不存在的词返回空列表，不是错误。
        let none = searcher.find_in_document(&path, "不存在的词")?.unwrap();
        assert!(none.matches.is_empty());
        Ok(())
    }

    #[test]
    fn find_in_document_greedy_matches_are_non_overlapping_and_deduped() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("overlap.md"), "aaaa")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let path = indexed_path(&searcher, "overlap.md");

        let result = searcher.find_in_document(&path, "aa")?.unwrap();
        assert_eq!(
            result.matches,
            vec![0..2, 2..4],
            "aaaa 查 aa 应得两个不重叠命中，不做重叠计数"
        );
        // 区间有序且互不重叠
        for w in result.matches.windows(2) {
            assert!(w[0].end <= w[1].start);
        }

        // 空查找词返回空命中列表（不算错误）。
        let empty = searcher.find_in_document(&path, "")?.unwrap();
        assert!(empty.matches.is_empty());
        Ok(())
    }

    #[test]
    fn find_in_document_finds_keyword_spanning_chunk_boundary() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 造一段长正文，让哨兵词正好跨在分块边界上：分块大小取 100 字符，
        // 词从第 97 个字符开始（占 6 个字符），横跨第 0 块和第 1 块。
        let head: String = "字".repeat(97);
        let content = format!("{head}跨块哨兵词{}", "尾".repeat(200));
        std::fs::write(target_dir.path().join("long.md"), &content)?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let path = indexed_path(&searcher, "long.md");

        let needle = "跨块哨兵词";
        let found = searcher.find_in_document(&path, needle)?.unwrap();
        assert_eq!(found.matches.len(), 1, "跨块关键词必须被找到: {found:?}");
        let hit = found.matches[0].clone();
        assert_eq!(hit.start, 97);

        // 按命中起点定位正文块。这里要钉住的是**前端分块阅读的契约**：命中可能
        // 横跨两个块（块边界正好落在词中间），只看起点所在块会把词尾截掉，必须
        // 把命中覆盖到的块都补齐、拼起来才是完整的关键词。
        const CHUNK: usize = 100;
        let first_offset = (hit.start / CHUNK) * CHUNK;

        let first_chunk = searcher
            .read_indexed_content_chunk(&path, first_offset, CHUNK)?
            .expect("文档应在索引里");
        assert!(
            !first_chunk.content.contains(needle),
            "用例前提：命中应横跨块边界，起点所在块不该已含完整关键词（否则这条用例没测到跨块）: {:?}",
            first_chunk.content
        );

        let mut combined = String::new();
        let mut offset = first_offset;
        let total_chars = first_chunk.total_chars;
        let mut chunk = first_chunk;
        loop {
            combined.push_str(&chunk.content);
            let covered_to = offset + chunk.returned_chars;
            match chunk.next_offset_chars {
                // 还没读到命中的结尾，接着读下一块。
                Some(next) if covered_to < hit.end => offset = next,
                _ => break,
            }
            chunk = searcher
                .read_indexed_content_chunk(&path, offset, CHUNK)?
                .expect("文档应在索引里");
        }
        assert!(
            combined.contains(needle),
            "覆盖命中所需的所有块拼起来必须含完整关键词（跨块不漏词）: {combined:?}"
        );
        assert_eq!(total_chars, found.total_chars, "两条读取路径的总字符数应一致");
        Ok(())
    }

    #[test]
    fn find_in_document_returns_none_for_unknown_path() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("known.md"), "内容")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        assert!(
            searcher
                .find_in_document("C:\\not\\in\\index.md", "内容")?
                .is_none(),
            "路径不在索引里时应返回 None"
        );
        Ok(())
    }

    #[test]
    fn read_indexed_content_chunk_reports_totals_and_end_of_document() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("doc.md"), "0123456789")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let path = indexed_path(&searcher, "doc.md");

        let first = searcher
            .read_indexed_content_chunk(&path, 0, 4)?
            .expect("文档应在索引里");
        assert_eq!(first.content, "0123");
        assert_eq!(first.returned_chars, 4);
        assert_eq!(first.next_offset_chars, Some(4), "还有内容");
        assert_eq!(first.total_chars, 10);

        let last = searcher
            .read_indexed_content_chunk(&path, 8, 4)?
            .expect("文档应在索引里");
        assert_eq!(last.content, "89");
        assert_eq!(last.returned_chars, 2);
        assert_eq!(last.next_offset_chars, None, "末页应明确告知没有下一页");
        assert_eq!(last.total_chars, 10);

        // 越界读取返回空末页，不是错误。
        let beyond = searcher
            .read_indexed_content_chunk(&path, 100, 4)?
            .expect("文档应在索引里");
        assert!(beyond.content.is_empty());
        assert_eq!(beyond.next_offset_chars, None);
        Ok(())
    }

    #[test]
    fn corrupted_docx_is_indexed_as_status_placeholder_searchable_by_name() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 一个损坏的 docx：抽取失败，但应落成 err:corrupted 占位文档——
        // 按文件名能搜到，预览能给出明确状态。
        std::fs::write(
            target_dir.path().join("损坏的季报.docx"),
            b"not a zip at all",
        )?;
        std::fs::write(target_dir.path().join("normal.md"), "正常内容")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search("损坏的季报", 10)?;
        let path = hits
            .iter()
            .find(|h| h.path.ends_with("损坏的季报.docx"))
            .unwrap_or_else(|| {
                panic!(
                    "抽取失败的文件仍应能按文件名搜到: {:?}",
                    hits.iter().map(|h| &h.path).collect::<Vec<_>>()
                )
            })
            .path
            .clone();

        let preview = searcher
            .preview(&path, "损坏的季报")?
            .expect("占位文档应有预览结果");
        assert_eq!(preview.kind, "err:corrupted", "预览应带上明确的失败状态");
        assert_eq!(preview.snippet, "", "占位文档没有正文");
        Ok(())
    }

    #[test]
    fn search_is_case_insensitive_for_latin() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("api.md"), "API design notes")?;
        std::fs::write(target_dir.path().join("fs.md"), "File system layout")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        for (query, expected) in [
            ("api", "api.md"),
            ("API", "api.md"),
            ("file", "fs.md"),
            ("FILE", "fs.md"),
        ] {
            let hits = searcher.search(query, 10)?;
            assert_eq!(hits.len(), 1, "查询 {query:?} 应命中一篇");
            assert!(
                hits[0].path.ends_with(expected),
                "查询 {query:?} 应命中 {expected}，实际 {:?}",
                hits[0].path
            );
        }
        Ok(())
    }

    #[test]
    fn search_hyphenated_string_matches_subwords() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("covid.md"), "covid-19 vaccine")?;
        std::fs::write(
            target_dir.path().join("marker.md"),
            "glimmerfrost-9931-unique-marker",
        )?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        for (query, expected) in [
            ("covid", "covid.md"),
            ("19", "covid.md"),
            ("glimmerfrost", "marker.md"),
            ("9931", "marker.md"),
            ("marker", "marker.md"),
        ] {
            let hits = searcher.search(query, 10)?;
            assert!(
                hits.iter().any(|h| h.path.ends_with(expected)),
                "子词查询 {query:?} 应命中 {expected}，实际 {:?}",
                hits.iter().map(|h| &h.path).collect::<Vec<_>>()
            );
        }

        // 整串查询：AND 语义下所有子词都在同一篇，应该命中。
        let full = searcher.search("glimmerfrost-9931-unique-marker", 10)?;
        assert!(
            full.iter().any(|h| h.path.ends_with("marker.md")),
            "整串查询应命中 marker.md，实际 {:?}",
            full.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn search_mixed_cjk_latin_document() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(
            target_dir.path().join("mixed.md"),
            "用 GPT-4 写的 covid-19 报告",
        )?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        for query in ["gpt", "covid", "报告"] {
            let hits = searcher.search(query, 10)?;
            assert!(
                hits.iter().any(|h| h.path.ends_with("mixed.md")),
                "混合文档查询 {query:?} 应命中 mixed.md，实际 {:?}",
                hits.iter().map(|h| &h.path).collect::<Vec<_>>()
            );
        }
        Ok(())
    }

    #[test]
    fn typeahead_prefixes_keep_recall_stable_across_scripts() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        for (name, content) in [
            (
                "brief.md",
                "北极星计划的检索质量用户访谈和周会纪要，搜索体验 northstar，版本 2026",
            ),
            (
                "meeting.md",
                "北极星计划检索质量复盘、用户访谈和周会纪要，搜索体验 northstar，目标 2026",
            ),
            ("north-cn.md", "北方项目检查记录"),
            ("north-en.md", "northbound migration notes"),
            ("number.md", "2048 capacity plan"),
            ("system.md", "搜索体系说明"),
            ("south.md", "南方项目复盘"),
        ] {
            std::fs::write(target_dir.path().join(name), content)?;
        }

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        for prefixes in [
            &["北", "北极", "北极星", "北极星计", "北极星计划"][..],
            &["检", "检索", "检索质", "检索质量"][..],
            &["用户", "用户访", "用户访谈"][..],
            &["n", "no", "nor", "north", "norths", "northstar"][..],
            &["2", "20", "202", "2026"][..],
            &["搜索 体", "搜索 体验"][..],
            &["周", "周会", "周会纪", "周会纪要"][..],
        ] {
            let mut previous: Option<std::collections::BTreeSet<String>> = None;
            for query in prefixes {
                let hits = searcher.search(query, 10)?;
                let paths: std::collections::BTreeSet<String> =
                    hits.iter().map(|hit| hit.path.clone()).collect();
                assert!(
                    paths.iter().any(|path| path.ends_with("brief.md"))
                        && paths.iter().any(|path| path.ends_with("meeting.md")),
                    "逐字输入 {query:?} 时不能丢失目标文档，实际 {paths:?}"
                );
                if let Some(previous) = &previous {
                    assert!(
                        paths.is_subset(previous),
                        "查询前缀变长时命中集合只能收窄，不能新增：{query:?} 得到 {paths:?}，上一步是 {previous:?}"
                    );
                }
                previous = Some(paths);
            }
        }
        Ok(())
    }

    #[test]
    fn cjk_typeahead_requires_real_adjacency() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("joined.md"), "北极项目")?;
        std::fs::write(target_dir.path().join("separated.md"), "北-A-极项目")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let hits = searcher.search("北极", 10)?;
        assert!(hits.iter().any(|hit| hit.path.ends_with("joined.md")));
        assert!(
            !hits.iter().any(|hit| hit.path.ends_with("separated.md")),
            "非 CJK 间隔不能被逐字字段吞掉"
        );
        Ok(())
    }

    #[test]
    fn typeahead_prefix_recall_is_not_capped_by_term_expansion() -> Result<()> {
        let (schema, fields) = build_schema();
        let index = Index::create_in_ram(schema);
        register_tokenizers(&index);
        let mut writer = index.writer(50_000_000)?;

        for id in 0..2_500 {
            let path = format!("doc-{id:04}.md");
            let content = format!("n{id:04}marker");
            writer.add_document(doc!(
                fields.path => path.clone(),
                fields.name => path.clone(),
                fields.ext => "md",
                fields.content => content.clone(),
                fields.mtime => 0i64,
                fields.size => 0u64,
                fields.kind => "text",
                fields.path_text => path,
                fields.name_chars => "",
                fields.content_chars => "",
                fields.name_prefixes => content.clone(),
                fields.content_prefixes => content,
            ))?;
        }
        writer.commit()?;

        let mut parser = QueryParser::for_index(&index, vec![fields.name, fields.content]);
        parser.set_conjunction_by_default();
        let mut name_parser = QueryParser::for_index(&index, vec![fields.name]);
        name_parser.set_conjunction_by_default();
        let searcher = Searcher {
            reader: index.reader()?,
            parser,
            name_parser,
            content_cache: Mutex::new(ContentCache::default()),
            fields,
        };

        let totals = ["n", "n2", "n24"]
            .into_iter()
            .map(|query| {
                searcher
                    .search_paged(query, 10, 0, None, SortMode::Relevance)
                    .map(|page| page.total)
            })
            .collect::<Result<Vec<_>>>()?;
        assert_eq!(totals, vec![2_500, 500, 100]);
        assert!(
            totals.windows(2).all(|pair| pair[1] <= pair[0]),
            "前缀词典再大也只能收窄召回"
        );
        Ok(())
    }

    #[test]
    fn very_long_word_prefixes_fall_back_without_breaking_monotonicity() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        let word = format!("{}z", "a".repeat(70));
        std::fs::write(target_dir.path().join("long.md"), &word)?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        for length in [64, 65, 70, 71] {
            let query: String = word.chars().take(length).collect();
            assert_eq!(
                searcher.search(&query, 10)?.len(),
                1,
                "长度 {length} 的前缀仍应命中"
            );
        }
        Ok(())
    }

    #[test]
    fn search_mixed_document_highlight_ranges_are_valid() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(
            target_dir.path().join("mixed.md"),
            "这是一份关于 glimmerfrost-9931 的中文说明，包含 API 设计与 covid-19 数据。",
        )?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search("glimmerfrost", 10)?;
        assert_eq!(hits.len(), 1);
        let hit = &hits[0];
        assert!(!hit.highlighted.is_empty(), "命中词应该被标记出来");
        for w in hit.highlighted.windows(2) {
            assert!(w[0].end <= w[1].start, "区间应互不重叠且按起点排序");
        }
        for r in &hit.highlighted {
            assert!(r.start <= r.end);
            assert!(r.end <= hit.snippet.len());
            // 切片不 panic，且切出来的确实是原查询子串（小写归一后一致）。
            assert!(hit.snippet.is_char_boundary(r.start));
            assert!(hit.snippet.is_char_boundary(r.end));
            assert_eq!(hit.snippet[r.start..r.end].to_lowercase(), "glimmerfrost");
        }
        Ok(())
    }

    #[test]
    fn phrase_query_matches_adjacent_terms_under_sequential_positions() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(
            target_dir.path().join("marker.md"),
            "glimmerfrost-9931-unique-marker",
        )?;
        std::fs::write(
            target_dir.path().join("cn.md"),
            "系统采用分布式限流器保护后端服务。",
        )?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // 英文短语：unique 和 marker 在新的顺序 position 方案下相邻，短语应命中。
        let phrase = searcher.search("\"unique marker\"", 10)?;
        assert!(
            phrase.iter().any(|h| h.path.ends_with("marker.md")),
            "短语 \"unique marker\" 应命中 marker.md，实际 {:?}",
            phrase.iter().map(|h| &h.path).collect::<Vec<_>>()
        );

        // 顺序打乱的短语不应命中（验证短语确实按相邻位置匹配，不是退化成 OR）。
        let reversed = searcher.search("\"marker unique\"", 10)?;
        assert!(
            !reversed.iter().any(|h| h.path.ends_with("marker.md")),
            "词序颠倒的短语不该命中，实际 {:?}",
            reversed.iter().map(|h| &h.path).collect::<Vec<_>>()
        );

        // 中文短语：证明短语语法对 jieba 段照样解析并匹配。
        let cn = searcher.search("\"分布式\"", 10)?;
        assert!(
            cn.iter().any(|h| h.path.ends_with("cn.md")),
            "中文短语应命中 cn.md，实际 {:?}",
            cn.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn search_multi_word_query_defaults_to_and() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 两个词都有
        std::fs::write(target_dir.path().join("both.md"), "限流中间件的实现细节")?;
        // 只有一个词
        std::fs::write(target_dir.path().join("one.md"), "限流方案对比笔记")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // tantivy 默认词间是 OR，这里验证已经被 Searcher::open 显式改成 AND：
        // 两个词都出现的文档才应该命中。
        let hits = searcher.search("限流 中间件", 10)?;
        assert_eq!(
            hits.len(),
            1,
            "AND 语义下只有同时含两个词的文档命中: {hits:?}",
            hits = hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(hits[0].path.ends_with("both.md"));
        Ok(())
    }

    #[test]
    fn search_filtered_by_ext_only_matches_that_extension() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("note.md"), "限流方案对比")?;
        std::fs::write(target_dir.path().join("note.txt"), "限流方案对比")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search_filtered("限流", 10, Some("md"))?;
        assert_eq!(
            hits.len(),
            1,
            "ext 过滤后应该只剩 .md 那篇: {hits:?}",
            hits = hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(hits[0].path.ends_with("note.md"));
        assert_eq!(hits[0].ext, "md");

        // 不传 ext 过滤，两篇都应该命中
        let unfiltered = searcher.search("限流", 10)?;
        assert_eq!(unfiltered.len(), 2);
        Ok(())
    }

    #[test]
    fn search_advanced_ext_group_matches_only_group_members() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("note.md"), "限流方案")?;
        std::fs::write(target_dir.path().join("main.rs"), "限流方案 fn main")?;
        std::fs::write(target_dir.path().join("note.txt"), "限流方案")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // 代码分组（BooleanQuery Should 并集）应该只命中 .rs，md/txt 都不在这组里。
        let code_hits = searcher.search_advanced(
            "限流",
            10,
            Some(crate::ext_groups::CODE),
            SortMode::Relevance,
        )?;
        assert_eq!(
            code_hits.len(),
            1,
            "代码分组应该只命中 .rs: {:?}",
            code_hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(code_hits[0].path.ends_with("main.rs"));

        // 文档分组同时含 md 和 txt，验证并集确实取的是"任意一个成员命中"。
        let doc_hits = searcher.search_advanced(
            "限流",
            10,
            Some(crate::ext_groups::DOC),
            SortMode::Relevance,
        )?;
        assert_eq!(
            doc_hits.len(),
            2,
            "文档分组应该命中 .md 和 .txt 两篇: {:?}",
            doc_hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn search_sorted_by_mtime_desc_orders_newest_first() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("old.md"), "笔记 alpha")?;
        // 相邻写入间隔一小段时间，保证两篇的 mtime 毫秒级可区分。
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(target_dir.path().join("new.md"), "笔记 beta")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search_advanced("笔记", 10, None, SortMode::MtimeDesc)?;
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].path.ends_with("new.md"),
            "mtime 降序，最新的应排第一: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(hits[1].path.ends_with("old.md"));
        Ok(())
    }

    #[test]
    fn search_sorted_by_mtime_asc_orders_oldest_first() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("old.md"), "笔记 alpha")?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(target_dir.path().join("new.md"), "笔记 beta")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search_advanced("笔记", 10, None, SortMode::MtimeAsc)?;
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].path.ends_with("old.md"),
            "mtime 升序，最旧的应排第一: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(hits[1].path.ends_with("new.md"));
        Ok(())
    }

    #[test]
    fn search_sorted_by_size_desc_orders_largest_first() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("small.md"), "笔记")?;
        std::fs::write(target_dir.path().join("big.md"), "笔记".repeat(500))?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search_advanced("笔记", 10, None, SortMode::SizeDesc)?;
        assert_eq!(hits.len(), 2);
        assert!(
            hits[0].path.ends_with("big.md"),
            "size 降序，最大的应排第一: {:?}",
            hits.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(hits[1].path.ends_with("small.md"));
        Ok(())
    }

    #[test]
    fn preview_returns_window_around_hit_with_normalized_ranges() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 构造一篇比 1500 字窗口长得多的文档，命中词埋在中间。
        let filler_before = "无关内容。".repeat(400);
        let filler_after = "更多无关内容。".repeat(400);
        let content = format!("{filler_before}这里是分布式限流器的核心实现。{filler_after}");
        let doc_path = target_dir.path().join("long.md");
        std::fs::write(&doc_path, &content)?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search("限流器", 10)?;
        assert_eq!(hits.len(), 1);
        let full_path = &hits[0].path;

        let preview = searcher
            .preview(full_path, "限流器")?
            .expect("文档存在，preview 不应为 None");

        assert!(!preview.snippet.is_empty());
        assert!(
            preview.snippet.chars().count() < content.chars().count(),
            "预览窗口应该比全文短——验证确实做了截窗而不是整篇塞回来"
        );
        assert!(!preview.highlighted.is_empty(), "命中词应该被标记出来");
        for w in preview.highlighted.windows(2) {
            assert!(w[0].end <= w[1].start, "区间应互不重叠且按起点排序");
        }
        for r in &preview.highlighted {
            assert!(r.end <= preview.snippet.len());
            assert!(preview.snippet.is_char_boundary(r.start));
            assert!(preview.snippet.is_char_boundary(r.end));
        }
        Ok(())
    }

    #[test]
    fn count_under_only_counts_docs_within_root_prefix() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        std::fs::write(target_dir.path().join("a.md"), "内容")?;
        std::fs::write(target_dir.path().join("b.md"), "内容")?;
        let sibling_named_with_shared_prefix = target_dir.path().with_file_name(format!(
            "{}-sibling",
            target_dir.path().file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling_named_with_shared_prefix)?;
        std::fs::write(sibling_named_with_shared_prefix.join("c.md"), "内容")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        assert_eq!(searcher.count_under(target_dir.path())?, 2);

        std::fs::remove_dir_all(&sibling_named_with_shared_prefix).ok();
        Ok(())
    }

    /// 界面"范围"下拉选某个根时，同前缀的**兄弟目录**不能被捞进来。
    ///
    /// `docs` 是 `docs-other` 的字符串前缀——如果范围查询退化成裸前缀匹配，
    /// 这里就会从 2 条变成 3 条。这条锁住"前缀必须补路径分隔符"这个细节，
    /// 也保证跟 `count_under` 口径一致（否则会出现"托盘显示 2 篇、筛出 3 篇"）。
    #[test]
    fn search_scoped_root_excludes_sibling_with_shared_prefix() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let parent = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        let target = parent.path().join("docs");
        let sibling = parent.path().join("docs-other");
        std::fs::create_dir_all(&target)?;
        std::fs::create_dir_all(&sibling)?;

        std::fs::write(target.join("a.md"), "共同关键词")?;
        std::fs::write(target.join("b.md"), "共同关键词")?;
        std::fs::write(sibling.join("c.md"), "共同关键词")?;

        crate::rebuild_index(index_dir.path(), parent.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let all = searcher.search_scoped(
            "共同关键词",
            20,
            0,
            None,
            SortMode::Relevance,
            None,
            SearchField::Content,
        )?;
        assert_eq!(all.total, 3, "不限范围时应命中全部 3 篇");

        let target_str = target.to_string_lossy().into_owned();
        let scoped = searcher.search_scoped(
            "共同关键词",
            20,
            0,
            None,
            SortMode::Relevance,
            Some(&target_str),
            SearchField::Content,
        )?;
        assert_eq!(
            scoped.total, 2,
            "范围 = docs 时不能把 docs-other 也算进来（前缀漏了分隔符？）"
        );
        for hit in &scoped.hits {
            assert!(
                hit.path.contains("docs"),
                "命中 {} 不在选定的根下",
                hit.path
            );
            assert!(
                !hit.path.contains("docs-other"),
                "命中 {} 来自兄弟目录，范围过滤没生效",
                hit.path
            );
        }
        Ok(())
    }

    /// 范围根路径的写法差异（正斜杠 / 结尾分隔符 / 有无 `\\?\` 前缀）必须都能归一，
    /// 否则"选了范围却一条都搜不到"——比不过滤还糟。
    #[test]
    fn scope_root_variants_normalize_separators_and_verbatim_prefix() {
        let v = scope_root_variants("C:/Users/me/Docs/");
        assert!(
            v.contains(&r"C:\Users\me\Docs".to_string()),
            "正斜杠要归一成反斜杠、结尾分隔符要去掉，实际 {v:?}"
        );
        if cfg!(windows) {
            assert!(
                v.contains(&r"\\?\C:\Users\me\Docs".to_string()),
                "要补一份 verbatim 形态，实际 {v:?}"
            );
        }
    }

    /// 已经是 verbatim 形态的输入要补一份剥掉前缀的版本（界面两边写法都可能出现）。
    #[test]
    fn scope_root_variants_cover_already_prefixed_input() {
        let v = scope_root_variants(r"\\?\C:\Users\me\Docs");
        assert!(v.contains(&r"\\?\C:\Users\me\Docs".to_string()));
        if cfg!(windows) {
            assert!(
                v.contains(&r"C:\Users\me\Docs".to_string()),
                "带前缀的输入要补一份剥掉前缀的形态，实际 {v:?}"
            );
        }
    }

    /// 空范围不能变成一个"永远匹配不到"的查询，必须退回"不限"。
    #[test]
    fn scope_root_variants_reject_empty() {
        assert!(scope_root_variants("").is_empty());
        assert!(scope_root_variants("   ").is_empty());
        assert!(scope_root_variants("///").is_empty());
    }

    /// "模式 = 文件名"只按文件名检索：正文里的同名词不该把无关文档拉进结果。
    #[test]
    fn search_scoped_name_field_only_matches_filenames() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        // 文件名含"季度"、正文没有。
        std::fs::write(dir.path().join("季度报告.md"), "这是一份普通说明文档")?;
        // 正文含"季度"、文件名没有。
        std::fs::write(dir.path().join("notes.md"), "季度营收情况汇总")?;

        crate::rebuild_index(index_dir.path(), dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let content = searcher.search_scoped(
            "季度",
            20,
            0,
            None,
            SortMode::Relevance,
            None,
            SearchField::Content,
        )?;
        assert_eq!(content.total, 2, "内容模式两篇都该命中");

        let name = searcher.search_scoped(
            "季度",
            20,
            0,
            None,
            SortMode::Relevance,
            None,
            SearchField::Name,
        )?;
        assert_eq!(name.total, 1, "文件名模式只该命中文件名含该词的那篇");
        assert!(
            name.hits[0].path.ends_with("季度报告.md"),
            "命中的应是文件名含该词的文档，实际 {}",
            name.hits[0].path
        );
        Ok(())
    }

    /// 正文缓存不能改变分块读取的语义：逐块读（第 2 块起命中缓存）拼起来
    /// 必须等于一次读到的全文；`reload()` 清空缓存之后结果也必须一致——
    /// 否则"缓存失效"这条就没真的生效。
    #[test]
    fn chunked_reads_are_identical_with_and_without_cache() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        // 远超一块（8000 字）的长度，强制走"第 1 块未命中 + 后续块命中缓存"。
        let body: String = "中文段落标记".repeat(3000);
        std::fs::write(dir.path().join("long.md"), &body)?;

        crate::rebuild_index(index_dir.path(), dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let hits = searcher.search("中文段落标记", 10)?;
        let path = hits[0].path.clone();

        // 一次性读全文作为基准
        let whole = searcher
            .read_indexed_content_chunk(&path, 0, 100_000)?
            .expect("文件应在索引里");
        assert!(whole.content.chars().count() > 8000, "语料要真的超过一块");

        // 逐块读（第二块起走缓存）
        let mut stitched = String::new();
        let mut offset = 0usize;
        loop {
            let chunk = searcher
                .read_indexed_content_chunk(&path, offset, 8000)?
                .expect("文件应在索引里");
            stitched.push_str(&chunk.content);
            match chunk.next_offset_chars {
                Some(next) => offset = next,
                None => {
                    assert_eq!(
                        stitched.chars().count(),
                        chunk.total_chars,
                        "拼起来的长度应等于全文字符数"
                    );
                    break;
                }
            }
        }
        assert_eq!(
            stitched, whole.content,
            "逐块读（命中缓存）拼起来必须等于一次读到的全文"
        );

        // reload 会清空缓存：清空后重读仍要一致，不能读到脏内容
        searcher.reload()?;
        let after = searcher
            .read_indexed_content_chunk(&path, 0, 8000)?
            .expect("reload 后仍应在索引里");
        assert!(
            whole.content.starts_with(&after.content),
            "reload 后读到的第一块应与之前一致"
        );
        Ok(())
    }

    /// 缓存失效得是**真的**：真实改掉文件内容、增量提交、再 reload，缓存不能还
    /// 攥着旧正文。只在内容不变时调 reload 的那种测试证明不了任何事——旧正文和
    /// 新正文一样，怎么读都"通过"。
    #[test]
    fn cache_is_invalidated_after_real_content_change_and_reload() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        // 目录名不能带 "." 前缀，否则会被索引器当隐藏目录整棵跳过。
        let dir = tempfile::Builder::new().prefix("dowse-upd-").tempdir()?;
        let file = dir.path().join("note.md");
        std::fs::write(&file, "最初的正文讲的是苹果")?;

        crate::rebuild_index(index_dir.path(), dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let path = searcher.search("苹果", 10)?[0].path.clone();

        let first = searcher
            .read_indexed_content_chunk(&path, 0, 1000)?
            .expect("文件应在索引里");
        assert!(first.content.contains("苹果"), "第一次读到的是旧正文");

        // 真的改内容 + 真的增量提交，而不是只调一下 reload。
        std::fs::write(&file, "改过之后的正文讲的是香蕉")?;
        let mut updater = crate::IndexUpdater::open(index_dir.path())?;
        updater.apply(&[crate::PendingChange {
            path: file.clone(),
            op: crate::PendingOp::Upsert,
        }])?;
        drop(updater);

        searcher.reload()?;
        let second = searcher
            .read_indexed_content_chunk(&path, 0, 1000)?
            .expect("文件应还在索引里");
        assert!(
            second.content.contains("香蕉"),
            "改内容并提交后 reload，缓存不能还返回旧正文，实际读到：{}",
            second.content
        );
        assert!(!second.content.contains("苹果"), "旧正文必须已经失效");
        Ok(())
    }

    #[test]
    fn cache_is_invalidated_when_reader_reloads_without_wrapper() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let dir = tempfile::Builder::new().prefix("dowse-auto-cache-").tempdir()?;
        let file = dir.path().join("note.md");
        std::fs::write(&file, "自动重载之前的苹果")?;
        crate::rebuild_index(index_dir.path(), dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let path = searcher.search("苹果", 10)?[0].path.clone();
        assert!(searcher.read_indexed_content_chunk(&path, 0, 1000)?.unwrap().content.contains("苹果"));

        std::fs::write(&file, "自动重载之后的香蕉🧪")?;
        let mut updater = crate::IndexUpdater::open(index_dir.path())?;
        updater.apply(&[crate::PendingChange { path: file.clone(), op: crate::PendingOp::Upsert }])?;
        // Bypass Searcher::reload, exactly as Tantivy's automatic reload does.
        searcher.reader.reload()?;
        let updated = searcher.read_indexed_content_chunk(&path, 0, 1000)?.unwrap();
        assert!(updated.content.contains("香蕉🧪"));
        assert!(!updated.content.contains("苹果"));

        std::fs::remove_file(&file)?;
        updater.apply(&[crate::PendingChange { path: file, op: crate::PendingOp::Remove }])?;
        searcher.reader.reload()?;
        assert!(searcher.read_indexed_content_chunk(&path, 0, 1000)?.is_none());
        Ok(())
    }

    /// 文件被真实删除后 reload：缓存和索引都要同步变成"读不到"。
    #[test]
    fn cache_is_invalidated_after_real_delete_and_reload() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let dir = tempfile::Builder::new().prefix("dowse-upd-").tempdir()?;
        let file = dir.path().join("doomed.md");
        std::fs::write(&file, "待删除文档独有词汇 zzqq")?;

        crate::rebuild_index(index_dir.path(), dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;
        let path = searcher.search("zzqq", 10)?[0].path.clone();
        assert!(
            searcher
                .read_indexed_content_chunk(&path, 0, 1000)?
                .is_some(),
            "删除前应该读得到"
        );

        std::fs::remove_file(&file)?;
        let mut updater = crate::IndexUpdater::open(index_dir.path())?;
        updater.apply(&[crate::PendingChange {
            path: file.clone(),
            op: crate::PendingOp::Remove,
        }])?;
        drop(updater);

        searcher.reload()?;
        assert!(
            searcher
                .read_indexed_content_chunk(&path, 0, 1000)?
                .is_none(),
            "删除后 reload，缓存不能把已删文档的正文继续返回"
        );
        Ok(())
    }

    /// 反复 put 同一个 key，`order` 里不能堆重复条目——否则淘汰时空转，条目数
    /// 和队列长度脱钩，内存压不住。
    #[test]
    fn cache_repeated_put_same_key_keeps_order_in_sync() {
        let mut cache = ContentCache::default();
        for i in 0..5 {
            let content = format!("第 {} 版正文", i);
            let total_chars = content.chars().count();
            cache.put(
                "same.md".to_string(),
                CachedDoc {
                    content: Arc::from(content.as_str()),
                    total_chars,
                },
            );
        }
        assert_eq!(cache.map.len(), 1, "同一个 key 只该有一条");
        assert_eq!(
            cache.order.len(),
            1,
            "order 里不能堆同一 key 的副本，实际 {:?}",
            cache.order
        );
        assert!(
            cache.get("same.md").expect("应命中").content.contains("第 4 版"),
            "应该读到最后一次写入的内容"
        );
    }

    /// 光靠字节预算挡不住大量"几乎不占字节"的短文档：条目数上限必须单独生效。
    #[test]
    fn cache_entry_cap_bounds_tiny_documents() {
        let mut cache = ContentCache::default();
        // 每篇都是空正文（字节数 0），字节预算永远触发不了。
        for i in 0..(CONTENT_CACHE_MAX_ENTRIES + 50) {
            cache.put(
                format!("tiny-{i}.md"),
                CachedDoc {
                    content: Arc::from(""),
                    total_chars: 0,
                },
            );
        }
        assert!(
            cache.map.len() <= CONTENT_CACHE_MAX_ENTRIES,
            "条目数必须被上限压住，实际 {}",
            cache.map.len()
        );
        assert_eq!(cache.order.len(), cache.map.len(), "order 与 map 必须同步");
    }

    /// `clear` 之后回填的旧快照内容要被代号挡掉，不能写进新快照的缓存里。
    #[test]
    fn cache_generation_blocks_stale_refill_after_clear() {
        let mut cache = ContentCache::default();
        cache.put(
            "a.md".to_string(),
            CachedDoc {
                content: Arc::from("旧正文"),
                total_chars: 3,
            },
        );
        let gen_before = cache.generation;
        cache.clear();
        assert_ne!(cache.generation, gen_before, "clear 必须递增快照代号");
        assert!(cache.map.is_empty());

        // 模拟并发窗口：拿着 clear 之前记下的代号回来回填，应该被拒绝。
        if cache.generation == gen_before {
            cache.put(
                "a.md".to_string(),
                CachedDoc {
                    content: Arc::from("旧正文"),
                    total_chars: 3,
                },
            );
        }
        assert!(
            cache.map.is_empty(),
            "代号对不上时不能写入，否则这篇会一直返回过期正文"
        );
    }

    #[test]
    fn preview_unknown_path_returns_none() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("note.md"), "随便写点什么")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;

        let searcher = Searcher::open(index_dir.path())?;
        let preview = searcher.preview("C:\\不存在\\的路径.md", "什么")?;
        assert!(preview.is_none());
        Ok(())
    }

    #[test]
    fn preview_falls_back_to_head_when_query_does_not_match_doc() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("note.md"), "这篇笔记不含查询词")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;

        let searcher = Searcher::open(index_dir.path())?;
        let hits = searcher.search("笔记", 10)?;
        let path = &hits[0].path;

        // 查询词和文档实际内容不匹配（调用方传了跟原查询不一致的词），
        // 应该退回文档开头预览，而不是返回 None 或报错。
        let preview = searcher
            .preview(path, "完全不相关的词汇")?
            .expect("路径存在，应该退回开头预览而不是 None");
        assert!(!preview.snippet.is_empty());
        assert!(preview.highlighted.is_empty(), "回退分支不应该产生假的高亮");
        Ok(())
    }

    #[test]
    fn read_indexed_content_chunk_pages_by_unicode_chars() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("note.md"), "甲乙丙丁abc")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;

        let searcher = Searcher::open(index_dir.path())?;
        let path = &searcher.search("甲乙", 1)?[0].path;
        let first = searcher
            .read_indexed_content_chunk(path, 0, 3)?
            .expect("已索引路径应该能读取");
        assert_eq!(first.content, "甲乙丙");
        assert_eq!(first.returned_chars, 3);
        assert_eq!(first.next_offset_chars, Some(3));

        let second = searcher
            .read_indexed_content_chunk(path, first.next_offset_chars.unwrap(), 10)?
            .expect("第二页仍应命中同一文档");
        assert_eq!(second.content, "丁abc");
        assert_eq!(second.next_offset_chars, None);
        Ok(())
    }

    #[test]
    fn search_path_operator_matches_by_directory_and_filename() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 一篇埋在带辨识度名字的子目录里，一篇直接放根下。
        let sub = target_dir.path().join("zzreportfolder");
        std::fs::create_dir_all(&sub)?;
        std::fs::write(sub.join("note.md"), "内容")?;
        std::fs::write(target_dir.path().join("other.md"), "内容")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // path: 命中目录名——只应中子目录里那篇。
        let by_dir = searcher.search("path:zzreportfolder", 10)?;
        assert_eq!(
            by_dir.len(),
            1,
            "path: 目录名应只中一篇: {by_dir:?}",
            by_dir = by_dir.iter().map(|h| &h.path).collect::<Vec<_>>()
        );
        assert!(by_dir[0].path.ends_with("note.md"));

        // path: 命中文件名词——only other.md 名里有 other。
        let by_name = searcher.search("path:other", 10)?;
        assert_eq!(by_name.len(), 1);
        assert!(by_name[0].path.ends_with("other.md"));

        // 内容词 AND path: 组合——两篇都含"内容"，但只有子目录那篇路径含 zzreportfolder。
        let combined = searcher.search("内容 path:zzreportfolder", 10)?;
        assert_eq!(combined.len(), 1);
        assert!(combined[0].path.ends_with("note.md"));
        Ok(())
    }

    #[test]
    fn search_path_operator_quoted_phrase_operand() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 目录名切成相邻的两个 token（my / docs），验证带引号按整体（相邻短语）匹配。
        let sub = target_dir.path().join("my docs");
        std::fs::create_dir_all(&sub)?;
        std::fs::write(sub.join("a.md"), "内容")?;
        std::fs::write(target_dir.path().join("b.md"), "内容")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search("path:\"my docs\"", 10)?;
        assert_eq!(hits.len(), 1, "带引号的 path 操作数应按整体匹配那一篇");
        assert!(hits[0].path.ends_with("a.md"));
        Ok(())
    }

    #[test]
    fn search_mtime_operator_filters_by_date() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("note.md"), "笔记内容")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // 文件 mtime 是"现在"，稳定地落在 2000 与 2099 之间，无需 sleep 也确定。
        assert_eq!(searcher.search("笔记 mtime:>2000-01-01", 10)?.len(), 1);
        assert_eq!(searcher.search("笔记 mtime:>=2000-01", 10)?.len(), 1);
        assert_eq!(
            searcher.search("笔记 mtime:<2000-01-01", 10)?.len(),
            0,
            "2000 年之前的过滤应把当前文件排除"
        );
        assert_eq!(
            searcher.search("笔记 mtime:>2099-01-01", 10)?.len(),
            0,
            "未来日期之后的过滤应无命中"
        );
        Ok(())
    }

    #[test]
    fn search_size_operator_filters_by_bytes() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("small.md"), "小")?;
        // 2KB 以上的大文件。
        std::fs::write(target_dir.path().join("big.md"), "大".repeat(2000))?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let big = searcher.search("size:>1kb", 10)?;
        assert_eq!(big.len(), 1, "size:>1kb 只应中大文件");
        assert!(big[0].path.ends_with("big.md"));

        let small = searcher.search("size:<1kb", 10)?;
        assert_eq!(small.len(), 1, "size:<1kb 只应中小文件");
        assert!(small[0].path.ends_with("small.md"));
        Ok(())
    }

    #[test]
    fn search_or_groups_and_within_group_and() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("a.md"), "alpha 内容")?;
        std::fs::write(target_dir.path().join("b.md"), "beta 内容")?;
        std::fs::write(target_dir.path().join("c.md"), "gamma 内容")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // OR：两组各中一篇。
        let either = searcher.search("alpha OR beta", 10)?;
        assert_eq!(either.len(), 2, "alpha OR beta 应中两篇");

        // OR 里有一组无命中：只中 alpha 那篇。
        let one = searcher.search("alpha OR zzznotthere", 10)?;
        assert_eq!(one.len(), 1);
        assert!(one[0].path.ends_with("a.md"));

        // 组内 AND 优先于 OR：`alpha 缺失词 OR beta` → 第一组 (alpha AND 缺失词) 无命中，
        // 只剩第二组 beta。
        let grouped = searcher.search("alpha zzmissing OR beta", 10)?;
        assert_eq!(grouped.len(), 1, "AND 组优先，只应中 beta 那篇");
        assert!(grouped[0].path.ends_with("b.md"));
        Ok(())
    }

    #[test]
    fn search_exclusion_via_dash_and_not() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("keep.md"), "限流 中间件")?;
        std::fs::write(target_dir.path().join("drop.md"), "限流 废弃")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        for query in ["限流 -废弃", "限流 NOT 废弃"] {
            let hits = searcher.search(query, 10)?;
            assert_eq!(hits.len(), 1, "查询 {query:?} 应排除含'废弃'那篇");
            assert!(hits[0].path.ends_with("keep.md"), "查询 {query:?} 结果不对");
        }
        Ok(())
    }

    #[test]
    fn search_invalid_operand_errors_not_silently_ignored() -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("note.md"), "内容")?;
        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // 非法操作数必须报错，而不是静默当普通词——否则用户以为过滤生效了。
        assert!(searcher.search("mtime:>abc", 10).is_err());
        assert!(searcher.search("size:>10tb", 10).is_err());
        assert!(
            searcher.search("mtime:2026-01-01", 10).is_err(),
            "缺比较符应报错"
        );
        Ok(())
    }

    #[test]
    fn plain_query_without_operators_is_unchanged_regression() -> Result<()> {
        // 不含操作符的查询要跟从前逐字节一致：多词 AND + 短语都照旧。
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;
        std::fs::write(target_dir.path().join("both.md"), "限流中间件的实现")?;
        std::fs::write(target_dir.path().join("one.md"), "限流方案对比")?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        // 多词默认 AND（无操作符走老路径）。
        let and_hits = searcher.search("限流 中间件", 10)?;
        assert_eq!(and_hits.len(), 1);
        assert!(and_hits[0].path.ends_with("both.md"));

        // 短语查询照旧。
        let phrase = searcher.search("\"限流中间件\"", 10)?;
        assert!(phrase.iter().any(|h| h.path.ends_with("both.md")));
        Ok(())
    }

    #[test]
    fn search_snippet_scan_truncation_keeps_retrieval_and_falls_back_beyond_scan_window()
    -> Result<()> {
        let index_dir = tempfile::tempdir()?;
        let target_dir = tempfile::Builder::new().prefix("dowse-test-").tempdir()?;

        // 哨兵词用纯英文字母数字串，避免中文分词器按上下文动态切词导致
        // 同一个词在不同句子里切法不一致（jieba 是上下文相关的统计分词，
        // 中文短语嵌进不同句子可能被切成不同的 token 边界）。
        const SENTINEL: &str = "zzzsentinelprobe888";

        // 大文档：哨兵词只出现在文档末尾，且前面的填充内容远超摘要扫描窗口
        // （128KB），所以扫描窗口内绝不会看到哨兵词。用于验证：
        // 1) 检索命中不受摘要截断影响（截断只影响摘要生成，不影响倒排检索）；
        // 2) 摘要生成因为扫描窗口内没有命中区间而退回无高亮兜底，不 panic。
        let filler = "填充内容占位符文本。".repeat(20_000);
        assert!(
            filler.len() > SNIPPET_SCAN_MAX_BYTES,
            "语料要真的超过截断阈值才能验证截断生效"
        );
        let big_content = format!("{filler}文档末尾出现了 {SENTINEL} 这个词。");
        std::fs::write(target_dir.path().join("big.md"), &big_content)?;

        // 小文档：哨兵词在开头，落在扫描窗口内，摘要应该正常带高亮。
        let small_content = format!("{SENTINEL} 出现在这篇小文档的开头。");
        std::fs::write(target_dir.path().join("small.md"), &small_content)?;

        crate::rebuild_index(index_dir.path(), target_dir.path())?;
        let searcher = Searcher::open(index_dir.path())?;

        let hits = searcher.search(SENTINEL, 10)?;
        assert_eq!(hits.len(), 2, "检索不该被摘要截断影响，两篇文档都要命中");

        let big_hit = hits
            .iter()
            .find(|h| h.path.ends_with("big.md"))
            .expect("大文档应该命中");
        assert!(
            big_hit.highlighted.is_empty(),
            "命中词被截断窗口挡在外面，摘要应退回无高亮兜底"
        );
        assert!(!big_hit.snippet.is_empty(), "兜底摘要不应该是空字符串");

        let small_hit = hits
            .iter()
            .find(|h| h.path.ends_with("small.md"))
            .expect("小文档应该命中");
        assert!(
            !small_hit.highlighted.is_empty(),
            "命中词在扫描窗口内，摘要应该正常高亮"
        );

        Ok(())
    }
}
