import { invoke } from '@tauri-apps/api/core';
import type {
	AppSettings,
	ContentChunk,
	EffectLevel,
	ExtGroup,
	FindInDocResult,
	GlassAlpha,
	IndexingSnapshot,
	IndexRules,
	IndexStats,
	IndexStatus,
	IndexUsage,
	LangOption,
	PreviewResult,
	SearchField,
	SearchPage,
	SortOption,
	TransparencyTier
} from './types';

export function indexStatus(): Promise<IndexStatus> {
	return invoke('index_status');
}

/// 建索引进度的当前快照——窗口每次呼出都应该拉一次，跟事件流（
/// `dowse://rebuild-progress`/`dowse://ocr-progress`）接续起来，见 +page.svelte
/// 的 `dowse://shown` 处理。
export function indexingStatus(): Promise<IndexingSnapshot> {
	return invoke('indexing_status');
}

/// 状态栏的"索引占用"：索引目录实际占用的磁盘字节数 + 文档数。
/// 不必每次搜索都拉——建索引完成、窗口呼出、索引变更时拉一次即可。
export function indexUsage(): Promise<IndexUsage> {
	return invoke('index_usage');
}

/// `root` 是搜索范围（限定在某个索引根下，null 表示全部资料），`field` 是搜索
/// 模式（'name' 只查文件名）。两者都在 Rust 侧按"未知值退化成默认"处理，前端
/// 传坏了不会让搜索报错。
export function search(
	query: string,
	limit = 50,
	offset = 0,
	extGroup: ExtGroup = 'all',
	sort: SortOption = 'relevance',
	root: string | null = null,
	field: SearchField = 'content'
): Promise<SearchPage> {
	return invoke('search', { query, limit, offset, extGroup, sort, root, field });
}

export function preview(path: string, query: string): Promise<PreviewResult | null> {
	return invoke('preview', { path, query });
}

/// 全文阅读：按 Unicode 字符分块读取**已索引正文**（不重新解析原文件）。
/// 返回 null 表示路径不在索引里（文件被删/改名/还没收录）。
/// `nextOffsetChars` 为 null 表示全文已加载。
export function readContentChunk(
	path: string,
	offsetChars: number,
	maxChars: number
): Promise<ContentChunk | null> {
	return invoke('read_content_chunk', { path, offsetChars, maxChars });
}

/// 文内查找：在指定文档的已索引全文上做字面查找（忽略英文大小写、正则符号
/// 按普通文字处理），返回 Unicode 字符坐标的命中区间。返回 null 表示路径
/// 不在索引里。
export function findInDoc(path: string, needle: string): Promise<FindInDocResult | null> {
	return invoke('find_in_doc', { path, needle });
}

export function openFile(path: string): Promise<void> {
	return invoke('open_file', { path });
}

export function revealInFolder(path: string): Promise<void> {
	return invoke('reveal_in_folder', { path });
}

export function rebuildIndex(dir: string): Promise<IndexStats> {
	return invoke('rebuild_index', { dir });
}

/// 添加一个索引根（多根索引）：不动现有内容，只对新目录做一次收录。
/// 空态"添加文件夹"链接走这个，跟 rebuildIndex 是姊妹命令，返回同一套统计。
export function addRoot(dir: string): Promise<IndexStats> {
	return invoke('add_root', { dir });
}

export function getEffectLevel(): Promise<EffectLevel> {
	return invoke('get_effect_level');
}

export function getGlassAlpha(): Promise<GlassAlpha> {
	return invoke('get_glass_alpha');
}

/// 当前生效的全局呼出快捷键，`tauri-plugin-global-shortcut` 的原始格式
/// （如 "Alt+Backquote"）——快捷键速查浮层拿去做人类可读的转换再显示。
export function getHotkey(): Promise<string> {
	return invoke('get_hotkey');
}

/// 按扩展名（不带点，小写与否都行）取系统关联图标的 PNG data URI，
/// 取不到返回 null——由调用方（FileIcon 组件）回落到手绘图标。
export function fileIcon(ext: string): Promise<string | null> {
	return invoke('file_icon', { ext });
}

/// 图钉开关：会话级，不落盘（重启回到 tauri.conf.json 的 alwaysOnTop: false）。
/// 打开后窗口置顶、始终显示在最上层，方便一边对照别的程序一边查文档。
export function setPinned(pinned: boolean): Promise<void> {
	return invoke('set_pinned', { pinned });
}

/// 结果行右键：在给定路径上弹出 Win32 原生上下文菜单（打开/打开所在
/// 文件夹/复制路径/复制文件名），菜单本身由 Rust 侧构造和处理，这里只是
/// 触发弹出，不需要等待用户选了哪一项。
export function showResultContextMenu(path: string): Promise<void> {
	return invoke('show_result_context_menu', { path });
}

/// 呼出延迟性能埋点：窗口 `dowse://shown` 之后确认首帧真正绘制完成（双重
/// requestAnimationFrame）才调这个，让 Rust 侧拿热键回调进入的单调时钟算
/// 差值打日志。非热键触发的显示（比如托盘点击）Rust 侧没有起始时刻，
/// 命令内部会静默跳过，前端不需要关心这个区分。
export function reportShownPerf(): Promise<void> {
	return invoke('report_shown_perf');
}

/// 击键到渲染性能埋点：搜索防抖触发、拿到结果、DOM 渲染完成后调一次。
/// `e2eMs` 含防抖等待（触发输入事件到渲染完成），`netMs` 不含（发起后端
/// 调用到渲染完成），`debounceMs` 是当前防抖窗口，一并打进日志避免端到端
/// 数字被误读。
export function reportSearchPerf(e2eMs: number, netMs: number, debounceMs: number): Promise<void> {
	return invoke('report_search_perf', { e2eMs, netMs, debounceMs });
}

/// Esc 收起浮窗。不用 `@tauri-apps/api/window` 的 `getCurrentWindow().hide()`——
/// 那走的是 Tauri core 插件的 `window|hide` 权限点，默认 capability 没放开，
/// 真机上会被 ACL 拒绝。这里走自定义命令，复用全局呼出快捷键同一条隐藏路径，
/// 自定义命令不受 ACL 权限点约束。
export function hideWindow(): Promise<void> {
	return invoke('hide_window');
}

/// 索引规则面板 Ctrl+, 打开时拉一次当前规则填表单初值。
export function getRules(): Promise<IndexRules> {
	return invoke('get_rules');
}

/// 索引规则面板"保存"：`maxFileMb` 必须是非负整数（Rust 侧是 u64，负数/小数
/// 会在反序列化阶段直接报错，调用方要先归一；0 会被 Rust 侧兜底成 1）；
/// 列表项的 trim/去空/大小写/去重由 Rust 侧统一处理，前端不用重复一遍
/// 规范化逻辑。返回规范化之后的最终值，用来回填表单，让展示的就是落盘的样子。
export function setRules(
	excludeDirs: string[],
	extraTextExts: string[],
	maxFileMb: number
): Promise<IndexRules> {
	return invoke('set_rules', {
		excludeDirs,
		extraTextExts,
		maxFileMb
	});
}

/// 设置面板打开时拉一次通用区（改键/透明/自启/语言）的全部初值。
export function getConfig(): Promise<AppSettings> {
	return invoke('get_config');
}

/// 设置面板"改键"：`hotkey` 是 `tauri-plugin-global-shortcut` 认的格式
/// （如 "Ctrl+Alt+KeyK"，修饰键在前、一个主键在后）。注册失败（多半被别的
/// 程序占用）时 Promise reject，错误文案里说清楚——Rust 侧已回滚到旧键。
export function setHotkey(hotkey: string): Promise<void> {
	return invoke('set_hotkey', { hotkey });
}

/// 设置面板"透明效果"开关——复用托盘同一条命令路径，托盘勾选态与面板同步。
export function setTransparencyEnabled(enabled: boolean): Promise<void> {
	return invoke('set_transparency_enabled', { enabled });
}

/// 设置面板"透明度"三档——复用托盘同一条命令路径。
export function setTransparencyTier(tier: TransparencyTier): Promise<void> {
	return invoke('set_transparency_tier', { tier });
}

/// 设置面板"开机自启"——复用托盘同一条命令路径。系统拒绝写自启项时 reject，
/// 调用方据此回滚 UI 勾选态。
export function setAutostart(enabled: boolean): Promise<void> {
	return invoke('set_autostart', { enabled });
}

/// 设置面板"界面语言"：只落盘，重启后生效（不做运行时热切换，见 i18n.ts）。
export function setLang(lang: LangOption): Promise<void> {
	return invoke('set_lang', { lang });
}
