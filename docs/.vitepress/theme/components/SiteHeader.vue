<script setup lang="ts">
import { useData, useRoute } from "vitepress";

const { theme } = useData();
const route = useRoute();
const nav = theme.value.nav ?? [];
</script>

<template>
  <header class="kage-header">
    <div class="kage-header__inner">
      <a class="kage-header__brand" href="/">
        <span class="kage-header__prompt">$</span>
        <span class="kage-header__name">kage</span>
        <span class="kage-header__caret" aria-hidden="true"></span>
      </a>
      <nav class="kage-header__nav">
        <a
          v-for="item in nav"
          :key="item.link"
          :href="item.link"
          class="kage-header__link"
          :class="{ 'is-active': route.path.startsWith(item.link) }"
        >
          {{ item.text }}
        </a>
        <a
          v-for="social in theme.socialLinks ?? []"
          :key="social.link"
          :href="social.link"
          class="kage-header__link kage-header__link--muted"
          target="_blank"
          rel="noreferrer"
        >
          source
        </a>
      </nav>
    </div>
  </header>
</template>
