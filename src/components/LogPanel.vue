<script setup lang="ts">
import { nextTick, ref, watch } from "vue";
import type { LogLine } from "../api";

const props = defineProps<{ lines: LogLine[]; follow?: boolean }>();
const box = ref<HTMLDivElement | null>(null);

watch(
  () => props.lines.length,
  async () => {
    if (props.follow === false) return;
    await nextTick();
    const el = box.value;
    if (el) el.scrollTop = el.scrollHeight;
  },
  { immediate: true },
);
</script>

<template>
  <div ref="box" class="logs">
    <div v-if="lines.length === 0" class="empty">暂无日志</div>
    <div v-for="line in lines" :key="line.seq" class="line">
      <span class="stream" :class="{ stderr: line.stream === 'stderr' }">{{ line.stream }}</span>
      <span class="text">{{ line.text }}</span>
    </div>
  </div>
</template>
