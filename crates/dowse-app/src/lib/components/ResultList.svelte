<script lang="ts">
	import { tick } from 'svelte';
	import { animate, stagger } from 'motion';
	import type { SearchHit, SortOption } from '../types';
	import ResultRow from './ResultRow.svelte';
	import { t } from '../i18n';

	let {
		hits,
		selectedIndex,
		sort,
		onsort,
		onhover,
		onselect,
		onopen,
		oncontextmenu
	}: {
		hits: SearchHit[];
		selectedIndex: number;
		sort: SortOption;
		onsort: (next: SortOption) => void;
		onhover: (i: number) => void;
		onselect: (i: number) => void;
		onopen: (i: number) => void;
		oncontextmenu: (i: number) => void;
	} = $props();

	// 只有引擎真的能排的列才标 sortable。刻意**不给**"文件名/路径"开排序：
	// 索引里没有可排序的字段，前端只能排当前页这几十条，用户会误以为排完就是
	// 全局顺序——那比不给排序更糟。修改时间和体积有 fast field，交给引擎排，
	// 排的是整个命中集合。
	// 默认列宽按 1120 窗口下结果左栏（≈ 600–650px 可用）设计：五列合计 ≤ 600，
	// 保证修改时间 / 大小这两列默认就完整可见，不再被挤到视口外（旧默认合计 834
	// 会顶掉它们）。保留拖拽调整；若拖宽到超过左栏，.table 横向滚动让它们仍可触达。
	const COLS = [
		{ key: 'name', label: () => t.colName, width: 232, min: 120, sortable: false },
		{ key: 'path', label: () => t.colPath, width: 118, min: 60, sortable: false },
		{ key: 'type', label: () => t.colType, width: 52, min: 40, sortable: false },
		{ key: 'mtime', label: () => t.colMtime, width: 122, min: 96, sortable: true },
		{ key: 'size', label: () => t.colSize, width: 64, min: 52, sortable: true }
	];

	// 列宽存本地：这是纯个人偏好，不值得进配置文件，但关掉窗口再打开应该还在。
	const STORE_KEY = 'dowse.result.columns';

	function loadWidths(): number[] {
		try {
			const raw = localStorage.getItem(STORE_KEY);
			if (raw) {
				const arr = JSON.parse(raw) as number[];
				if (Array.isArray(arr) && arr.length === COLS.length) return arr;
			}
		} catch {
			// 读坏了就用默认值——列宽不是关键数据，不该因为一条坏记录开不了窗。
		}
		return COLS.map((c) => c.width);
	}

	let widths = $state<number[]>(loadWidths());

	function saveWidths() {
		try {
			localStorage.setItem(STORE_KEY, JSON.stringify(widths));
		} catch {
			// 存不了就算了，不影响本次会话使用。
		}
	}

	let listEl: HTMLDivElement | undefined = $state();
	let drag: { i: number; startX: number; startW: number } | null = $state(null);

	function startDrag(e: MouseEvent, i: number) {
		e.preventDefault();
		e.stopPropagation();
		drag = { i, startX: e.clientX, startW: widths[i] };
		window.addEventListener('mousemove', onDragMove);
		window.addEventListener('mouseup', endDrag);
	}

	function onDragMove(e: MouseEvent) {
		if (!drag) return;
		// 夹住下限：列宽拖到 0 会让整列消失且拖不回来。
		widths[drag.i] = Math.max(COLS[drag.i].min, drag.startW + (e.clientX - drag.startX));
	}

	function endDrag() {
		drag = null;
		window.removeEventListener('mousemove', onDragMove);
		window.removeEventListener('mouseup', endDrag);
		saveWidths();
	}

	// 表头和行共用同一份 grid 模板，改一处两边一起变。
	let cols = $derived(widths.map((w) => `${w}px`).join(' '));

	function sortDir(key: string): 'asc' | 'desc' | null {
		if (key === 'mtime') {
			if (sort === 'mtime_desc') return 'desc';
			if (sort === 'mtime_asc') return 'asc';
			return null;
		}
		if (key === 'size') {
			if (sort === 'size_desc') return 'desc';
			if (sort === 'size_asc') return 'asc';
			return null;
		}
		return null;
	}

	function clickSort(key: string) {
		if (key === 'mtime') onsort(sort === 'mtime_desc' ? 'mtime_asc' : 'mtime_desc');
		else if (key === 'size') onsort(sort === 'size_desc' ? 'size_asc' : 'size_desc');
	}

	// 选中项变了就把它滚进可视区——键盘走天下的场景下，用户不该需要手动滚动
	// 去找选中行在哪。
	$effect(() => {
		selectedIndex;
		if (!listEl) return;
		const row = listEl.querySelector<HTMLElement>(`[data-idx="${selectedIndex}"]`);
		row?.scrollIntoView({ block: 'nearest' });
	});

	// 新的一批结果上屏时做极短的错峰滑入——只给前 10 行做，结果多时不能让
	// 用户等一串错峰跑完才看到东西；弹簧动画本身也要控制在 150ms 内。
	let lastRevealKey = '';
	$effect(() => {
		const key = hits.map((h) => h.path).join('|');
		if (key === lastRevealKey || !listEl) return;
		lastRevealKey = key;
		tick().then(() => {
			if (!listEl) return;
			const rows = Array.from(listEl.querySelectorAll<HTMLElement>('.row')).slice(0, 10);
			if (rows.length === 0) return;
			animate(
				rows,
				{ opacity: [0, 1], y: [6, 0] },
				{ type: 'spring', bounce: 0.15, duration: 0.12, delay: stagger(0.012) }
			);
		});
	});
</script>

<div
	class="table"
	bind:this={listEl}
	role="listbox"
	aria-label={t.resultListLabel}
	style="--cols: {cols}"
>
	<div class="head">
		{#each COLS as col, i (col.key)}
			<div class="hcell">
				{#if col.sortable}
					<button
						type="button"
						class="hbtn"
						class:active={sortDir(col.key) !== null}
						onclick={() => clickSort(col.key)}
					>
						<span class="hlabel">{col.label()}</span>
						<span class="arrow"
							>{sortDir(col.key) === 'asc' ? '▲' : sortDir(col.key) === 'desc' ? '▼' : '↕'}</span
						>
					</button>
				{:else}
					<span class="hlabel">{col.label()}</span>
				{/if}
				<!-- svelte-ignore a11y_no_static_element_interactions: 列宽拖动手柄是纯鼠标交互，键盘有其它方式访问全部列内容 -->
				<span class="grip" class:dragging={drag?.i === i} onmousedown={(e) => startDrag(e, i)}
				></span>
			</div>
		{/each}
	</div>

	<div class="body">
		{#each hits as hit, i (hit.path)}
			<div data-idx={i}>
			<ResultRow
				{hit}
				selected={i === selectedIndex}
				onhover={() => onhover(i)}
				onselect={() => onselect(i)}
				onopen={() => onopen(i)}
				oncontextmenu={() => oncontextmenu(i)}
			/>
			</div>
		{/each}
	</div>
</div>

<style>
	.table {
		flex: 1;
		min-height: 0;
		display: flex;
		flex-direction: column;
		/* 允许横向滚动：列被拖宽到超过左栏时，表头与正文一起横移，保持对齐，
		   修改时间/大小两列始终可触达，不会被永久裁掉。 */
		overflow-x: auto;
		overflow-y: hidden;
	}

	.head {
		display: grid;
		grid-template-columns: var(--cols);
		align-items: center;
		height: 28px;
		flex-shrink: 0;
		padding: 0 8px;
		border-bottom: 1px solid var(--border-subtle, rgba(127, 127, 127, 0.18));
	}

	.hcell {
		position: relative;
		display: flex;
		align-items: center;
		min-width: 0;
	}

	.hlabel {
		font-size: 13px;
		font-weight: 600;
		letter-spacing: 0.02em;
		color: var(--fg-secondary);
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}

	.hbtn {
		display: flex;
		align-items: center;
		gap: 4px;
		min-width: 0;
		padding: 2px 4px;
		margin-left: -4px;
		border: 0;
		border-radius: 4px;
		background: transparent;
		font: inherit;
		color: inherit;
		cursor: pointer;
	}

	.hbtn:hover {
		background: var(--row-hover);
	}

	.hbtn.active {
		color: var(--fg-primary);
	}

	.arrow {
		font-size: 8px;
		opacity: 0.55;
	}

	.hbtn.active .arrow {
		opacity: 1;
	}

	/* 拖宽手柄：默认不可见，hover 整列时才浮现，避免表头看起来脏。 */
	.grip {
		position: absolute;
		right: -4px;
		top: 4px;
		bottom: 4px;
		width: 8px;
		cursor: col-resize;
		border-radius: 2px;
	}

	.grip:hover,
	.grip.dragging {
		background: var(--accent-soft);
	}

	.body {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		overflow-x: hidden;
		padding: 4px 0;
	}
</style>
