<script lang="ts">
	// 幽灵态下拉——Raycast 手法：默认态几乎不存在（无边框无底色、11~12px 灰字），
	// 只有 hover 才微亮；选中非默认值时换成所选项文字并用水蓝微标记。
	// 键盘导航（Ctrl+P/Ctrl+S 开关、↑↓ 选、Enter 定、Esc 关）由父组件
	// （+page.svelte 的 handleKeydown）统一处理——这里只负责呈现和鼠标交互，
	// 避免菜单打开时把 DOM 焦点从搜索输入框挪走导致打字体验被打断。

	let {
		idleLabel,
		options,
		value,
		defaultValue,
		open,
		menuAlign = 'right',
		activeIndex = $bindable(0),
		onselect,
		ontoggle
	}: {
		idleLabel: string;
		options: readonly { value: string; label: string }[];
		value: string;
		defaultValue: string;
		open: boolean;
		// 菜单对齐方式：右侧控件（类型/排序）默认右对齐；左侧控件（范围/模式/匹配）
		// 传 'left'，否则长路径菜单会从左窄按钮向右伸出、被窗口左边裁掉。
		menuAlign?: 'left' | 'right';
		activeIndex?: number;
		onselect: (value: string) => void;
		ontoggle: () => void;
	} = $props();

	let isDefault = $derived(value === defaultValue);
	let selectedLabel = $derived(options.find((o) => o.value === value)?.label ?? idleLabel);
	let triggerTitle = $derived(isDefault ? idleLabel : selectedLabel);
</script>

<div class="dropdown">
	<button type="button" class="trigger" class:marked={!isDefault} onclick={ontoggle}>
		<span class="trigger-text" title={triggerTitle}>{isDefault ? idleLabel : selectedLabel}</span>
		<svg class="chevron" width="10" height="10" viewBox="0 0 10 10" aria-hidden="true">
			<path
				d="M2.5 4l2.5 2.5L7.5 4"
				stroke="currentColor"
				stroke-width="1.3"
				fill="none"
				stroke-linecap="round"
				stroke-linejoin="round"
			/>
		</svg>
	</button>
	{#if open}
		<div class="menu" class:left={menuAlign === 'left'} role="listbox">
			{#each options as opt, i (opt.value)}
				<button
					type="button"
					class="item"
					class:current={opt.value === value}
					class:hovered={i === activeIndex}
					title={opt.label}
					onmouseenter={() => (activeIndex = i)}
					onclick={() => onselect(opt.value)}
				>
					<span class="item-label">{opt.label}</span>
				</button>
			{/each}
		</div>
	{/if}
</div>

<style>
	.dropdown {
		position: relative;
	}

	.trigger {
		display: inline-flex;
		align-items: center;
		gap: 4px;
		border: none;
		background: transparent;
		padding: 4px 6px;
		border-radius: var(--radius-chip);
		font: inherit;
		font-size: 12px;
		color: var(--fg-tertiary);
		cursor: default;
		transition:
			color 0.12s ease-out,
			background-color 0.12s ease-out;
	}

	.trigger:hover {
		color: var(--fg-secondary);
		background: var(--row-hover);
	}

	.trigger.marked {
		color: var(--accent-strong);
	}

	.trigger-text {
		white-space: nowrap;
		max-width: 220px;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.chevron {
		flex-shrink: 0;
		opacity: 0.75;
	}

	.menu {
		position: absolute;
		top: calc(100% + 6px);
		right: 0;
		z-index: 10;
		min-width: 108px;
		/* 视口限宽：长路径菜单不会伸出窗口被裁；桌面端封顶 340px。 */
		max-width: min(340px, calc(100vw - 24px));
		padding: 4px;
		display: flex;
		flex-direction: column;
		gap: 1px;
		background: var(--glass-tint);
		border: 1px solid var(--panel-border);
		border-radius: var(--radius-row);
		box-shadow: var(--panel-shadow);
	}

	/* 左侧控件（范围/模式/匹配）菜单改为左对齐，避免从窄按钮向右溢出被窗口裁切。 */
	.menu.left {
		right: auto;
		left: 0;
	}

	.item {
		border: none;
		background: transparent;
		text-align: left;
		font: inherit;
		font-size: 12.5px;
		padding: 6px 8px;
		border-radius: 6px;
		color: var(--fg-primary);
		cursor: default;
	}

	.item-label {
		display: block;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}

	.item.hovered {
		background: var(--row-hover);
	}

	.item.current {
		color: var(--accent-strong);
		font-weight: 500;
	}
</style>
