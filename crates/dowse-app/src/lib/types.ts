// 跟 Rust 侧 crates/dowse-app/src-tauri/src/commands.rs 与 window_fx.rs 的
// DTO 一一对应。命中区间在 Rust 侧已经切成 TextSegment 数组——这里不做任何
// 字节偏移换算，前端只管按顺序渲染 segments。

export interface TextSegment {
	text: string;
	highlighted: boolean;
	/** 文内查找的高亮段会带上它在当前文档里的全局命中序号（从 0 起），
	 * 用来标记"当前是第几个命中"并滚动定位。上下文高亮没有这个字段。 */
	matchIndex?: number;
}

export interface SearchHit {
	/** 打开文件、跳转文件夹用这个——可能带 Windows 扩展长度路径的 `\\?\` 前缀，别拿去展示。 */
	path: string;
	/** 结果行、预览区展示路径文本用这个——`\\?\` 前缀已经剥掉。 */
	display_path: string;
	name: string;
	name_segments: TextSegment[];
	snippet_segments: TextSegment[];
	score: number;
	/** 原文件体积（字节），来自索引里的 size 字段——不是运行时 stat 拿到的，
	 * 所以文件被移动/删除后这个值仍然可用。 */
	size: number;
	/** 原文件修改时间（**毫秒**级 Unix 时间戳），来自索引里的 mtime 字段。
	 * 为 0 表示建索引时没拿到，界面显示"—"而不是 1970-01-01。 */
	mtime: number;
	/** 扩展名（不含点，小写）；无扩展名时是空串。表格"类型"列直接用它，
	 * 不自己从文件名里截。 */
	ext: string;
}

export interface SearchPage {
	hits: SearchHit[];
	total: number;
}

export interface PreviewResult {
	segments: TextSegment[];
	/** 索引里这篇文档的 kind："text"/"image" 是正常内容；"err:xxx" 说明这篇
	 * 文档抽取失败（不支持/加密/损坏/无可提取文字/超限），预览区据此显示
	 * 明确状态而不是一片空白。短码取值见 Rust 侧 ExtractFailure::kind_code。 */
	kind: string;
}

/** 文内查找的一处命中。坐标是 **Unicode 字符**（不是字节、也不是 JS 的
 * UTF-16 下标）——跟 ContentChunk.offset_chars 同一口径，渲染前要把块内容
 * 转成码点数组（Array.from）再按这些数字切。 */
export interface DocMatch {
	start_char: number;
	end_char: number;
}

/** 文内查找结果（find_in_doc 命令）。 */
export interface FindInDocResult {
	matches: DocMatch[];
	total_chars: number;
}

/** 按块读取的已索引正文（read_content_chunk 命令）。 */
export interface ContentChunk {
	content: string;
	offset_chars: number;
	returned_chars: number;
	/** null 表示**全文已加载**（不是读取失败）。 */
	next_offset_chars: number | null;
	total_chars: number;
}

export interface IndexStats {
	indexed: number;
	skipped: number;
	seconds: number;
	/** 建索引结束时 OCR 队列里还没识别完的图片数，0 表示没有存量。 */
	ocr_pending: number;
	/** `skipped` 里因单文件体积超过规则里 `max_file_mb` 上限而被跳过的那一部分。
	 * `null` 表示这次操作拿不到这份明细（`add_root`/托盘单根重建走的是没有
	 * 这个细分字段的统计结构），不是"这次没有文件超限"。 */
	skipped_oversize: number | null;
}

/// 索引规则面板的读/写对象，跟 Rust 侧 `dowse::IndexRules` 一一对应——
/// 字段名保持 snake_case（不转 camelCase），跟这个代码库其它 DTO
/// （`IndexStatus`/`IndexStats` 等）的惯例一致。
export interface IndexRules {
	/** 整棵跳过的目录名列表（精确名匹配）。 */
	exclude_dirs: string[];
	/** 在内建文本扩展名白名单之外追加认定为纯文本的扩展名（不含点）。 */
	extra_text_exts: string[];
	/** 单文件体积上限（MB），超过则不抽取、跳过。 */
	max_file_mb: number;
}

/** `dowse://rebuild-progress` 事件载荷——建索引期间每处理若干文件推一次。 */
export interface IndexProgress {
	processed: number;
	path: string;
}

/// 状态栏的"索引占用"（index_usage 命令）。bytes 读不到时是 0，界面要显示
/// "—"而不是"0 B"——否则"读不到"会被显示成"索引不占空间"。
export interface IndexUsage {
	bytes: number;
	num_docs: number;
}

export interface IndexStatus {
	has_index: boolean;
	num_docs: number;
	/** 已注册的全部索引根，已经过 display_path 清洗（不带 `\\?\` 前缀），可直接渲染。 */
	roots: string[];
}

/// 建索引进度阶段——跟 Rust 侧 `indexing_status.rs::IndexingPhase` 一一对应。
export type IndexingPhase = 'idle' | 'text' | 'ocr';

/// `indexing_status` 查询命令的返回值，也是窗口每次呼出时用来"续播"进度视图
/// 的快照：事件流只在窗口开着时有意义，重新唤出窗口必须能补一次这份快照，
/// 不能是一片空白或者停在呼出前那一刻的旧状态。
export interface IndexingSnapshot {
	phase: IndexingPhase;
	text_processed: number;
	text_current_file: string;
	ocr_processed: number;
	ocr_total: number;
}

/// 类型筛选下拉的取值，跟 Rust 侧 dowse-core::ext_groups::by_name 认的字符串一一对应。
/// 两档：`doc`/`code`/`image` 是粗粒度，`pdf`/`word`/`excel`/`ppt`/`text` 是界面
/// "类型"下拉里那六个具体选项（对应"我要找那份 Excel"的实际心智）。
export type ExtGroup = 'all' | 'doc' | 'code' | 'image' | 'pdf' | 'word' | 'excel' | 'ppt' | 'text';

/// 排序下拉的取值，跟 Rust 侧 dowse-core::SortMode::parse 认的字符串一一对应。
export type SortOption =
	| 'relevance'
	| 'mtime_desc'
	| 'mtime_asc'
	| 'size_desc'
	| 'size_asc';

/// 搜索模式，跟 Rust 侧 dowse::SearchField 一一对应：'content' 查文件名+正文
/// （默认），'name' 只查文件名。
export type SearchField = 'content' | 'name';

export type EffectLevel = 'acrylic' | 'mica' | 'solid';

/// 透明度三档，跟 Rust 侧 `window_fx.rs::TransparencyTier`
/// （`serde(rename_all = "lowercase")`）一一对应。
export type TransparencyTier = 'low' | 'mid' | 'high';

/// 界面语言覆盖，跟 Rust 侧 `config.rs::AppConfig::lang` 一一对应：
/// 'auto' 跟随系统，'zh'/'en' 钉死为中/英。见 i18n.ts 顶部的启动镜像说明。
export type LangOption = 'auto' | 'zh' | 'en';

/// 设置面板通用区一次拉齐的初值，跟 Rust 侧 `commands.rs::SettingsDto`
/// 一一对应。`autostart_enabled` 是自启插件报告的真实系统态（不是 config 里
/// 语义不同的 `autostart_user_disabled`），字段名保持 snake_case 跟其它 DTO 一致。
export interface AppSettings {
	hotkey: string;
	transparency_enabled: boolean;
	transparency_tier: TransparencyTier;
	autostart_enabled: boolean;
	lang: LangOption;
}

/// 面板可视不透明度的明/暗两套 CSS alpha（0~1），跟托盘"透明度"三档挂钩，
/// 见 Rust 侧 window_fx.rs 的 `TransparencyTier`/`GlassAlpha`。
export interface GlassAlpha {
	light: number;
	dark: number;
}
