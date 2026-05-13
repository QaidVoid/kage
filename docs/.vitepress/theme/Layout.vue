<script setup lang="ts">
import { computed } from "vue";
import { Content, useData } from "vitepress";
import SideRail from "./components/SideRail.vue";
import PageOutline from "./components/PageOutline.vue";
import Hero from "./components/Hero.vue";
import SiteHeader from "./components/SiteHeader.vue";
import SiteFooter from "./components/SiteFooter.vue";

const { frontmatter } = useData();

const isHome = computed(() => frontmatter.value.layout === "home");
</script>

<template>
  <div class="kage-doc">
    <div class="kage-doc__grid-bg" aria-hidden="true"></div>
    <SiteHeader />
    <main class="kage-doc__main" :class="{ 'is-home': isHome }">
      <SideRail v-if="!isHome" />
      <article class="kage-doc__article">
        <Hero v-if="isHome" />
        <div class="kage-doc__content" :class="{ 'is-home': isHome }">
          <Content />
        </div>
      </article>
      <PageOutline v-if="!isHome" />
    </main>
    <SiteFooter />
  </div>
</template>
