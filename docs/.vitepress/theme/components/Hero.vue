<script setup lang="ts">
import { useData } from "vitepress";

const { frontmatter } = useData();
</script>

<template>
  <section class="kage-hero">
    <div class="kage-hero__terminal">
      <div class="kage-hero__chrome">
        <span class="kage-hero__dot kage-hero__dot--red"></span>
        <span class="kage-hero__dot kage-hero__dot--amber"></span>
        <span class="kage-hero__dot kage-hero__dot--green"></span>
        <span class="kage-hero__chrome-title">kage</span>
      </div>
      <div class="kage-hero__body">
        <div class="kage-hero__line">
          <span class="kage-hero__prompt">$</span>
          <span class="kage-hero__cmd">kage</span>
          <span class="kage-hero__caret" aria-hidden="true"></span>
        </div>
        <div class="kage-hero__output">
          <h1 class="kage-hero__title">
            {{ frontmatter.hero?.title ?? "the keyboard agent" }}
          </h1>
          <p class="kage-hero__tagline">
            {{
              frontmatter.hero?.tagline ??
              "A minimal, extensible coding agent that lives in your terminal."
            }}
          </p>
        </div>
      </div>
    </div>
    <div class="kage-hero__actions" v-if="frontmatter.hero?.actions">
      <a
        v-for="action in frontmatter.hero.actions"
        :key="action.link"
        :href="action.link"
        :class="[
          'kage-hero__action',
          `kage-hero__action--${action.theme ?? 'brand'}`,
        ]"
      >
        {{ action.text }}
      </a>
    </div>
    <div class="kage-hero__features" v-if="frontmatter.features">
      <div
        v-for="feature in frontmatter.features"
        :key="feature.title"
        class="kage-hero__feature"
      >
        <div class="kage-hero__feature-tag">{{ feature.tag }}</div>
        <div class="kage-hero__feature-title">{{ feature.title }}</div>
        <p class="kage-hero__feature-text">{{ feature.text }}</p>
      </div>
    </div>
  </section>
</template>
