<script lang="ts">
	import type { TextSegment } from '../types';

	/// `activeIndex` 只给"文内查找"用：高亮段上带的 `matchIndex` 命中这个值时，
	/// 该段会被标成"当前命中"（更强的高亮 + data 属性供父组件滚动定位）。
	/// 上下文预览（命中片段）不传它，所有高亮段一视同仁。
	let {
		segments,
		activeIndex = null
	}: { segments: TextSegment[]; activeIndex?: number | null } = $props();
</script>

{#each segments as seg, i (i)}
	{#if seg.highlighted}
		{@const isActive = activeIndex !== null && seg.matchIndex === activeIndex}
		<mark
			class="hl"
			class:active={isActive}
			data-active-match={isActive ? 'true' : undefined}>{seg.text}</mark>
	{:else}
		{seg.text}
	{/if}
{/each}
