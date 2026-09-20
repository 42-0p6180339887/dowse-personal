<script lang="ts">
	import type { SearchHit } from '../types';
	import FileIcon from './FileIcon.svelte';
	import Segments from './Segments.svelte';

	let {
		hit,
		selected,
		onhover,
		onselect,
		onopen,
		oncontextmenu
	}: {
		hit: SearchHit;
		selected: boolean;
		onhover: () => void;
		onselect: () => void;
		onopen: () => void;
		oncontextmenu: () => void;
	} = $props();

	// 右键先把这一行选中（跟资源管理器的习惯一致），再交给父组件弹原生菜单——
	// 菜单本身是 Win32 原生绘制的（见 Rust 侧 context_menu.rs），这里只负责触发。
	// 注意：onhover 只在右键这一**显式**动作里用来高亮选中行，鼠标单纯划过
	// （onmouseenter）不调用它——否则悬停就会切走正在阅读的文件。
	function handleContextMenu(e: MouseEvent) {
		e.preventDefault();
		onhover();
		oncontextmenu();
	}

	// "路径"列只显示目录部分：文件名已经单独占一列了，重复整条路径会挤掉别的信息。
	let dirOf = $derived.by(() => {
		const p = hit.display_path;
		const slash = Math.max(p.lastIndexOf('/'), p.lastIndexOf('\\'));
		return slash >= 0 ? p.slice(0, slash + 1) : p;
	});

	// 类型列直接用后端给的 ext（索引里存的小写扩展名），不自己从文件名里再截一遍。
	let typeLabel = $derived(hit.ext ? hit.ext.toUpperCase() : '');

	// 修改时间：索引里没拿到（0）时显示"—"，绝不显示成 1970-01-01——
	// 那会把"建索引时这个字段没值"伪装成"这个文件是 1970 年改的"。
	let mtimeText = $derived.by(() => {
		if (!hit.mtime) return '—';
		const d = new Date(hit.mtime);
		const p2 = (n: number) => String(n).padStart(2, '0');
		return `${d.getFullYear()}-${p2(d.getMonth() + 1)}-${p2(d.getDate())} ${p2(d.getHours())}:${p2(d.getMinutes())}`;
	});

	let sizeText = $derived.by(() => {
		const b = hit.size;
		if (!b) return '—';
		if (b < 1024) return `${b} B`;
		if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)} KB`;
		if (b < 1024 * 1024 * 1024) return `${(b / 1024 / 1024).toFixed(1)} MB`;
		return `${(b / 1024 / 1024 / 1024).toFixed(2)} GB`;
	});
</script>

<!-- 单击 = 仅选中并预览（不打开原文件）；双击 / 键盘 Enter = 打开一次。
     鼠标**划过**不绑定任何预览切换（见 onhover 的注释），避免悬停切走阅读对象。 -->
<button
	type="button"
	class="row"
	class:selected
	onclick={onselect}
	ondblclick={onopen}
	onkeydown={(event) => {
		if (event.key === 'Enter' && !event.repeat) {
			event.preventDefault();
			event.stopPropagation();
			onopen();
		}
	}}
	oncontextmenu={handleContextMenu}
>
	<span class="c c-name" title={hit.name}>
		<span class="ico"><FileIcon path={hit.path} /></span>
		<span class="nm"><Segments segments={hit.name_segments} /></span>
	</span>
	<span class="c c-path" title={dirOf}>{dirOf}</span>
	<span class="c c-type">{typeLabel}</span>
	<span class="c c-mtime">{mtimeText}</span>
	<span class="c c-size">{sizeText}</span>
</button>

<style>
	/* 列宽由父组件（ResultList）通过 --cols 传进来，行和表头共用同一份模板，
	   拖动表头改列宽时两边一起变，不会错位。 */
	.row {
		display: grid;
		grid-template-columns: var(--cols);
		align-items: center;
		width: 100%;
		height: 30px;
		padding: 0 8px;
		border: 0;
		border-radius: var(--radius-row, 6px);
		background: transparent;
		font: inherit;
		text-align: left;
		cursor: default;
		color: var(--fg-primary);
		transition: background-color 0.09s ease-out;
	}

	.row:hover {
		background: var(--row-hover);
	}

	/* 选中态只用纯色块——跟卡片版一致，切换选中时不抖动布局。 */
	.row.selected {
		background: var(--accent-soft);
	}

	.c {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-size: 13.5px;
		line-height: 1.35;
	}

	.c-name {
		display: flex;
		align-items: center;
		gap: 7px;
		font-weight: 500;
	}

	.ico {
		display: flex;
		align-items: center;
		flex-shrink: 0;
	}

	.nm {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.c-path {
		font-family: var(--font-mono);
		font-size: 11px;
		color: var(--fg-tertiary);
	}

	.c-type {
		font-size: 10.5px;
		font-weight: 500;
		letter-spacing: 0.02em;
		color: var(--fg-tertiary);
		opacity: 0.8;
	}

	/* 数字列用等宽数字，上下行对齐才不会因为字宽跳动看着乱。 */
	.c-mtime,
	.c-size {
		font-size: 13px;
		color: var(--fg-reading);
		font-variant-numeric: tabular-nums;
	}

	.c-size {
		text-align: right;
	}
</style>
