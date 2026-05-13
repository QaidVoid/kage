<script setup lang="ts">
import { useData } from "vitepress";

const { page } = useData();
</script>

<template>
  <footer class="kage-footer">
    <div class="kage-footer__inner">
      <div class="kage-footer__cell kage-footer__cell--brand">
        kage <span class="kage-footer__sep">/</span> docs
      </div>
      <div class="kage-footer__cell kage-footer__cell--copy">
        MIT licensed
      </div>
      <div class="kage-footer__cell kage-footer__cell--meta" v-if="page.lastUpdated">
        last updated
        <time :datetime="new Date(page.lastUpdated).toISOString()">
          {{ new Date(page.lastUpdated).toISOString().slice(0, 10) }}
        </time>
      </div>
    </div>
  </footer>
</template>
