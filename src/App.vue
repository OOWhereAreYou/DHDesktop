<script setup lang="ts">
/**
 * 按窗口标签分流:主窗口是启动页,`console` 窗口是控制台。
 *
 * 两个窗口加载同一份前端产物,靠 label 区分 —— 这样只需要一个入口,
 * 不必为控制台单独配置第二个 HTML 入口。
 */
import { getCurrentWindow } from "@tauri-apps/api/window";
import StartupView from "./views/StartupView.vue";
import ConsoleView from "./views/ConsoleView.vue";

const isConsole = getCurrentWindow().label === "console";
</script>

<template>
  <ConsoleView v-if="isConsole" />
  <StartupView v-else />
</template>
