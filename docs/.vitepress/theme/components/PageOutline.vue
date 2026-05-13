<script setup lang="ts">
import { onMounted, onUnmounted, ref, watch } from "vue";
import { useRoute } from "vitepress";

interface Heading {
  id: string;
  text: string;
  level: number;
}

const route = useRoute();
const headings = ref<Heading[]>([]);
const active = ref<string>("");

function collect() {
  const nodes = document.querySelectorAll<HTMLHeadingElement>(
    ".kage-doc__content h2, .kage-doc__content h3",
  );
  headings.value = Array.from(nodes)
    .filter((n) => n.id)
    .map((n) => ({
      id: n.id,
      text: n.innerText.replace(/#$/, "").trim(),
      level: Number(n.tagName.slice(1)),
    }));
}

let observer: IntersectionObserver | null = null;

function observe() {
  observer?.disconnect();
  const nodes = document.querySelectorAll<HTMLHeadingElement>(
    ".kage-doc__content h2, .kage-doc__content h3",
  );
  observer = new IntersectionObserver(
    (entries) => {
      for (const entry of entries) {
        if (entry.isIntersecting) {
          active.value = (entry.target as HTMLElement).id;
        }
      }
    },
    { rootMargin: "-30% 0px -60% 0px", threshold: 0 },
  );
  nodes.forEach((n) => observer?.observe(n));
}

function refresh() {
  collect();
  observe();
}

onMounted(() => {
  refresh();
});

onUnmounted(() => {
  observer?.disconnect();
});

watch(
  () => route.path,
  async () => {
    await new Promise((r) => setTimeout(r, 50));
    refresh();
  },
);
</script>

<template>
  <aside class="kage-outline" v-if="headings.length > 0">
    <div class="kage-outline__label">on this page</div>
    <ul class="kage-outline__list">
      <li
        v-for="h in headings"
        :key="h.id"
        :class="['kage-outline__item', `is-level-${h.level}`, { 'is-active': active === h.id }]"
      >
        <a :href="`#${h.id}`" class="kage-outline__link">{{ h.text }}</a>
      </li>
    </ul>
  </aside>
</template>
