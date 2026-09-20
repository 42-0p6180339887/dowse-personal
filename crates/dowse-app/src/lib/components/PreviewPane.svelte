<script lang="ts">
	import { convertFileSrc } from '@tauri-apps/api/core';
	import { tick, untrack } from 'svelte';
	import * as api from '../api';
	import type { ContentChunk, DocMatch, SearchHit, TextSegment } from '../types';
	import { kindOf } from '$lib/fileKind';
	import { middleEllipsis } from '../pathTruncate';
	import FileIcon from './FileIcon.svelte';
	import Segments from './Segments.svelte';
	import { t } from '../i18n';

	let {
		hit,
		segments,
		loading,
		kind
	}: {
		hit: SearchHit | null;
		segments: TextSegment[] | null;
		loading: boolean;
		/** 索引里这篇文档的 kind：正常是 "text"/"image"，抽取失败是 "err:xxx"。 */
		kind: string | null;
	} = $props();

	// 代码类文件的正文用等宽字体排——对齐缩进、行号视觉上更容易对上，
	// 文档类（md/txt 等）保持正文字体不变。
	let isCode = $derived(hit !== null && kindOf(hit.path) === 'code');
	let isImage = $derived(hit !== null && kindOf(hit.path) === 'image');

	// 图片走 Tauri 的 asset 协议直接读本地文件（tauri.conf.json 里
	// app.security.assetProtocol 开了 scope: ["**"]——dowse 索引的目录是用户
	// 任意选的，没法预先收窄成一个固定范围）。同一路径每次 $derived 都会重新
	// 转换一次 URL，代价很小，不用额外缓存。
	let imageSrc = $derived(hit && isImage ? convertFileSrc(hit.path) : null);

	/// 每次按块读取的字符数。取 8000：一屏正文通常几千字，这个粒度下点一次
	/// "加载更多"就够再读一屏多，同时单次 IPC 载荷只有几十 KB。
	const CHUNK_CHARS = 8000;
	/// 文内查找的防抖窗口：打字期间不每个字符都跑一次全文扫描。
	const FIND_DEBOUNCE_MS = 180;

	/** 抽取失败的状态文档：预览区要给"不支持/加密/损坏/无可提取文字/超限"的
	 * 明确状态，而不是一页空白（验收条件：损坏或不支持的文件有明确状态）。 */
	let statusMessage = $derived(statusTextFor(kind));

	let mode = $state<'context' | 'full'>('context');

	// ---- 全文阅读：按块从索引里读正文，不重新解析原文件 ----
	let chunks = $state<ContentChunk[]>([]);
	let fullLoading = $state(false);
	let fullError = $state('');
	let fullToken = 0;
	/** 最近一次请求的块起点：出错重试时复用，不重新猜位置。 */
	let lastRequestedOffset = 0;

	// ---- 文内查找 ----
	let needle = $state('');
	let matches = $state<DocMatch[] | null>(null);
	let activeMatch = $state(0);
	let findPending = $state(false);
	let findToken = 0;
	let findTimer: ReturnType<typeof setTimeout> | undefined;

	let bodyEl: HTMLDivElement | undefined = $state();
	/** 复制选中/加载失败的瞬时提示，几秒后自己消失。 */
	let notice = $state('');

	let totalChars = $derived(chunks.length > 0 ? chunks[0].total_chars : 0);
	/** 已读到的末尾字符位置（按块的 offset+returned 算，跟后端同一套字符坐标）。 */
	let loadedChars = $derived(
		chunks.length === 0
			? 0
			: (() => {
					const last = chunks[chunks.length - 1];
					return last.offset_chars + last.returned_chars;
				})()
	);
	let fullDone = $derived(
		chunks.length > 0 && chunks[chunks.length - 1].next_offset_chars === null
	);

	/// 每块渲染用的 segment 切片。**坐标约定**：后端的命中区间和块的
	/// offset_chars 都是 Unicode 字符（标量值）计数，而 JS 字符串下标是
	/// UTF-16 单元，两者对中文以外的 emoji 并不等价。所以这里先把块内容拆成
	/// 码点数组（Array.from），用码点下标切片，再拼回字符串——绝不把后端的
	/// 字符坐标当 JS 下标直接用。
	let viewChunks = $derived(
		chunks.map((chunk) => ({
			chunk,
			segments: segmentsForChunk(chunk, matches)
		}))
	);

	function statusTextFor(value: string | null): string | null {
		if (!value || !value.startsWith('err:')) return null;
		switch (value) {
			case 'err:unsupported':
				return t.pvStatusUnsupported;
			case 'err:encrypted':
				return t.pvStatusEncrypted;
			case 'err:corrupted':
				return t.pvStatusCorrupted;
			case 'err:no-text':
				return t.pvStatusNoText;
			case 'err:oversize':
				return t.pvStatusOversize;
			default:
				return t.pvStatusUnknown;
		}
	}

	/// 把落在这一块里的命中切成高亮/非高亮段。
	/// - 命中区间按"全局字符坐标"给出，先减去块起点换成块内局部坐标；
	/// - 跨块边界的命中在每块里各切出自己那一半（块本身是连续读的，拼起来
	///   刚好是完整命中），不存在漏词或重复计数；
	/// - `matchIndex` 保留全局命中序号，供"当前是第几个"和滚动定位使用。
	function segmentsForChunk(chunk: ContentChunk, all: DocMatch[] | null): TextSegment[] {
		if (!all || all.length === 0) {
			return [{ text: chunk.content, highlighted: false }];
		}
		const base = chunk.offset_chars;
		const local: { start: number; end: number; index: number }[] = [];
		all.forEach((m, index) => {
			const start = m.start_char - base;
			const end = m.end_char - base;
			if (end <= 0 || start >= chunk.returned_chars) return;
			local.push({
				start: Math.max(0, start),
				end: Math.min(end, chunk.returned_chars),
				index
			});
		});
		if (local.length === 0) {
			return [{ text: chunk.content, highlighted: false }];
		}
		local.sort((a, b) => a.start - b.start);

		const cps = Array.from(chunk.content);
		const out: TextSegment[] = [];
		let cursor = 0;
		for (const hit of local) {
			if (hit.start > cursor) {
				out.push({ text: cps.slice(cursor, hit.start).join(''), highlighted: false });
			}
			out.push({
				text: cps.slice(hit.start, hit.end).join(''),
				highlighted: true,
				matchIndex: hit.index
			});
			cursor = Math.max(cursor, hit.end);
		}
		if (cursor < cps.length) {
			out.push({ text: cps.slice(cursor).join(''), highlighted: false });
		}
		return out;
	}

	function showNotice(message: string) {
		notice = message;
		setTimeout(() => {
			if (notice === message) notice = '';
		}, 2200);
	}

	// 切换文件：重置全文阅读与文内查找的全部状态。渲染函数之间不互调，统一由
	// 这个"状态重置 + 视图入口"处理，避免出现互相触发的环。
	let lastPath: string | null = null;
	let lastRevision = '';
	let documentPath = $derived(hit?.path ?? null);
	let documentRevision = $derived(hit ? `${hit.mtime}:${hit.size}` : '');
	$effect(() => {
		const path = documentPath;
		const revision = documentRevision;
		if (path === lastPath) {
			if (revision !== lastRevision) {
				lastRevision = revision;
				untrack(() => void reloadChangedDocument());
			}
			return;
		}
		lastPath = path;
		lastRevision = revision;
		mode = 'context';
		chunks = [];
		fullLoading = false;
		fullError = '';
		fullToken++;
		needle = '';
		matches = null;
		activeMatch = 0;
		findPending = false;
		findToken++;
		notice = '';
	});

	// Refresh loaded chunks when this document changes, without discarding the
	// reading mode, query or scroll position on unrelated index updates.
	async function reloadChangedDocument() {
		const path = hit?.path;
		const offsets = chunks.map((chunk) => chunk.offset_chars);
		const scrollTop = bodyEl?.scrollTop ?? 0;
		const token = ++fullToken;
		findToken++;
		clearTimeout(findTimer);
		matches = null;
		chunks = [];
		fullError = '';
		if (!path || mode !== 'full') return;
		fullLoading = true;
		try {
			const fresh: ContentChunk[] = [];
			for (const offset of offsets.length ? offsets : [0]) {
				const chunk = await api.readContentChunk(path, offset, CHUNK_CHARS);
				if (token !== fullToken) return;
				if (!chunk) throw new Error(t.pvDocMissing);
				if (offset === 0 || offset < chunk.total_chars) fresh.push(chunk);
			}
			if (fresh.length === 0) {
				const first = await api.readContentChunk(path, 0, CHUNK_CHARS);
				if (token !== fullToken) return;
				if (first) fresh.push(first);
			}
			chunks = fresh;
			await tick();
			if (token === fullToken && bodyEl) bodyEl.scrollTop = scrollTop;
		} catch (err) {
			if (token === fullToken) fullError = String(err);
		} finally {
			if (token === fullToken) fullLoading = false;
		}
	}

	async function loadChunkAt(offset: number) {
		if (!hit) return;
		const path = hit.path;
		lastRequestedOffset = offset;
		fullLoading = true;
		fullError = '';
		const token = ++fullToken;
		try {
			const chunk = await api.readContentChunk(path, offset, CHUNK_CHARS);
			// 过期响应直接丢弃：文件已经换了、或已经有更新的请求在跑。
			if (token !== fullToken) return;
			if (!chunk) {
				fullError = t.pvDocMissing;
				return;
			}
			insertChunk(chunk);
		} catch (err) {
			if (token !== fullToken) return;
			fullError = String(err);
		} finally {
			if (token === fullToken) fullLoading = false;
		}
	}

	function insertChunk(chunk: ContentChunk) {
		if (chunks.some((c) => c.offset_chars === chunk.offset_chars)) return;
		chunks = [...chunks, chunk].sort((a, b) => a.offset_chars - b.offset_chars);
	}

	function loadNextChunk() {
		const last = chunks[chunks.length - 1];
		if (!last) {
			void loadChunkAt(0);
			return;
		}
		if (last.next_offset_chars === null) return;
		void loadChunkAt(last.next_offset_chars);
	}

	function openFullView() {
		mode = 'full';
		if (chunks.length === 0) void loadChunkAt(0);
	}

	function switchMode(next: 'context' | 'full') {
		if (next === 'full') {
			openFullView();
		} else {
			mode = 'context';
		}
	}

	/// 保证"完整覆盖 [startChar, endChar) 这段区间"的块都已加载。块起点按固定
	/// 块大小对齐（后端每次从完整块起点读同样长度），所以块号 = 位置 / 块大小。
	///
	/// 为什么要覆盖到 endChar 而不是只看起点：块边界可能正好落在一个词的中间，
	/// 只加载起点所在块的话，这个词只有前半截在页面里、后半截根本没加载，高亮
	/// 看起来是断的。把区间覆盖到的每一块都补齐，前半截和后半截在同一页里接上，
	/// 才和"命中高亮完整"的预期一致（`segmentsForChunk` 会按块各自切出自己那半截，
	/// 块本身连续读、拼起来就是完整命中，不会重复计数）。
	async function ensureChunksCovering(startChar: number, endChar: number) {
		const first = Math.floor(startChar / CHUNK_CHARS) * CHUNK_CHARS;
		// 用 endChar - 1 定位"最后一个字符"所在的块；空区间（理论上不会有）
		// 退化成只覆盖起点那一块。
		const lastPos = endChar > startChar ? endChar - 1 : startChar;
		const last = Math.floor(lastPos / CHUNK_CHARS) * CHUNK_CHARS;
		for (let offset = first; offset <= last; offset += CHUNK_CHARS) {
			if (chunks.some((c) => c.offset_chars === offset)) continue;
			await loadChunkAt(offset);
		}
	}

	async function scrollToActiveMatch() {
		await tick();
		const el = bodyEl?.querySelector('[data-active-match="true"]');
		el?.scrollIntoView({ block: 'center' });
	}

	async function jumpToMatch(index: number) {
		if (!matches || matches.length === 0) return;
		const total = matches.length;
		activeMatch = ((index % total) + total) % total;
		if (mode !== 'full') mode = 'full';
		const target = matches[activeMatch];
		await ensureChunksCovering(target.start_char, target.end_char);
		await scrollToActiveMatch();
	}

	// 文内查找：查找词或文件一变，先作废旧计数/旧位置（不把上一轮的命中数
	// 当成最终结果显示），再短防抖发起新查询；过期响应用 token 丢弃。
	$effect(() => {
		const path = documentPath;
		const revision = documentRevision;
		void revision;
		const raw = needle;
		clearTimeout(findTimer);
		const token = ++findToken;
		matches = null;
		activeMatch = 0;
		if (!path || raw.length === 0) {
			findPending = false;
			return;
		}
		findPending = true;
		findTimer = setTimeout(async () => {
			try {
				const result = await api.findInDoc(path, raw);
				if (token !== findToken) return;
				matches = result?.matches ?? [];
				activeMatch = 0;
				findPending = false;
				if (matches.length > 0) void jumpToMatch(0);
			} catch {
				if (token !== findToken) return;
				matches = [];
				findPending = false;
			}
		}, FIND_DEBOUNCE_MS);
		return () => clearTimeout(findTimer);
	});

	function onFindKeydown(e: KeyboardEvent) {
		if (e.key === 'Enter') {
			e.preventDefault();
			void jumpToMatch(activeMatch + (e.shiftKey ? -1 : 1));
			return;
		}
		if (e.key === 'Escape') {
			// 只清空查找框，不把整个窗口收起来——用户按 Esc 想退出的是查找。
			e.preventDefault();
			e.stopPropagation();
			needle = '';
		}
	}

	function openFile() {
		if (!hit) return;
		api.openFile(hit.path).catch((err) => showNotice(t.toastOpenFailed(String(err))));
	}

	function revealFile() {
		if (!hit) return;
		api.revealInFolder(hit.path).catch((err) => showNotice(t.toastRevealFailed(String(err))));
	}

	/// 复制用户在正文里选中的文字：正文本身是可选中文本（CSS 只调了
	/// white-space/word-break，没有禁选），这里只是给一个显式的按钮入口，
	/// 省得用户去猜 Ctrl+C 在预览区能不能用。
	function copySelection() {
		const selected = window.getSelection()?.toString() ?? '';
		if (selected.length === 0) {
			showNotice(t.pvCopyNothing);
			return;
		}
		navigator.clipboard
			.writeText(selected)
			.then(() => showNotice(t.pvCopied))
			.catch(() => showNotice(t.toastCopyFailed));
	}
</script>

<div class="preview">
	{#if !hit}
		<p class="hint">{t.previewSelectHint}</p>
	{:else}
		<div class="header">
			<FileIcon path={hit.path} />
			<span class="name"><Segments segments={hit.name_segments} /></span>
		</div>
		<div class="path">{middleEllipsis(hit.display_path)}</div>

		<div class="toolbar">
			<div class="tabs">
				<button
					type="button"
					class="tab"
					class:on={mode === 'context'}
					onclick={() => switchMode('context')}
				>
					{t.pvTabContext}
				</button>
				<button
					type="button"
					class="tab"
					class:on={mode === 'full'}
					disabled={isImage || statusMessage !== null}
					onclick={() => switchMode('full')}
				>
					{t.pvTabFull}
				</button>
			</div>
			<div class="actions">
				<button type="button" onclick={openFile}>{t.pvOpenFile}</button>
				<button type="button" onclick={revealFile}>{t.pvReveal}</button>
				<button type="button" onclick={copySelection}>{t.pvCopySelection}</button>
			</div>
		</div>

		{#if notice}
			<p class="hint">{notice}</p>
		{/if}

		{#if mode === 'full' && !isImage && statusMessage === null}
			<div class="findbar">
				<svg class="find-icon" width="13" height="13" viewBox="0 0 18 18" fill="none" aria-hidden="true">
					<circle cx="8" cy="8" r="5.4" stroke="currentColor" stroke-width="1.6" />
					<path d="M12.2 12.2 16 16" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" />
				</svg>
				<input
					type="text"
					class="find-input"
					placeholder={t.pvFindPlaceholder}
					bind:value={needle}
					onkeydown={onFindKeydown}
					autocomplete="off"
					spellcheck="false"
				/>
				<span class="find-count" aria-live="polite">
					{#if findPending}
						{t.pvFindSearching}
					{:else if matches !== null && matches.length === 0}
						{t.pvFindNone}
					{:else if matches !== null}
						{t.pvFindCount(activeMatch + 1, matches.length)}
					{/if}
				</span>
				<button
					type="button"
					class="find-nav"
					title={t.pvFindPrev}
					aria-label={t.pvFindPrev}
					disabled={!matches || matches.length === 0}
					onclick={() => jumpToMatch(activeMatch - 1)}
				>
					<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4 10 4-4 4 4" /></svg>
				</button>
				<button
					type="button"
					class="find-nav"
					title={t.pvFindNext}
					aria-label={t.pvFindNext}
					disabled={!matches || matches.length === 0}
					onclick={() => jumpToMatch(activeMatch + 1)}
				>
					<svg viewBox="0 0 16 16" aria-hidden="true"><path d="m4 6 4 4 4-4" /></svg>
				</button>
			</div>
		{/if}

		<div class="body" bind:this={bodyEl}>
			{#if isImage}
				{#if imageSrc}
					<img class="image-preview" src={imageSrc} alt={hit.name} />
				{/if}
				{#if loading}
					<p class="hint">{t.ocrLoading}</p>
				{:else if segments && segments.length > 0}
					<p class="ocr-caption">{t.ocrCaption}</p>
					<div class="ocr-text-wrap">
						<p class="context ocr-text"><Segments {segments} /></p>
					</div>
				{:else}
					<p class="hint">{t.ocrEmpty}</p>
				{/if}
			{:else if statusMessage !== null}
				<div class="status">
					<p class="status-text">{statusMessage}</p>
					<p class="hint">{t.previewEmpty}</p>
				</div>
			{:else if mode === 'context'}
				{#if loading}
					<p class="hint">{t.previewLoading}</p>
				{:else if segments && segments.length > 0}
					<p class="context" class:mono={isCode}><Segments {segments} /></p>
				{:else}
					<p class="hint">{t.previewEmpty}</p>
				{/if}
			{:else}
				{#if chunks.length === 0 && fullLoading}
					<p class="hint">{t.previewLoading}</p>
				{:else if chunks.length === 0 && fullError}
					<p class="hint">{fullError}</p>
					<button type="button" class="retry" onclick={() => loadChunkAt(lastRequestedOffset)}>
						{t.pvRetry}
					</button>
				{:else}
					{#each viewChunks as view, i (view.chunk.offset_chars)}
						{#if i > 0 && viewChunks[i - 1].chunk.offset_chars + viewChunks[i - 1].chunk.returned_chars < view.chunk.offset_chars}
							<p class="elision">···</p>
						{/if}
						<p class="context" class:mono={isCode}>
							<Segments segments={view.segments} activeIndex={activeMatch} />
						</p>
					{/each}

					{#if fullError}
						<p class="hint">{fullError}</p>
						<button type="button" class="retry" onclick={() => loadChunkAt(lastRequestedOffset)}>
							{t.pvRetry}
						</button>
					{:else if fullDone}
						<p class="full-state">{t.pvFullLoaded(totalChars)}</p>
					{:else}
						<button type="button" class="load-more" disabled={fullLoading} onclick={loadNextChunk}>
							{fullLoading ? t.previewLoading : t.pvLoadMore}
						</button>
						<p class="full-state">{t.pvHasMore(loadedChars, totalChars)}</p>
					{/if}
				{/if}
			{/if}
		</div>
	{/if}
</div>

<style>
	.preview {
		height: 100%;
		display: flex;
		flex-direction: column;
		padding: 20px;
		gap: 8px;
		overflow: hidden;
	}

	.header {
		display: flex;
		align-items: center;
		gap: 8px;
	}

	.name {
		font-size: 13.5px;
		font-weight: 500;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.path {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--fg-tertiary);
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		padding-bottom: 8px;
		border-bottom: 1px solid var(--divider);
	}

	/* 视图切换 + 文件动作：两个分区，左边切视图、右边对文件动手。都做成
	   幽灵按钮（默认几乎不占视觉重量），跟搜索框右侧的控件同一档。 */
	.toolbar {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: 10px;
		flex-wrap: wrap;
	}

	.tabs {
		display: flex;
		align-items: center;
		gap: 2px;
	}

	.tab,
	.actions button {
		border: 0;
		background: transparent;
		color: var(--fg-tertiary);
		font-size: 11.5px;
		line-height: 1;
		padding: 5px 8px;
		border-radius: var(--radius-chip);
		cursor: default;
		transition:
			background 90ms ease-out,
			color 90ms ease-out;
	}

	.tab:hover:not(:disabled),
	.actions button:hover {
		background: var(--row-hover);
		color: var(--fg-primary);
	}

	.tab.on {
		background: var(--accent-soft);
		color: var(--accent-strong);
		font-weight: 500;
	}

	.tab:disabled {
		opacity: 0.35;
	}

	.actions {
		display: flex;
		align-items: center;
		gap: 2px;
	}

	.findbar {
		display: flex;
		align-items: center;
		gap: 6px;
		padding: 5px 8px;
		border: 1px solid var(--divider);
		border-radius: var(--radius-chip);
		flex-shrink: 0;
	}

	.find-icon {
		color: var(--fg-tertiary);
		flex-shrink: 0;
	}

	.find-input {
		flex: 1;
		min-width: 0;
		border: none;
		outline: none;
		background: transparent;
		font-size: 16px;
		color: var(--fg-primary);
	}

	.find-count {
		font-size: 11px;
		color: var(--fg-tertiary);
		font-variant-numeric: tabular-nums;
		white-space: nowrap;
		flex-shrink: 0;
	}

	.find-nav {
		display: grid;
		place-items: center;
		width: 22px;
		height: 22px;
		padding: 0;
		border: 0;
		background: transparent;
		color: var(--fg-secondary);
		border-radius: var(--radius-chip);
		cursor: default;
		flex-shrink: 0;
	}

	.find-nav:hover:not(:disabled) {
		background: var(--row-hover);
		color: var(--fg-primary);
	}

	.find-nav:disabled {
		opacity: 0.3;
	}

	.find-nav svg {
		width: 12px;
		height: 12px;
		fill: none;
		stroke: currentColor;
		stroke-width: 1.6;
		stroke-linecap: round;
		stroke-linejoin: round;
	}

	.body {
		flex: 1;
		overflow-y: auto;
	}

	.context {
		margin: 0;
		font-size: 14px;
		line-height: 1.85;
		color: var(--fg-reading);
		white-space: pre-wrap;
		word-break: break-word;
	}

	.context.mono {
		font-family: var(--font-mono);
		font-size: 13.5px;
	}

	/* 跳读留下的空档：说明中间还有没加载的正文（不是文档本身缺内容）。 */
	.elision {
		margin: 6px 0;
		font-size: 12px;
		color: var(--fg-tertiary);
		text-align: center;
		letter-spacing: 0.2em;
	}

	.load-more,
	.retry {
		margin: 10px 0 4px;
		width: 100%;
		padding: 7px 10px;
		border: 1px solid var(--divider);
		border-radius: var(--radius-chip);
		background: transparent;
		color: var(--fg-secondary);
		font-size: 12px;
		cursor: default;
		transition: background 90ms ease-out;
	}

	.load-more:hover:not(:disabled),
	.retry:hover {
		background: var(--row-hover);
		color: var(--fg-primary);
	}

	.load-more:disabled {
		opacity: 0.5;
	}

	/* "还有内容/全文已加载"这类状态文字：跟分区小标题同一档，安静但不隐形。 */
	.full-state {
		margin: 6px 0 0;
		font-size: 11px;
		letter-spacing: 0.04em;
		color: var(--fg-tertiary);
		text-align: center;
	}

	/* 抽取失败的状态块：给一个浅底容器，让"这个文件为什么没有正文"看起来
	   是结论，而不是一条被忽略的提示。 */
	.status {
		padding: 12px 14px;
		border: 1px solid var(--divider);
		border-radius: 8px;
		background: var(--row-hover);
		display: flex;
		flex-direction: column;
		gap: 8px;
	}

	.status-text {
		margin: 0;
		font-size: 14px;
		line-height: 1.7;
		color: var(--fg-primary);
	}

	.image-preview {
		display: block;
		max-width: 100%;
		max-height: 260px;
		object-fit: contain;
		border-radius: 6px;
		border: 1px solid var(--divider);
		margin-bottom: 10px;
	}

	/* 分区小标题规格：跟 +page.svelte 的 .results-heading 同一档——11px、
	   微字距、三级灰，跟正文（.context）明确区分出"这是个标签，不是正文"。 */
	.ocr-caption {
		margin: 0 0 4px;
		font-size: 11px;
		letter-spacing: 0.04em;
		color: var(--fg-tertiary);
	}

	/* OCR 文字段限高约 6~8 行，超出内部滚动——手机截图的状态栏文字、大段
	   OCR 误识别是常态，不能让预览区被一张图的识别结果撑到没法看别的信息。 */
	.ocr-text-wrap {
		max-height: 200px;
		overflow-y: auto;
	}

	/* 对比度比普通正文提一档：OCR 识别本身就有噪声，用更高对比度的主文字色
	   帮助辨认，跟旁边的 .hint/.ocr-caption 三级灰形成清楚的层次。 */
	.ocr-text {
		color: var(--fg-primary);
	}

	.hint {
		margin: 0;
		font-size: 13px;
		color: var(--fg-secondary);
	}
</style>
