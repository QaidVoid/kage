<script setup lang="ts">
import { useData, useRoute } from "vitepress";

const { theme } = useData();
const route = useRoute();
const sections = theme.value.sections ?? [];
</script>

<template>
  <aside class="kage-rail">
    <div
      v-for="section in sections"
      :key="section.label"
      class="kage-rail__section"
    >
      <div class="kage-rail__header">
        <span class="kage-rail__glyph">{{ section.glyph }}</span>
        <span class="kage-rail__label">{{ section.label }}</span>
      </div>
      <ul class="kage-rail__list">
        <li v-for="item in section.items" :key="item.link">
          <a
            :href="item.link"
            class="kage-rail__link"
            :class="{ 'is-active': route.path === item.link }"
          >
            <span class="kage-rail__bullet" aria-hidden="true"></span>
            <span class="kage-rail__text">{{ item.text }}</span>
          </a>
        </li>
      </ul>
    </div>
  </aside>
</template>
