<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { listen } from '@tauri-apps/api/event';
	import { getCurrentWindow } from '@tauri-apps/api/window';
	import { open } from '@tauri-apps/plugin-dialog';
	import { animate } from 'motion';

	import * as api from '$lib/api';
	import type {
		EffectLevel,
		ExtGroup,
		GlassAlpha,
		IndexingPhase,
		IndexingSnapshot,
		IndexProgress,
		IndexUsage,
		SearchField,
		SearchHit,
		SortOption,
		TextSegment
	} from '$lib/types';
	import ResultList from '$lib/components/ResultList.svelte';
	import PreviewPane from '$lib/components/PreviewPane.svelte';
	import ShortcutBar from '$lib/components/ShortcutBar.svelte';
	import EmptyState from '$lib/components/EmptyState.svelte';
	import GhostDropdown from '$lib/components/GhostDropdown.svelte';
	import PinButton from '$lib/components/PinButton.svelte';
	import SettingsButton from '$lib/components/SettingsButton.svelte';
	import AnimatedNumber from '$lib/components/AnimatedNumber.svelte';
	import ShortcutOverlay from '$lib/components/ShortcutOverlay.svelte';
	import IndexingStrip from '$lib/components/IndexingStrip.svelte';
	import SettingsPanel from '$lib/components/SettingsPanel.svelte';
	import { formatHotkey } from '$lib/hotkey';
	import { t, LANG_OVERRIDE_KEY } from '$lib/i18n';
	import { loadHistory, recordHistory, removeHistoryEntry, clearHistory } from '$lib/searchHistory';
	import { reduceIndexingView } from '$lib/indexingState';
	import {
		SEARCH_PAGE_SIZE,
		clampPageIndex,
		pageCount,
		pageOffset
	} from '$lib/pagination.js';
	import {
		DEFAULT_SPLIT_RATIO,
		SPLIT_DIVIDER_WIDTH,
		clampSplitWidth,
		normalizeSplitRatio,
		splitRatioFromWidth,
		splitWidthFromRatio
	} from '$lib/splitPane.js';

	// 类型筛选 / 排序器两个幽灵态下拉的选项表——顺序即菜单里的显示顺序，
	// 第一项永远是"默认值"（对应 GhostDropdown 的 defaultValue）。文案跟随系统语言。
	// 前六项就是界面"类型"下拉要的那六类常用文档，后三项保留粗粒度分组
	// （全部文档/代码/图片）——细分覆盖不到的场景还能退回大类别。
	const TYPE_OPTIONS = [
		{ value: 'all', label: t.filterAll },
		{ value: 'pdf', label: t.filterPdf },
		{ value: 'word', label: t.filterWord },
		{ value: 'excel', label: t.filterExcel },
		{ value: 'ppt', label: t.filterPpt },
		{ value: 'text', label: t.filterText },
		{ value: 'doc', label: t.filterDoc },
		{ value: 'code', label: t.filterCode },
		{ value: 'image', label: t.filterImage }
	];
	const SORT_OPTIONS = [
		{ value: 'relevance', label: t.sortRelevance },
		{ value: 'mtime_desc', label: t.sortNewest },
		{ value: 'mtime_asc', label: t.sortOldest },
		{ value: 'size_desc', label: t.sortLargest },
		{ value: 'size_asc', label: t.sortSmallest }
	];
	const MODE_OPTIONS = [
		{ value: 'content', label: t.modeContent },
		{ value: 'name', label: t.modeName }
	];
	const MATCH_OPTIONS = [
		{ value: 'normal', label: t.matchNormal },
		{ value: 'phrase', label: t.matchPhrase }
	];

	let query = $state('');
	let hits = $state<SearchHit[]>([]);
	let totalHits = $state(0);
	let pageIndex = $state(0);
	let searchPending = $state(false);
	let selectedIndex = $state(0);
	let hasIndex = $state<boolean | null>(null);
	let numDocs = $state(0);
	/** 已注册的全部索引根（已过 display_path 清洗），空态逐行列出。 */
	let roots = $state<string[]>([]);
	/// 状态栏"索引占用"；null 表示还没拉到（首次查询前显示"—"）。
	let indexUsage = $state<IndexUsage | null>(null);

	// 范围下拉：第一项"全部资料"，其余是已注册的索引根。根列表取自索引状态，
	// 一个根都没有时就只剩"全部资料"一项——此时没什么可挑，但仍照常渲染，
	// 免得工具栏在建索引前后宽度跳动。
	let SCOPE_OPTIONS = $derived([
		{ value: 'all', label: t.scopeAll },
		...roots.map((r) => ({ value: r, label: r }))
	]);
	let previewSegments = $state<TextSegment[] | null>(null);
	/// 选中文档在索引里的 kind：正常是 "text"/"image"，抽取失败是 "err:xxx"
	/// 短码（预览区据此显示"不支持/加密/损坏/无可提取文字/超限"的明确状态）。
	let previewKind = $state<string | null>(null);
	let previewLoading = $state(false);
	let rebuildState = $state<'idle' | 'rebuilding' | 'error'>('idle');
	let rebuildError = $state('');
	let toast = $state('');

	// 建索引"实时直播"：processed/currentFile 在整个重建期间随
	// dowse://rebuild-progress 事件滚动更新；report 只在重建成功的那一刻
	// 赋值，浮窗拿它替换掉实时计数、停留片刻后再收回引导层（见
	// pickDirectoryAndRebuild）。
	let indexingProcessed = $state(0);
	let indexingCurrentFile = $state('');
	let indexingReport = $state<{ indexed: number; seconds: number; skippedOversize?: number | null } | null>(null);

	// 建索引进度的"活的"状态：`indexingPhase` 是单一事实来源，决定要不要展示
	// 文本阶段的全屏引导层（'text'）、OCR 阶段的常驻进度条（'ocr'，不遮挡
	// 搜索结果）。事件流只在窗口开着时有意义，`refreshIndexingStatus` 在
	// 窗口每次呼出/挂载时拉一次快照续播，避免窗口隐藏期间错过事件后留下
	// 一片空白（症状 2/3 的验收场景：反复唤出/隐藏窗口，进度视图每次都活着）。
	let indexingPhase = $state<IndexingPhase>('idle');
	let indexingOcrProcessed = $state(0);
	let indexingOcrTotal = $state(0);

	// 每次"重建 / 添加根"操作一个自增序号：定时器回调用**捕获到的序号**跟当前
	// 序号比对，决定是否复位引导层——而不是像以前那样拿 `$state` 代理对象去跟原始
	// 对象比引用。Svelte 5 的 `$state` 读写都经过响应式代理，`indexingReport` 读出来
	// 的是代理、`report` 是普通对象，两者引用永远不同，旧写法
	// `if (indexingReport !== report) return;` 恒为 true，导致重建成功后
	// `rebuildState` / `indexingPhase` 永远回不到 idle、全屏引导层永远不收回、结果被
	// 遮挡（"7 篇 0.2 秒"一直挡着、搜出来的结果看不到）。这也是 03:21 release 被
	// Codex 抓到的 GUI P1。
	let indexingOpToken = 0;

	function applyIndexingEvent(
		event:
			| { type: 'text-progress'; progress: IndexProgress }
			| { type: 'snapshot'; snapshot: IndexingSnapshot }
	) {
		const next = reduceIndexingView(
			{
				phase: indexingPhase,
				textProcessed: indexingProcessed,
				textCurrentFile: indexingCurrentFile,
				ocrProcessed: indexingOcrProcessed,
				ocrTotal: indexingOcrTotal
			},
			event
		);
		indexingPhase = next.phase;
		indexingProcessed = next.textProcessed;
		indexingCurrentFile = next.textCurrentFile;
		indexingOcrProcessed = next.ocrProcessed;
		indexingOcrTotal = next.ocrTotal;
	}

	// 本次搜索耗时（发起请求到结果上屏），结果标题小字用；null 表示还没有可展示
	// 的一次搜索（空查询/首次挂载）。刻意不做滚动动画——每次搜索都变，
	// 动画反而晃眼。
	let lastSearchMs = $state<number | null>(null);

	let extGroup = $state<ExtGroup>('all');
	let sortOption = $state<SortOption>('relevance');
	/** 搜索范围：null = 全部资料，否则是某个索引根的路径。做成显式下拉而不是
	 * 让用户自己敲 `path:` 语法——记语法是给人添麻烦，且敲错了只会得到空结果。 */
	let scopeRoot = $state<string | null>(null);
	/** 搜索模式：'content' 查文件名+正文，'name' 只查文件名。 */
	let searchMode = $state<SearchField>('content');
	/** 匹配模式：'phrase' 给查询词整体加引号，按相邻词匹配。 */
	let matchMode = $state<'normal' | 'phrase'>('normal');
	let typeMenuOpen = $state(false);
	let sortMenuOpen = $state(false);
	let typeMenuIndex = $state(0);
	let sortMenuIndex = $state(0);
	// 范围/模式/匹配三个下拉各自的开合与高亮项——跟类型/排序是同一套机制，
	// 只是这三个默认常显（见工具栏），所以初始 index 直接对齐当前值。
	let scopeMenuOpen = $state(false);
	let modeMenuOpen = $state(false);
	let matchMenuOpen = $state(false);
	let scopeMenuIndex = $state(0);
	let modeMenuIndex = $state(0);
	let matchMenuIndex = $state(0);
	let pinned = $state(false);
	let shortcutOverlayOpen = $state(false);
	let hotkeyLabel = $state('Alt+`');
	/** 设置面板（Ctrl+, 打开）。跟 shortcutOverlayOpen 不同，这个面板里有真实
	 * 表单字段，打开时 DOM 焦点会移进面板本身（见 SettingsPanel 组件顶部
	 * 注释），不是靠 handleKeydown 拦截键盘。 */
	let settingsPanelOpen = $state(false);

	// 搜索历史：只在"打开了某条结果"的那一刻记（见 recordSearchHistory），
	// 不在击键/出结果时记。只在输入框为空时展示（historyMode），跟结果列表的
	// ↑↓/Enter 天然互斥——查询非空时 hits 才可能非空，两套导航不会抢键。
	let history = $state<string[]>([]);
	let historyIndex = $state(0);
	let historyMode = $derived(query.trim().length === 0 && history.length > 0);

	// 历史条目增删后夹紧选中下标，避免删空/删到末尾后指向一个不存在的位置。
	$effect(() => {
		if (historyIndex >= history.length) historyIndex = Math.max(0, history.length - 1);
	});

	let inputEl: HTMLInputElement | undefined = $state();
	let panelEl: HTMLDivElement | undefined = $state();
	let caretFlourishEl: HTMLSpanElement | undefined = $state();
	let controlsEl: HTMLDivElement | undefined = $state();
	let bodyEl: HTMLDivElement | undefined = $state();
	let splitRatio = $state(DEFAULT_SPLIT_RATIO);
	let resultsWidth = $state(0);
	let splitResizing = $state(false);
	const SPLIT_RATIO_KEY = 'dowse:results-preview-split';

	let selectedHit = $derived(hits[selectedIndex] ?? null);
	let totalPages = $derived(pageCount(totalHits));

	// 文本阶段（'text'）无论触发源（浮窗按钮/托盘"重建索引"/托盘"更改索引
	// 文件夹…"）都要接管成全屏引导层——`indexingPhase` 是跨触发源统一的
	// 事实来源；`rebuildState === 'rebuilding'` 仍然保留，覆盖"点击到第一次
	// 状态同步之间"那一小段间隙，避免按钮点下去的瞬间还没转譯成
	// indexingPhase 更新时闪一下结果视图。OCR 阶段（'ocr'）不在这里处理——
	// 它不遮挡搜索结果，见下面的 <IndexingStrip>。
	let guidanceKind = $derived.by((): 'idle' | 'no-index' | 'no-results' | 'rebuilding' | 'error' => {
		if (rebuildState === 'rebuilding' || indexingPhase === 'text') return 'rebuilding';
		if (rebuildState === 'error') return 'error';
		if (hasIndex === false) return 'no-index';
		if (query.trim().length === 0) return 'idle';
		if (hits.length === 0) return 'no-results';
		return 'idle';
	});
	let showGuidance = $derived(guidanceKind !== 'idle' || query.trim().length === 0);

	async function refreshIndexStatus() {
		try {
			const status = await api.indexStatus();
			hasIndex = status.has_index;
			numDocs = status.num_docs;
			roots = status.roots;
		} catch {
			hasIndex = false;
			numDocs = 0;
			roots = [];
		}
		// 索引占用单独拉：它要遍历索引目录统计字节，失败也不该影响上面那几个
		// 决定界面能不能用的字段。
		try {
			indexUsage = await api.indexUsage();
		} catch {
			indexUsage = null;
		}
	}

	/// OCR 队列进度的归一化更新：`pending` 是队列里还剩多少张待处理，跟
	/// Rust 侧 `indexing_status.rs::set_ocr_pending` 同一套语义（常驻监听
	/// 期间又发现新图片时顺势抬高 total，保证 processed 不会算出负数）。
	function applyOcrPending(pending: number) {
		if (pending <= 0) {
			indexingPhase = 'idle';
			indexingOcrProcessed = 0;
			indexingOcrTotal = 0;
			return;
		}
		indexingPhase = 'ocr';
		if (pending > indexingOcrTotal) indexingOcrTotal = pending;
		indexingOcrProcessed = Math.max(0, indexingOcrTotal - pending);
	}

	/// 拉一次后端的"当前建索引进度"快照，跟事件流接续起来——窗口每次呼出
	/// 都要调用一次：事件在窗口隐藏期间照样会发，但前端没监听、没地方存，
	/// 重新唤出时必须能补一次，不能是一片空白或者停在呼出前那一刻的旧状态。
	async function refreshIndexingStatus() {
		try {
			const snap = await api.indexingStatus();
			applyIndexingEvent({ type: 'snapshot', snapshot: snap });
		} catch {
			// 拉取失败（比如 Tauri IPC 一次性抖动）保留上一次已知状态，
			// 不主动清空——清空反而会让活着的进度视图短暂"消失"一下。
		}
	}

	// 短语模式：给查询词整体加引号，让引擎按相邻词匹配（"限流 中间件"在两个词
	// 紧挨着时才算命中）。已经带引号的不重复包一层——`""词""` 会被解析成空串，
	// 结果反而什么都搜不到。
	let effectiveQuery = $derived.by(() => {
		const q = query.trim();
		if (matchMode !== 'phrase' || q.length === 0) return q;
		if (q.length >= 2 && q.startsWith('"') && q.endsWith('"')) return q;
		return `"${q}"`;
	});

	/// 状态栏"索引占用"用的体积格式化。值为 0（索引目录读不到）时显示"—"——
	/// "读不到"不能显示成"0 B"，那会被当成"索引不占空间"。
	function formatBytes(bytes: number): string {
		if (!bytes) return t.statusNoIndexSize;
		if (bytes < 1024) return `${bytes} B`;
		if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
		if (bytes < 1024 * 1024 * 1024) return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
		return `${(bytes / 1024 / 1024 / 1024).toFixed(2)} GB`;
	}

	// 键入 30ms 防抖即搜——查询词变了就重新发起，过期响应用 token 挡掉。
	// 类型筛选 / 排序器的选择跟查询词一起参与防抖：选中即重搜，不需要额外
	// 的"应用"按钮。这个常量同时喂给下面的 setTimeout 和 reportSearchPerf
	// 的日志文案，避免两处各写一份 30 而将来改漏一处。
	const SEARCH_DEBOUNCE_MS = 30;
	let searchToken = 0;
	let previousSearchCriteria = '';
	// 记录上一次 effect 重跑时的索引刷新 nonce 与页码，用来区分"索引变更刷新"
	// 与"普通翻页"——只有 nonce 变化且查询与页码都没变才算刷新、才按路径保留选中。
	let previousRefreshNonce = 0;
	let previousPageIndex = 0;
	let pendingPageSelection: 'first' | 'last' = 'first';
	// 索引变更事件（dowse://index-changed）翻转这个 nonce，强制搜索 effect 用**相同**
	// 查询条件重跑一次，从而刷新结果、同时按路径保留当前选中文件——而不是靠重新
	// 键入触发（那会重置选中、丢掉正在阅读的位置）。
	let refreshNonce = $state(0);
	// 文件监听活跃提交时的"更新中…"指示：收到事件立即亮起，防抖合并刷新后熄灭。
	let indexWatchActive = $state(false);
	let indexChangedTimer: ReturnType<typeof setTimeout> | null = null;
	$effect(() => {
		const q = effectiveQuery;
		const group = extGroup;
		const sort = sortOption;
		const scope = scopeRoot;
		const mode = searchMode;
		const match = matchMode;
		const requestedPage = pageIndex;
		const token = ++searchToken;
		// 读一下 refreshNonce 把它登记成依赖：索引变更事件翻转它时，本 effect 会用
		// 相同查询条件重跑（见下方 wasRefresh 分支），刷新结果同时保留选中文件。
		const nonce = refreshNonce;
		void nonce;
		const criteria = `${q}\u0000${group}\u0000${sort}\u0000${scope}\u0000${mode}\u0000${match}`;
		const isCriteriaChange = criteria !== previousSearchCriteria;
		const isNonceChange = nonce !== previousRefreshNonce;
		const isPageChange = requestedPage !== previousPageIndex;
		// 只在"索引变更事件翻转 nonce"这一种情况才按路径保留选中文件；
		// 普通翻页（页码变化）或查询条件变化都不算刷新，必须走首/末行选中逻辑，
		// 否则翻页会被误判成刷新、丢掉用户想看的页首/页末行。
		const wasRefresh = isNonceChange && !isCriteriaChange && !isPageChange;
		// 记录本次 nonce / 页码，供下次 effect 重跑时区分"刷新"还是"翻页"。
		previousRefreshNonce = nonce;
		previousPageIndex = requestedPage;
		if (isCriteriaChange) {
			previousSearchCriteria = criteria;
			pendingPageSelection = 'first';
			if (requestedPage !== 0) {
				pageIndex = 0;
				return;
			}
		}
		// 击键到渲染性能埋点的起点：近似"触发搜索的输入事件"的时刻——
		// $effect 因 query/extGroup/sortOption 变化而重跑，跟用户敲键盘/
		// 切换筛选几乎同时发生。真正测的是下面 setTimeout 跑完之后（见
		// reportSearchPerf），中间的等待就是防抖窗口本身。
		const keystrokeAt = performance.now();
		if (q.trim().length === 0) {
			hits = [];
			totalHits = 0;
			selectedIndex = 0;
			lastSearchMs = null;
			searchPending = false;
			return;
		}
		const timer = setTimeout(async () => {
			// 计时窗口：从这里"发起请求"到下面结果赋值"上屏"，不含防抖等待——
			// 标题栏毫秒数是给用户看引擎有多快，不是给他们看输了多久的字。
			const startedAt = performance.now();
			searchPending = true;
			try {
				const results = await api.search(
					q,
					SEARCH_PAGE_SIZE,
					pageOffset(requestedPage),
					group,
					sort,
					scope,
					mode
				);
				if (token !== searchToken) return;
				// 索引变更重查时，先记下当前选中的文件路径，重查后尽量回到同一篇，
				// 保留用户的选中与阅读位置（文件被删/不再命中则夹紧下标）。
				const prevPath = wasRefresh ? selectedHit?.path ?? null : null;
				const resolvedPages = pageCount(results.total);
				const resolvedPage = clampPageIndex(requestedPage, resolvedPages);
				if (resolvedPage !== requestedPage) {
					pageIndex = resolvedPage;
					return;
				}
				hits = results.hits;
				totalHits = results.total;
				if (wasRefresh && prevPath) {
					const idx = results.hits.findIndex((h) => h.path === prevPath);
					selectedIndex = idx >= 0 ? idx : Math.min(selectedIndex, Math.max(0, results.hits.length - 1));
				} else {
					selectedIndex = pendingPageSelection === 'last' ? Math.max(0, results.hits.length - 1) : 0;
					pendingPageSelection = 'first';
				}
				lastSearchMs = performance.now() - startedAt;
				reportSearchPerf(keystrokeAt, startedAt, token);
			} catch (err) {
				if (token !== searchToken) return;
				hits = [];
				totalHits = 0;
				lastSearchMs = null;
				console.error('search failed', err);
			} finally {
				if (token === searchToken) searchPending = false;
			}
		}, SEARCH_DEBOUNCE_MS);
		return () => clearTimeout(timer);
	});

	function goToPage(nextPage: number, selection: 'first' | 'last' = 'first') {
		const next = clampPageIndex(nextPage, totalPages);
		if (next === pageIndex || searchPending) return;
		pendingPageSelection = selection;
		pageIndex = next;
	}

	/// 击键到渲染性能埋点：`hits` 赋值只是把新数据排进 Svelte 的响应式
	/// 更新队列，不代表 DOM 已经画出来——`await tick()` 等 Svelte 把这次
	/// 更新刷进 DOM，再等一帧 `requestAnimationFrame` 确认浏览器完成绘制，
	/// 这时才是"结果渲染完成"的真实时刻。`token` 复核放在渲染之后——如果
	/// 这次搜索已经被后续输入取代（`token !== searchToken`），说明用户又
	/// 敲了字，这次上报的数字已经没有意义，静默丢弃。任何一步失败（理论上
	/// 不会，但 invoke 本身可能抖动）都吞掉，不影响搜索主流程。
	async function reportSearchPerf(keystrokeAt: number, invokedAt: number, token: number) {
		await tick();
		requestAnimationFrame(() => {
			if (token !== searchToken) return;
			const renderedAt = performance.now();
			const e2eMs = renderedAt - keystrokeAt;
			const netMs = renderedAt - invokedAt;
			api.reportSearchPerf(e2eMs, netMs, SEARCH_DEBOUNCE_MS).catch(() => {
				// 失败安全：埋点不能影响搜索主流程。
			});
		});
	}

	// 选中行变了就换一份更长的预览上下文；轻微防抖避免按住方向键连续刷。
	// 预览自带的"全文阅读 / 文内查找"状态由 PreviewPane 内部管理：这里的
	// token 只负责"这个文件这一轮预览请求是否过期"，切换文件时旧请求的
	// 响应会被丢弃，不会覆盖新文件的预览。
	let previewToken = 0;
	$effect(() => {
		const hit = selectedHit;
		const q = query;
		if (!hit) {
			previewSegments = null;
			previewKind = null;
			previewLoading = false;
			return;
		}
		previewLoading = true;
		const token = ++previewToken;
		const timer = setTimeout(async () => {
			try {
				const result = await api.preview(hit.path, q);
				if (token !== previewToken) return;
				previewSegments = result?.segments ?? null;
				previewKind = result?.kind ?? null;
			} catch (err) {
				if (token !== previewToken) return;
				previewSegments = null;
				previewKind = null;
				console.error('preview failed', err);
			} finally {
				if (token === previewToken) previewLoading = false;
			}
		}, 40);
		return () => clearTimeout(timer);
	});

	function showToast(msg: string) {
		toast = msg;
		setTimeout(() => {
			if (toast === msg) toast = '';
		}, 2400);
	}

	/// 全量重建的共同实现：`pickDirectoryAndRebuild`（先弹目录选择器）和
	/// `rebuildCurrentIndex`（索引规则面板"立即重建"，目标目录已知）都走
	/// 这一份，避免"实时直播 + 冷报告"这套 UI 节奏在两个入口各写一遍、
	/// 后续改动漏改一处。
	async function rebuildWithDir(dir: string) {
		rebuildState = 'rebuilding';
		indexingPhase = 'text';
		indexingProcessed = 0;
		indexingCurrentFile = '';
		indexingReport = null;
		// 捕获本次操作的序号；定时器里据此判断"这期间有没有更新的操作插进来"，
		// 有就别复位（交给定版本去收尾），没有才把引导层收回。
		const myOp = ++indexingOpToken;
		try {
			const stats = await api.rebuildIndex(dir);
			hasIndex = true;
			refreshIndexStatus();
			// 文本阶段结束，交接给 OCR 阶段（如果还有图片没识别完）——不等
			// 第一个 `dowse://ocr-progress` 事件，直接用这次 invoke 返回的
			// 初始值起播，衔接文本直播 → 完成报告 → 图片余量递减的完整时间线。
			applyOcrPending(stats.ocr_pending);
			// 冷报告替换掉实时计数，停留片刻后收回整个引导层——用户看得见
			// "完成了"，不需要再点一下才能回到搜索态。
			indexingReport = {
				indexed: stats.indexed,
				seconds: stats.seconds,
				skippedOversize: stats.skipped_oversize
			};
			setTimeout(() => {
				// 用序号判活，不靠对象引用：此间若没有更新的操作（myOp 仍是最新），
				// 才把重建态 / 文本阶段 / 冷报告一并收掉，回到可搜索态。
				if (myOp !== indexingOpToken) return;
				rebuildState = 'idle';
				indexingPhase = 'idle';
				indexingReport = null;
			}, 1800);
		} catch (err) {
			rebuildState = 'error';
			rebuildError = String(err);
			indexingPhase = 'idle';
		}
	}

	async function pickDirectoryAndRebuild() {
		const dir = await open({ directory: true, multiple: false, title: t.dialogPickIndexFolder });
		if (!dir || Array.isArray(dir)) return;
		await rebuildWithDir(dir);
	}

	/// 索引规则面板"立即重建"：只在恰好一个索引根时可用（面板自己按
	/// `roots.length === 1` 禁用了按钮，这里的判断是防御性的，防止将来
	/// 谁绕过面板直接调这个函数）。`rebuild_index` 命令用单个目标目录整体
	/// 替换索引，多根场景下这么做会把其它根一并冲掉——多根索引目前没有
	/// 暴露"重建全部根"的前端命令，只有托盘每根子菜单的单根重建，所以
	/// 多根时不做任何事，交给面板上的提示文案引导用户去托盘操作。
	async function rebuildCurrentIndex() {
		if (roots.length !== 1) return;
		await rebuildWithDir(roots[0]);
	}

	function openSettingsPanel() {
		typeMenuOpen = false;
		sortMenuOpen = false;
		shortcutOverlayOpen = false;
		settingsPanelOpen = true;
	}

	function closeSettingsPanel() {
		settingsPanelOpen = false;
		// 面板关闭后把焦点交还给搜索框——面板打开期间焦点在面板内
		// （见 SettingsPanel 组件顶部注释），关闭动作不该让用户还要再点
		// 一下才能继续打字，跟呼出浮窗时 focusAndSelectAll 是同一个诉求。
		inputEl?.focus();
	}

	/**
	 * Settings is a window-level action, not a search-input action. Keeping this
	 * handler above the focused control means Ctrl+, still works after the user
	 * clicks a result, the preview pane, the divider, or any other blank area.
	 * `code` also covers keyboard layouts/IMEs whose localized `key` is not `,`.
	 */
	function handleWindowKeydown(e: KeyboardEvent) {
		if (!e.ctrlKey || (e.key !== ',' && e.code !== 'Comma')) return;
		e.preventDefault();
		openSettingsPanel();
	}

	/// 空态"添加文件夹"链接：多根索引场景下追加一个根，不动现有内容——跟
	/// pickDirectoryAndRebuild 走的是同一套"实时直播 + 冷报告"UI 节奏
	/// （同一批 dowse://rebuild-progress 事件），唯一区别是调用的命令
	/// （add_root 而不是 rebuild_index）和完成后不整体替换 hasIndex/roots，
	/// 而是照常刷新（roots 会多出这一项）。
	async function pickDirectoryAndAddRoot() {
		const dir = await open({ directory: true, multiple: false, title: t.dialogAddFolder });
		if (!dir || Array.isArray(dir)) return;

		rebuildState = 'rebuilding';
		indexingPhase = 'text';
		indexingProcessed = 0;
		indexingCurrentFile = '';
		indexingReport = null;
		const myOp = ++indexingOpToken;
		try {
			const stats = await api.addRoot(dir);
			refreshIndexStatus();
			applyOcrPending(stats.ocr_pending);
			indexingReport = {
				indexed: stats.indexed,
				seconds: stats.seconds,
				skippedOversize: stats.skipped_oversize
			};
			setTimeout(() => {
				if (myOp !== indexingOpToken) return;
				rebuildState = 'idle';
				indexingPhase = 'idle';
				indexingReport = null;
			}, 1800);
		} catch (err) {
			rebuildState = 'error';
			rebuildError = String(err);
			indexingPhase = 'idle';
		}
	}

	/// 记一次搜索历史：只在用户明确要"打开"某条结果的时刻调用（openSelected/
	/// revealSelected/showContextMenu），不在击键或结果刷新时调用——那会把
	/// 历史刷满中间态。当前查询词为空时不记（没有词可记）。
	function recordSearchHistory() {
		if (query.trim().length === 0) return;
		history = recordHistory(query);
	}

	function openSelected() {
		if (!selectedHit) return;
		recordSearchHistory();
		api.openFile(selectedHit.path).catch((err) => showToast(t.toastOpenFailed(String(err))));
	}

	function revealSelected() {
		if (!selectedHit) return;
		recordSearchHistory();
		api.revealInFolder(selectedHit.path).catch((err) => showToast(t.toastRevealFailed(String(err))));
	}

	function copySelectedPath() {
		if (!selectedHit) return;
		navigator.clipboard
			.writeText(selectedHit.path)
			.then(() => showToast(t.toastPathCopied))
			.catch(() => showToast(t.toastCopyFailed));
	}

	function togglePinned() {
		pinned = !pinned;
		api.setPinned(pinned).catch((err) => console.error('setPinned failed', err));
	}

	function showContextMenu(i: number) {
		selectedIndex = i;
		const hit = hits[i];
		if (!hit) return;
		// 右键菜单的具体动作（打开/定位/复制路径）是 Rust 侧原生处理的
		// （context_menu.rs::handle_context_menu_event），前端拿不到"选中了
		// 哪一项"的回调；退而在弹出菜单的这一刻记录，近似"用户对这条结果
		// 表达了打开/定位的意图"——唯一的误差是"仅复制路径"也会计入历史，
		// 可接受（历史本身就是去重+faint 展示，不是精确审计）。
		recordSearchHistory();
		api.showResultContextMenu(hit.path).catch((err) => console.error('showResultContextMenu failed', err));
	}

	/// 历史条目被选中（Enter 或鼠标点击）：填入查询词并让已有的搜索 $effect
	/// 接管防抖+发起搜索，这里不用重复实现一遍。
	function selectHistoryEntry(q: string) {
		query = q;
		inputEl?.focus();
	}

	function removeHistoryAt(index: number) {
		const target = history[index];
		if (target === undefined) return;
		history = removeHistoryEntry(target);
	}

	function clearAllHistory() {
		history = clearHistory();
	}

	function closeMenus() {
		typeMenuOpen = false;
		sortMenuOpen = false;
		scopeMenuOpen = false;
		modeMenuOpen = false;
		matchMenuOpen = false;
	}

	function openTypeMenu() {
		closeMenus();
		typeMenuOpen = !typeMenuOpen;
		typeMenuIndex = Math.max(
			0,
			TYPE_OPTIONS.findIndex((o) => o.value === extGroup)
		);
	}

	function openSortMenu() {
		closeMenus();
		sortMenuOpen = !sortMenuOpen;
		sortMenuIndex = Math.max(
			0,
			SORT_OPTIONS.findIndex((o) => o.value === sortOption)
		);
	}

	function openScopeMenu() {
		closeMenus();
		scopeMenuOpen = !scopeMenuOpen;
		scopeMenuIndex = Math.max(
			0,
			SCOPE_OPTIONS.findIndex((o) => o.value === (scopeRoot ?? 'all'))
		);
	}

	function openModeMenu() {
		closeMenus();
		modeMenuOpen = !modeMenuOpen;
		modeMenuIndex = Math.max(
			0,
			MODE_OPTIONS.findIndex((o) => o.value === searchMode)
		);
	}

	function openMatchMenu() {
		closeMenus();
		matchMenuOpen = !matchMenuOpen;
		matchMenuIndex = Math.max(
			0,
			MATCH_OPTIONS.findIndex((o) => o.value === matchMode)
		);
	}

	function handleKeydown(e: KeyboardEvent) {
		// 速查浮层打开期间，主输入完全不响应——任意键都只用来关掉浮层，
		// 不会漏进搜索框变成一个字符，也不会触发下面任何快捷键分支。
		if (shortcutOverlayOpen) {
			e.preventDefault();
			shortcutOverlayOpen = false;
			return;
		}
		// 设置面板打开期间的保险丝：正常情况下 DOM 焦点在面板内、本处理器
		// （绑在 .search-input 上）根本不会触发。但焦点转移可能落空——比如
		// 字段还在 loading disabled 态、或未来某个改动打断了聚焦时机——那时
		// 绝不能让按键漏进面板背后的搜索 UI：Esc 只关面板（不是藏整个窗口），
		// 其余按键一律吞掉。改键捕获态的 Esc 由面板自己（stopPropagation）先
		// 行拦截，走不到这里，所以这里的 Esc 只会是"关面板"语义。
		if (settingsPanelOpen) {
			e.preventDefault();
			if (e.key === 'Escape') {
				closeSettingsPanel();
			}
			return;
		}
		if (e.ctrlKey && e.key === '/') {
			e.preventDefault();
			closeMenus();
			shortcutOverlayOpen = true;
			return;
		}

		// Ctrl+P / Ctrl+S 开关类型/排序菜单，Ctrl+D 切换图钉——都不进底部快捷键
		// 提示条（保持底部简洁），只在这份速查浮层里能查到。两个下拉菜单互斥：
		// 开一个就关另一个。
		if (e.ctrlKey && e.key.toLowerCase() === 'p') {
			e.preventDefault();
			openTypeMenu();
			return;
		}
		if (e.ctrlKey && e.key.toLowerCase() === 's') {
			e.preventDefault();
			openSortMenu();
			return;
		}
		if (e.ctrlKey && e.key.toLowerCase() === 'd') {
			e.preventDefault();
			togglePinned();
			return;
		}
		// 菜单打开时，↑↓/Enter/Esc 转去控制菜单本身，不透传给下面的结果列表
		// 导航——避免同一次按键既翻结果又翻菜单项。
		if (typeMenuOpen || sortMenuOpen) {
			const options = typeMenuOpen ? TYPE_OPTIONS : SORT_OPTIONS;
			let index = typeMenuOpen ? typeMenuIndex : sortMenuIndex;
			if (e.key === 'ArrowDown') {
				e.preventDefault();
				index = Math.min(index + 1, options.length - 1);
			} else if (e.key === 'ArrowUp') {
				e.preventDefault();
				index = Math.max(index - 1, 0);
			} else if (e.key === 'Enter') {
				e.preventDefault();
				const picked = options[index].value;
				if (typeMenuOpen) extGroup = picked as ExtGroup;
				else sortOption = picked as SortOption;
				closeMenus();
				return;
			} else if (e.key === 'Escape') {
				e.preventDefault();
				closeMenus();
				return;
			} else {
				return;
			}
			if (typeMenuOpen) typeMenuIndex = index;
			else sortMenuIndex = index;
			return;
		}

		// 历史条目导航：只在输入框为空且有历史时生效（historyMode）。查询非空
		// 时 hits 才可能非空，两套 ↑↓/Enter 天然互斥，不会抢键。Delete 删除
		// 选中条目——不影响 Backspace，输入框这时本来就是空的，Backspace
		// 依旧是正常的编辑行为。
		if (historyMode) {
			if (e.key === 'ArrowDown') {
				e.preventDefault();
				historyIndex = Math.min(historyIndex + 1, history.length - 1);
				return;
			}
			if (e.key === 'ArrowUp') {
				e.preventDefault();
				historyIndex = Math.max(historyIndex - 1, 0);
				return;
			}
			if (e.key === 'Enter') {
				e.preventDefault();
				selectHistoryEntry(history[historyIndex]);
				return;
			}
			if (e.key === 'Delete') {
				e.preventDefault();
				removeHistoryAt(historyIndex);
				return;
			}
		}

		if (e.key === 'Escape') {
			e.preventDefault();
			api.hideWindow().catch((err) => console.error('hideWindow failed', err));
			return;
		}
		if (e.key === 'ArrowDown') {
			e.preventDefault();
			if (hits.length > 0 && selectedIndex === hits.length - 1 && pageIndex + 1 < totalPages) {
				goToPage(pageIndex + 1);
			} else if (hits.length > 0) {
				selectedIndex = Math.min(selectedIndex + 1, hits.length - 1);
			}
			return;
		}
		if (e.key === 'ArrowUp') {
			e.preventDefault();
			if (hits.length > 0 && selectedIndex === 0 && pageIndex > 0) {
				goToPage(pageIndex - 1, 'last');
			} else if (hits.length > 0) {
				selectedIndex = Math.max(selectedIndex - 1, 0);
			}
			return;
		}
		if (e.key === 'Enter') {
			e.preventDefault();
			if (e.ctrlKey) revealSelected();
			else openSelected();
			return;
		}
		if (e.key.toLowerCase() === 'c' && e.ctrlKey) {
			// 输入框里有正在编辑的文本选区时，让浏览器按正常复制处理，
			// 不要抢用户的编辑操作。
			const hasTextSelection =
				inputEl && inputEl.selectionStart !== null && inputEl.selectionStart !== inputEl.selectionEnd;
			if (!hasTextSelection && selectedHit) {
				e.preventDefault();
				copySelectedPath();
			}
		}
	}

	function focusAndSelectAll() {
		inputEl?.focus();
		inputEl?.select();
	}

	// 面板可视不透明度收拢到这一个入口：两个数字（明/暗主题各一个 alpha）
	// 直接写进 CSS 变量，具体哪个生效由 app.css 的 prefers-color-scheme
	// 媒体查询决定，这里不用猜当前是明是暗。托盘切透明度档位时走
	// dowse://glass-alpha 事件复用同一个函数。
	function applyGlassAlpha(alpha: GlassAlpha) {
		document.documentElement.style.setProperty('--glass-alpha-light', String(alpha.light));
		document.documentElement.style.setProperty('--glass-alpha-dark', String(alpha.dark));
	}

	// 呼出的手感：轻微放大 + 淡入，全程压在 120ms 以内的弹簧物理，不是缓动曲线。
	// 用显式 keyframe（而不是读当前样式）保证每次呼出都从同一个起点播，
	// 不会因为上一次动画没播完就被打断而出现错位。
	function playShowAnimation() {
		if (!panelEl) return;
		animate(
			panelEl,
			{ opacity: [0, 1], scale: [0.98, 1] },
			{ type: 'spring', bounce: 0.2, duration: 0.12 }
		);
		playCaretFlourish();
	}

	// 呼出瞬间的光标手感：一根装饰性的竖条从 0 高度弹到全高，跟输入框呼出
	// 动画同一时刻起播。呼出时上次查询词会被全选（focusAndSelectAll），
	// 原生光标本来就被选区盖住看不见，这根竖条负责传达"已经就绪、可以打字
	// 了"的瞬时反馈；短暂停留后自己收回去，交回给原生光标，不留一根常驻的
	// 假光标在那里。
	function playCaretFlourish() {
		if (!caretFlourishEl) return;
		animate(
			caretFlourishEl,
			{ height: ['0px', '20px'] },
			{ type: 'spring', bounce: 0.15, duration: 0.1 }
		).then(() => {
			if (!caretFlourishEl) return;
			animate(caretFlourishEl, { height: '0px' }, { duration: 0.08, ease: 'easeIn', delay: 0.1 });
		});
	}

	/// 呼出延迟性能埋点：`dowse://shown` 事件到这里只说明 Rust 侧调用过
	/// `window.show()`，不代表前端这一帧已经真正绘制上屏——用双重
	/// requestAnimationFrame 确认至少完成一次绘制之后再回报给 Rust 侧，
	/// Rust 那边拿热键回调进入的单调时钟算差值打日志（见 perf.rs）。非
	/// 热键触发的显示（托盘点击等）Rust 侧没有起始时刻可算，命令内部会
	/// 静默跳过，这里不需要关心那个区分。
	function reportShownPerf() {
		requestAnimationFrame(() => {
			requestAnimationFrame(() => {
				api.reportShownPerf().catch(() => {
					// 失败安全：埋点不能影响呼出主流程。
				});
			});
		});
	}

	// 鼠标点了 .controls（两个下拉 + 图钉）以外的地方就收起菜单——两个下拉
	// 平时几乎不占视觉存在感，不能指望用户记得回来点一下 trigger 才能关掉。
	function handleDocumentClick(e: MouseEvent) {
		if (!typeMenuOpen && !sortMenuOpen) return;
		if (controlsEl && e.target instanceof Node && controlsEl.contains(e.target)) return;
		closeMenus();
	}

	// 无边框窗口没有系统标题栏。搜索行的空白处充当拖动区域，但输入框、按钮
	// 和菜单保留原本的点击行为，避免用户选文字或点筛选器时把窗口拖走。
	function handleWindowDrag(e: MouseEvent) {
		if (e.button !== 0 || e.detail > 1) return;
		const target = e.target;
		if (!(target instanceof Element)) return;
		if (target.closest('input, button, [role="button"], [role="menu"], [data-no-window-drag]')) return;
		getCurrentWindow()
			.startDragging()
			.catch((err) => console.error('startDragging failed', err));
	}

	type ResizeDirection =
		| 'North'
		| 'NorthEast'
		| 'East'
		| 'SouthEast'
		| 'South'
		| 'SouthWest'
		| 'West'
		| 'NorthWest';

	// 无边框窗口也没有系统边框命中区。四边和四角各放一个透明热区，交给
	// Tauri 启动原生窗口缩放；这样鼠标离开 WebView 后仍由 Windows 接管拖动。
	function handleWindowResize(e: MouseEvent, direction: ResizeDirection) {
		if (e.button !== 0) return;
		e.preventDefault();
		e.stopPropagation();
		getCurrentWindow()
			.startResizeDragging(direction)
			.catch((err) => console.error('startResizeDragging failed', err));
	}

	function syncResultsWidth() {
		if (!bodyEl) return;
		resultsWidth = splitWidthFromRatio(bodyEl.clientWidth, splitRatio);
	}

	function updateSplitFromPointer(clientX: number) {
		if (!bodyEl) return;
		const rect = bodyEl.getBoundingClientRect();
		const desired = clientX - rect.left - SPLIT_DIVIDER_WIDTH / 2;
		resultsWidth = clampSplitWidth(rect.width, desired);
		splitRatio = splitRatioFromWidth(rect.width, resultsWidth);
	}

	function persistSplitRatio() {
		try {
			localStorage.setItem(SPLIT_RATIO_KEY, String(splitRatio));
		} catch {
			// localStorage 不可用时只保留本次会话宽度，拖动本身仍然可用。
		}
	}

	function handleDividerPointerDown(e: PointerEvent) {
		if (e.button !== 0) return;
		e.preventDefault();
		const divider = e.currentTarget as HTMLInputElement;
		divider.setPointerCapture(e.pointerId);
		splitResizing = true;
		document.body.classList.add('split-resizing');
		updateSplitFromPointer(e.clientX);
	}

	function handleDividerPointerMove(e: PointerEvent) {
		if (!splitResizing) return;
		updateSplitFromPointer(e.clientX);
	}

	function finishDividerResize(e: PointerEvent) {
		if (!splitResizing) return;
		const divider = e.currentTarget as HTMLInputElement;
		if (divider.hasPointerCapture(e.pointerId)) divider.releasePointerCapture(e.pointerId);
		splitResizing = false;
		document.body.classList.remove('split-resizing');
		persistSplitRatio();
	}

	function handleDividerKeydown(e: KeyboardEvent) {
		if (!bodyEl) return;
		let nextWidth = resultsWidth;
		if (e.key === 'ArrowLeft') nextWidth -= 20;
		else if (e.key === 'ArrowRight') nextWidth += 20;
		else if (e.key === 'Home') nextWidth = 0;
		else if (e.key === 'End') nextWidth = bodyEl.clientWidth;
		else return;

		e.preventDefault();
		resultsWidth = clampSplitWidth(bodyEl.clientWidth, nextWidth);
		splitRatio = splitRatioFromWidth(bodyEl.clientWidth, resultsWidth);
		persistSplitRatio();
	}

	onMount(() => {
		try {
			splitRatio = normalizeSplitRatio(localStorage.getItem(SPLIT_RATIO_KEY));
		} catch {
			splitRatio = DEFAULT_SPLIT_RATIO;
		}
		syncResultsWidth();
		const splitResizeObserver = new ResizeObserver(syncResultsWidth);
		if (bodyEl) splitResizeObserver.observe(bodyEl);

		history = loadHistory();
		refreshIndexStatus();
		// 窗口挂载时也拉一次进度快照——覆盖"托盘触发的建索引正在跑，浮窗这时
		// 才第一次打开"的场景，不用等下一条事件才能看到活的进度。
		refreshIndexingStatus();
		focusAndSelectAll();

		api.getEffectLevel().then((level: EffectLevel) => {
			document.documentElement.dataset.effect = level;
		});
		api.getGlassAlpha().then(applyGlassAlpha);
		api.getHotkey().then((raw) => {
			hotkeyLabel = formatHotkey(raw);
		});
		// 语言启动镜像自愈：config.lang 是权威（Rust 托盘 i18n 也读它），把它
		// 同步进 localStorage，供下次启动时 i18n.ts 同步读取决定界面语言（见
		// i18n.ts 顶部说明）。即使有人手改了 config.json，这一步也能在下一次
		// 启动前把镜像纠正过来——本轮语言本就"重启后生效"，一拍延迟可接受。
		api
			.getConfig()
			.then((cfg) => {
				try {
					localStorage.setItem(LANG_OVERRIDE_KEY, cfg.lang);
				} catch {
					// localStorage 不可用就算了，i18n.ts 会回落系统语言。
				}
			})
			.catch(() => {});

		document.addEventListener('click', handleDocumentClick);

		const unlistenShown = listen('dowse://shown', () => {
			refreshIndexStatus();
			// 窗口重新唤出时必须能看到活的进度，不能是呼出前那一刻的旧快照，
			// 更不能是空白——建索引期间反复隐藏/唤出窗口是这套进度视图的
			// 核心验收场景（症状 2/3）。
			refreshIndexingStatus();
			focusAndSelectAll();
			playShowAnimation();
			closeMenus();
			reportShownPerf();
		});
		const unlistenOpenSettings = listen('dowse://open-settings', openSettingsPanel);
		const unlistenEffect = listen<EffectLevel>('dowse://effect-level', (evt) => {
			document.documentElement.dataset.effect = evt.payload;
		});
		const unlistenGlassAlpha = listen<GlassAlpha>('dowse://glass-alpha', (evt) => {
			applyGlassAlpha(evt.payload);
		});
		const unlistenRebuildDone = listen<number>('dowse://rebuild-done', (evt) => {
			refreshIndexStatus();
			// 托盘触发的重建（"重建索引"/"更改索引文件夹…"）没有走本地的
			// pickDirectoryAndRebuild，指望这里补一次快照拉取，好在窗口这时
			// 恰好开着的话立刻接上 OCR 阶段的进度条，不用等下一条事件。
			refreshIndexingStatus();
			showToast(t.toastRebuildDone(evt.payload));
		});
		const unlistenRebuildError = listen<string>('dowse://rebuild-error', (evt) => {
			refreshIndexingStatus();
			showToast(t.toastRebuildFailed(evt.payload));
		});
		// 托盘"移除"文件夹的成功回执——移除没有"收录数"可言，走独立事件而不是
		// 复用 dowse://rebuild-done（那个的 toast 文案是"收录 N 个文件"，套用
		// 到移除操作上语义是反的）。
		const unlistenRootRemoved = listen<number>('dowse://root-removed', (evt) => {
			refreshIndexStatus();
			showToast(t.toastFolderRemoved(evt.payload));
		});
		// 建索引"实时直播"：全程监听，不只在 rebuildState === 'rebuilding' 时才挂——
		// 事件只在 rebuild_index 命令执行期间才会发出，不重建时这个监听器闲置无害。
		const unlistenRebuildProgress = listen<IndexProgress>('dowse://rebuild-progress', (evt) => {
			applyIndexingEvent({ type: 'text-progress', progress: evt.payload });
		});
		// 终态跟 progress 走同一条 Tauri 事件通道，必定排在本轮最后一条进度之后。
		// 这层兜底覆盖 invoke 已返回、迟到 progress 又把界面写回 text 的竞争，
		// 避免完成报告永久遮住后台已经拿到的搜索结果（Issue #28）。
		const unlistenIndexingSettled = listen<IndexingSnapshot>(
			'dowse://indexing-settled',
			(evt) => applyIndexingEvent({ type: 'snapshot', snapshot: evt.payload })
		);
		// OCR 队列消化进度：payload 是这一刻队列里还剩多少张待处理——
		// 症状 3 的修复核心，v0.6.1 之前这行字是重建完成那一刻的静态快照，
		// 从不刷新；现在跟着 OCR worker 每个 flush 批次持续推送、持续下降。
		const unlistenOcrProgress = listen<number>('dowse://ocr-progress', (evt) => {
			applyOcrPending(evt.payload);
		});
		// 文件监听增量提交：每次 commit 推一个事件，这里合并高频事件后刷新结果 +
		// 索引统计（文档数/占用）。见 watcher.rs 的 Committed 分支。
		const unlistenIndexChanged = listen('dowse://index-changed', () => {
			// 先亮"更新中"，再用 600ms 静默窗口把连续提交合并成一次刷新——
			// 编辑文件时的连续 commit 不会把界面刷到抖；也不做每秒全盘扫描，
			// 只重跑当前查询（索引 reader 已自动重载），由 searchToken 挡掉旧响应。
			indexWatchActive = true;
			if (indexChangedTimer !== null) clearTimeout(indexChangedTimer);
			indexChangedTimer = setTimeout(() => {
				indexChangedTimer = null;
				indexWatchActive = false;
				refreshIndexStatus();
				// 有查询词才重跑结果（保留选中文件由搜索 effect 的 wasRefresh 分支处理）；
				// 空查询只刷新索引统计即可，空态会随文档数变化。
				if (effectiveQuery.trim().length > 0) refreshNonce++;
			}, 600);
		});

		return () => {
			splitResizeObserver.disconnect();
			document.body.classList.remove('split-resizing');
			document.removeEventListener('click', handleDocumentClick);
			if (indexChangedTimer !== null) clearTimeout(indexChangedTimer);
			unlistenShown.then((f) => f());
			unlistenOpenSettings.then((f) => f());
			unlistenEffect.then((f) => f());
			unlistenGlassAlpha.then((f) => f());
			unlistenRebuildDone.then((f) => f());
			unlistenRebuildError.then((f) => f());
			unlistenRootRemoved.then((f) => f());
			unlistenRebuildProgress.then((f) => f());
			unlistenIndexingSettled.then((f) => f());
			unlistenOcrProgress.then((f) => f());
			unlistenIndexChanged.then((f) => f());
		};
	});
</script>

<svelte:window onkeydown={handleWindowKeydown} />

<div class="panel" bind:this={panelEl}>
	<!-- 自绘的八个方向缩放热区已移除：窗口改用标准 Windows 标题栏后，缩放手势
	     由系统边框提供。留着这些透明热区只会盖住内容边缘、拦截本该给内容的鼠标事件。 -->
	<!-- svelte-ignore a11y_no_static_element_interactions: the blank title area maps to the native OS window-drag gesture; interactive descendants are excluded in handleWindowDrag -->
	<div class="search-row" onmousedown={handleWindowDrag}>
		<svg class="search-icon" width="18" height="18" viewBox="0 0 18 18" fill="none" aria-hidden="true">
			<circle cx="8" cy="8" r="5.4" stroke="currentColor" stroke-width="1.4" />
			<path d="M12.2 12.2 16 16" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" />
		</svg>
		<div class="input-wrap">
			<input
				bind:this={inputEl}
				type="text"
				class="search-input"
				placeholder={t.searchPlaceholder}
				bind:value={query}
				onkeydown={handleKeydown}
				autocomplete="off"
				spellcheck="false"
			/>
			<span class="caret-flourish" bind:this={caretFlourishEl} aria-hidden="true"></span>
		</div>
		<div class="controls" bind:this={controlsEl}>
			<GhostDropdown
				idleLabel={t.filterIdleLabel}
				options={TYPE_OPTIONS}
				value={extGroup}
				defaultValue="all"
				open={typeMenuOpen}
				bind:activeIndex={typeMenuIndex}
				onselect={(v) => {
					extGroup = v as ExtGroup;
					closeMenus();
				}}
				ontoggle={openTypeMenu}
			/>
			<GhostDropdown
				idleLabel={t.sortIdleLabel}
				options={SORT_OPTIONS}
				value={sortOption}
				defaultValue="relevance"
				open={sortMenuOpen}
				bind:activeIndex={sortMenuIndex}
				onselect={(v) => {
					sortOption = v as SortOption;
					closeMenus();
				}}
				ontoggle={openSortMenu}
			/>
			<PinButton {pinned} onclick={togglePinned} />
			<SettingsButton onclick={openSettingsPanel} />
		</div>
	</div>

	<!-- 第二行：范围 / 模式 / 匹配。刻意跟关键词输入框分行——挤在一行会让每个
	     下拉都窄到只能显示图标，用户得逐个点开才知道当前选的是什么。 -->
	<div class="filter-row">
		<span class="flabel">{t.scopeLabel}</span>
		<GhostDropdown
			idleLabel={t.scopeAll}
			options={SCOPE_OPTIONS}
			value={scopeRoot ?? 'all'}
			defaultValue="all"
			menuAlign="left"
			open={scopeMenuOpen}
			bind:activeIndex={scopeMenuIndex}
			onselect={(v) => {
				scopeRoot = v === 'all' ? null : v;
				closeMenus();
			}}
			ontoggle={openScopeMenu}
		/>
		<span class="flabel">{t.modeLabel}</span>
		<GhostDropdown
			idleLabel={t.modeContent}
			options={MODE_OPTIONS}
			value={searchMode}
			defaultValue="content"
			menuAlign="left"
			open={modeMenuOpen}
			bind:activeIndex={modeMenuIndex}
			onselect={(v) => {
				searchMode = v as SearchField;
				closeMenus();
			}}
			ontoggle={openModeMenu}
		/>
		<span class="flabel">{t.matchLabel}</span>
		<GhostDropdown
			idleLabel={t.matchNormal}
			options={MATCH_OPTIONS}
			value={matchMode}
			defaultValue="normal"
			menuAlign="left"
			open={matchMenuOpen}
			bind:activeIndex={matchMenuIndex}
			onselect={(v) => {
				matchMode = v as 'normal' | 'phrase';
				closeMenus();
			}}
			ontoggle={openMatchMenu}
		/>
		<span class="fspacer"></span>
		<!-- 索引管理入口：文件夹、排除规则、体积上限这些原来只在托盘菜单里，
		     用户遇到"这个文件怎么没被收录"时要先猜该去哪找。放主界面常驻。 -->
		<button type="button" class="fbtn" onclick={openSettingsPanel}>{t.indexManage}</button>
	</div>

	<div
		class="body"
		class:has-results={!showGuidance}
		bind:this={bodyEl}
		style={`--results-width: ${resultsWidth > 0 ? `${resultsWidth}px` : '58%'}`}
	>
		{#if showGuidance}
			<EmptyState
				kind={guidanceKind}
				{query}
				{numDocs}
				errorMessage={rebuildError}
				{indexingProcessed}
				{indexingCurrentFile}
				{indexingReport}
				{roots}
				{history}
				{historyIndex}
				onpick={pickDirectoryAndRebuild}
				onaddfolder={pickDirectoryAndAddRoot}
				onselecthistory={selectHistoryEntry}
				onhoverhistory={(i) => (historyIndex = i)}
				onclearhistory={clearAllHistory}
			/>
		{:else}
			<div class="results">
				<div class="results-heading">
					<span class="results-meta">
						<span>{t.resultsPrefix}<AnimatedNumber value={totalHits} />{t.resultsSuffix}</span>
						{#if lastSearchMs !== null}
							<span class="search-ms">· {Math.round(lastSearchMs)}ms</span>
						{/if}
					</span>
					{#if totalPages > 1}
						<nav class="pagination" class:pending={searchPending} aria-label={t.pageStatus(pageIndex + 1, totalPages)}>
							<button
								type="button"
								aria-label={t.previousPage}
								title={t.previousPage}
								disabled={pageIndex === 0 || searchPending}
								onclick={() => goToPage(pageIndex - 1)}
							>
								<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m10 3-5 5 5 5" /></svg>
							</button>
							<span class="page-number" aria-live="polite">{pageIndex + 1} / {totalPages}</span>
							<button
								type="button"
								aria-label={t.nextPage}
								title={t.nextPage}
								disabled={pageIndex + 1 >= totalPages || searchPending}
								onclick={() => goToPage(pageIndex + 1)}
							>
								<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m6 3 5 5-5 5" /></svg>
							</button>
						</nav>
					{/if}
				</div>
				<ResultList
					{hits}
					{selectedIndex}
					sort={sortOption}
					onsort={(next) => (sortOption = next)}
					onhover={(i) => (selectedIndex = i)}
					onselect={(i) => {
						selectedIndex = i;
					}}
					onopen={(i) => {
						selectedIndex = i;
						openSelected();
					}}
					oncontextmenu={showContextMenu}
				/>
			</div>
			<input
				type="range"
				class="divider-v"
				class:resizing={splitResizing}
				aria-label={t.resizePreviewPane}
				aria-orientation="vertical"
				min="0"
				max="100"
				value={Math.round(splitRatio * 100)}
				onpointerdown={handleDividerPointerDown}
				onpointermove={handleDividerPointerMove}
				onpointerup={finishDividerResize}
				onpointercancel={finishDividerResize}
				onkeydown={handleDividerKeydown}
			/>
			<div class="preview-col">
				<PreviewPane
					hit={selectedHit}
					segments={previewSegments}
					loading={previewLoading}
					kind={previewKind}
				/>
			</div>
		{/if}
	</div>

	<!-- OCR 回填阶段的常驻进度条：跟 showGuidance 的分支是平级关系，不遮挡
	     搜索结果——文本已经 commit 的内容立刻可搜，图片在后台慢慢识别，
	     这条状态是独立叠加在结果视图之上的，不是替换（症状 4 的验收场景：
	     建索引期间反复搜索必须可用）。 -->
	{#if indexingPhase === 'ocr'}
		<IndexingStrip processed={indexingOcrProcessed} total={indexingOcrTotal} />
	{/if}

	<!-- 状态栏：命中数 / 搜索耗时 / 已索引 / 更新状态 / 索引占用。这些原来散在
	     结果标题和托盘里，用户要"我的索引多大、有没有在更新"得去翻托盘——现在
	     常驻底部一行，一眼能看完。 -->
	<div class="statusbar">
		<span class="sitem">{t.statusHits} <b>{totalHits}</b></span>
		<span class="ssep">·</span>
		<span class="sitem">{lastSearchMs === null ? '—' : `${Math.round(lastSearchMs)} ms`}</span>
		<span class="ssep">·</span>
		<span class="sitem">{t.statusIndexed} <b>{numDocs}</b></span>
		{#if indexingPhase !== 'idle' || rebuildState === 'rebuilding' || indexWatchActive}
			<span class="ssep">·</span>
			<span class="sitem live">更新中…</span>
		{/if}
		<span class="sspacer"></span>
		<span class="sitem">{t.statusIndexSize} <b>{formatBytes(indexUsage?.bytes ?? 0)}</b></span>
	</div>

	<ShortcutBar hasSelection={selectedHit !== null} />

	{#if toast}
		<div class="toast">{toast}</div>
	{/if}

	{#if shortcutOverlayOpen}
		<ShortcutOverlay hotkey={hotkeyLabel} onclose={() => (shortcutOverlayOpen = false)} />
	{/if}

	{#if settingsPanelOpen}
		<SettingsPanel
			{roots}
			onclose={closeSettingsPanel}
			onrebuild={() => {
				closeSettingsPanel();
				rebuildCurrentIndex();
			}}
		/>
	{/if}
</div>

<style>
	/* v0.4.1 曾经让 .panel 内缩 16px（inset: var(--panel-margin)）给
	   box-shadow 留渲染空间——结论错了，撤销，别再试：DWM 的 Acrylic/Mica
	   是整个窗口生效的合成效果，不认 CSS 布局留出来的"空白"，缩出来的这一
	   圈边距照样被渲染成玻璃，视觉上就成了"外面一圈裸玻璃画框、里面一个
	   .panel 边框"的双框。整窗玻璃和"用内缩+CSS阴影模拟悬浮"这个方案在
	   物理上不兼容，不是哪个数值没调对，任何再往这个方向调 inset 数值的
	   尝试都会复现同一个问题。
	   悬浮感的代价就此放弃——.panel 满铺整个窗口（inset: 0），只留一圈
	   1px 半透明描边勾出边界，不再画阴影。position: absolute 以窗口
	   （初始包含块）为参照，而不是随便找一个祖先元素——html/body 都没有
	   设 position，天然就是这个参照系。 */
	.panel {
		position: absolute;
		inset: 0;
		display: flex;
		flex-direction: column;
		background: var(--glass-tint);
		border-radius: var(--radius-window);
		border: 1px solid var(--panel-border);
		overflow: hidden;
	}

	/* 缩放热区的 CSS 已随自绘手柄一起删掉：改用标准窗口（decorations: true）后，
	   缩放/最小化/最大化/关闭全交给 Windows 原生边框，这里再留 6px 边角热区
	   只会挡住原生边框自己的热区，反而拖不动。 */

	/* 输入区刻意不做"框"——没有边框、没有底色块，大字号裸排；下面这条
	   1px 低对比发丝线才是分隔输入区和结果区的唯一视觉边界，Raycast 同款。 */
	.search-row {
		display: flex;
		align-items: center;
		gap: 10px;
		padding: 15px 24px;
		border-bottom: 1px solid var(--divider);
		flex-shrink: 0;
	}

	.search-icon {
		color: var(--fg-secondary);
		flex-shrink: 0;
	}

	.input-wrap {
		position: relative;
		flex: 1;
		display: flex;
		align-items: center;
	}

	.search-input {
		flex: 1;
		border: none;
		outline: none;
		background: transparent;
		font-size: 18px;
		font-weight: 400;
		caret-color: var(--accent-caret);
	}

	/* 类型筛选 / 排序器 / 图钉——三个都是默认态"几乎不存在"的幽灵控件，
	   紧贴输入区右侧，跟输入框之间留一点呼吸但不占视觉重量。 */
	.controls {
		display: flex;
		align-items: center;
		gap: 2px;
		flex-shrink: 0;
	}

	/* 第二行筛选栏：范围 / 模式 / 匹配。左边留一点内缩，视觉上从属于上面的
	   搜索行；整行不高，不给结果区挤太多高度。 */
	.filter-row {
		display: flex;
		align-items: center;
		gap: 6px;
		flex-shrink: 0;
		padding: 0 14px 6px;
	}

	.flabel {
		font-size: 11px;
		color: var(--fg-tertiary);
		user-select: none;
	}

	.fspacer {
		flex: 1;
	}

	/* 索引管理按钮：跟筛选下拉同一行，但压得更淡，避免跟"筛选条件"抢视觉重心。 */
	.fbtn {
		padding: 2px 8px;
		border: 1px solid var(--border-subtle, rgba(127, 127, 127, 0.22));
		border-radius: 5px;
		background: transparent;
		font: inherit;
		font-size: 11px;
		color: var(--fg-secondary);
		cursor: pointer;
	}

	.fbtn:hover {
		background: var(--row-hover);
		color: var(--fg-primary);
	}

	/* 底部状态栏：命中数 / 耗时 / 已索引 / 更新状态 …… 索引占用 */
	.statusbar {
		display: flex;
		align-items: center;
		gap: 6px;
		flex-shrink: 0;
		padding: 3px 14px;
		font-size: 11px;
		color: var(--fg-tertiary);
		border-top: 1px solid var(--border-subtle, rgba(127, 127, 127, 0.14));
	}

	.statusbar b {
		font-weight: 600;
		color: var(--fg-secondary);
	}

	.ssep {
		opacity: 0.45;
	}

	.sspacer {
		flex: 1;
	}

	/* 正在建索引/增量更新时，"更新中…"跟别的状态项区分开——这是唯一会自己
	   变化的字段，值得一点颜色。 */
	.statusbar .live {
		color: var(--accent-caret, var(--fg-secondary));
		font-weight: 500;
	}

	.search-input::placeholder {
		color: var(--fg-placeholder);
	}

	/* 装饰性光标：呼出瞬间从 0 高度弹到全高的那一下手感，见 playCaretFlourish。
	   平时高度是 0（不可见），不跟原生光标打架。 */
	.caret-flourish {
		position: absolute;
		left: 0;
		top: 50%;
		width: 2px;
		height: 0px;
		transform: translateY(-50%);
		background: var(--accent-caret);
		border-radius: 1px;
		pointer-events: none;
	}

	.body {
		flex: 1;
		display: flex;
		min-height: 0;
	}

	.body.has-results {
		display: grid;
		grid-template-columns: var(--results-width) 9px minmax(240px, 1fr);
	}

	.results {
		min-width: 280px;
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}

	/* 区分小标题：只在真的有结果时出现（showGuidance 为假意味着 hits 非空，
	   见 +page.svelte 顶部的 guidanceKind 推导），空态/建索引态走 EmptyState
	   分支，不会渲染到这里。 */
	.results-heading {
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 8px;
		padding: 12px 16px 8px;
		font-size: 11px;
		letter-spacing: 0.04em;
		color: var(--fg-tertiary);
	}

	.results-meta {
		display: inline-flex;
		align-items: baseline;
		gap: 5px;
		min-width: 0;
		white-space: nowrap;
	}

	/* 标题栏毫秒数：不解释、不加图标，比小标题本身再淡一档，等宽数字继承
	   全局 body 的 tabular-nums。刻意不做滚动动画（AnimatedNumber）——
	   每次搜索都变，滚一下反而晃眼，见 +page.svelte 顶部 lastSearchMs 的注释。 */
	.search-ms {
		flex-shrink: 0;
		opacity: 0.7;
		letter-spacing: 0;
	}

	.pagination {
		display: inline-flex;
		align-items: center;
		gap: 1px;
		height: 24px;
		margin: -4px -5px -4px auto;
		letter-spacing: 0;
		transition: opacity 90ms ease-out;
	}

	.pagination.pending {
		opacity: 0.55;
	}

	.pagination button {
		display: grid;
		place-items: center;
		width: 26px;
		height: 26px;
		padding: 0;
		border: 0;
		border-radius: var(--radius-chip);
		background: transparent;
		color: var(--fg-primary);
		cursor: default;
		transition:
			background 90ms ease-out,
			color 90ms ease-out,
			opacity 90ms ease-out;
	}

	.pagination button:hover:not(:disabled) {
		background: var(--row-hover);
		color: var(--fg-primary);
	}

	.pagination button:focus-visible {
		outline: 1px solid var(--accent-border);
		outline-offset: -1px;
	}

	.pagination button:disabled {
		opacity: 0.24;
	}

	.pagination svg {
		width: 12px;
		height: 12px;
		fill: none;
		stroke: currentColor;
		stroke-width: 1.5;
		stroke-linecap: round;
		stroke-linejoin: round;
	}

	.page-number {
		min-width: 42px;
		text-align: center;
		font-variant-numeric: tabular-nums;
		color: var(--fg-secondary);
	}

	.divider-v {
		appearance: none;
		position: relative;
		width: 9px;
		min-width: 9px;
		height: 100%;
		padding: 0;
		border: 0;
		background: linear-gradient(
			to right,
			transparent 4px,
			var(--divider) 4px,
			var(--divider) 5px,
			transparent 5px
		);
		cursor: col-resize;
		touch-action: none;
		outline: none;
	}

	.divider-v::-webkit-slider-thumb {
		appearance: none;
		width: 9px;
		height: 100%;
		background: transparent;
		cursor: col-resize;
	}

	.divider-v:hover,
	.divider-v:focus-visible,
	.divider-v.resizing {
		background: linear-gradient(
			to right,
			transparent 3px,
			color-mix(in srgb, var(--accent-strong) 18%, transparent) 3px,
			color-mix(in srgb, var(--accent-strong) 18%, transparent) 6px,
			transparent 6px
		);
	}

	:global(body.split-resizing),
	:global(body.split-resizing *) {
		cursor: col-resize !important;
		user-select: none !important;
	}

	.preview-col {
		flex: 1;
		min-width: 240px;
		overflow: hidden;
	}

	.toast {
		position: absolute;
		bottom: 48px;
		left: 50%;
		transform: translateX(-50%);
		background: var(--toast-bg);
		color: var(--toast-fg);
		font-size: 13px;
		padding: 8px 16px;
		border-radius: 8px;
		pointer-events: none;
		animation: toast-in 0.12s ease-out;
	}

	@keyframes toast-in {
		from {
			opacity: 0;
			transform: translate(-50%, 4px);
		}
		to {
			opacity: 1;
			transform: translate(-50%, 0);
		}
	}
</style>
