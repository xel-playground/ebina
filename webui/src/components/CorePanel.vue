<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

// No separate "current" entry — `commit_run` (autocommit.rs) commits the
// whole agent-home tree at the end of *every* run, so the live file on disk
// and the newest commit touching core.md are always identical by the time
// this panel can see either. Just default-select the newest commit; only
// fall back to the live file if core.md has never been committed at all
// (before the very first core_distillation run).
const commits = ref([])
const selectedHash = ref(null)
const displayedText = ref('')

async function refresh() {
  const { body } = await api('/core/history')
  commits.value = body.ok ? body.commits : []
  if (commits.value.length > 0) {
    await viewCommit(commits.value[0].hash)
  } else {
    selectedHash.value = null
    displayedText.value = (await api('/core')).body
  }
}

async function viewCommit(hash) {
  selectedHash.value = hash
  displayedText.value = (await api(`/core/history/${hash}`)).body
}

onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>Core <button class="secondary" @click="refresh">Refresh</button></h2>
    <p class="hint">
      `/core.md` — the distilled common-sense cache (adr/001-memory-v2.md §2), shown to the agent every
      wake alongside SOUL.md. Read-only here: only a once-a-day core_distillation run ever writes it,
      no exception path — not even from this panel.
    </p>
    <div class="split-body">
      <div class="split-list">
        <div v-if="commits.length === 0" class="hint">no history yet — core.md hasn't been committed</div>
        <div v-for="(c, i) in commits" :key="c.hash" class="split-item"
             :class="{ active: selectedHash === c.hash }" @click="viewCommit(c.hash)">
          {{ i === 0 ? '📄 latest — ' : '' }}{{ new Date(c.ts * 1000).toLocaleString() }}
        </div>
      </div>
      <div class="split-content">
        <div class="path" v-if="selectedHash">{{ selectedHash.slice(0, 10) }}</div>
        <pre v-if="displayedText">{{ displayedText }}</pre>
        <div class="hint" v-else>core.md is empty — nothing has survived a full distillation cycle yet</div>
      </div>
    </div>
  </div>
</template>
