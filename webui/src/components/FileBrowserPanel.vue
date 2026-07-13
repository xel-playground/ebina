<script setup>
import { ref, computed, onMounted } from 'vue'
import { api } from '../api'

// Reused by both the Logs and Workspace panels — `root`/`apiBase`/`title`/
// `description` are the only things that differ between them, everything
// else (tree navigation, tail-100-lines file view) is identical.
const props = defineProps({
  root: { type: String, required: true }, // display label for the root breadcrumb, e.g. "logs"
  apiBase: { type: String, required: true }, // e.g. "/logs" or "/workspace"
  title: { type: String, required: true },
  description: { type: String, required: true },
})

// relative to `root`/ itself, '' is the root — never a leading/trailing '/'
const currentPath = ref('')
const entries = ref([])
const selected = ref(null) // { path, content, totalBytes, truncated } | null
const loadingFile = ref(false)

const crumbs = computed(() => {
  if (!currentPath.value) return []
  const parts = currentPath.value.split('/')
  return parts.map((name, i) => ({ name, path: parts.slice(0, i + 1).join('/') }))
})

function formatSize(bytes) {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}

async function loadDir(path) {
  currentPath.value = path
  selected.value = null
  const { body } = await api(`${props.apiBase}/tree?path=` + encodeURIComponent(path))
  entries.value = body.ok ? body.entries : []
}

async function openEntry(entry) {
  const childPath = currentPath.value ? `${currentPath.value}/${entry.name}` : entry.name
  if (entry.is_dir) {
    await loadDir(childPath)
    return
  }
  loadingFile.value = true
  const { body } = await api(`${props.apiBase}/file?path=` + encodeURIComponent(childPath))
  loadingFile.value = false
  selected.value = body.ok
    ? { path: childPath, content: body.content, totalBytes: body.total_bytes, truncated: body.truncated }
    : { path: childPath, content: `(failed to read: ${body.error})`, totalBytes: 0, truncated: false }
}

onMounted(() => loadDir(''))
defineExpose({ refresh: () => loadDir(currentPath.value) })
</script>

<template>
  <div class="split-section">
    <h2>{{ title }} <button class="secondary" @click="loadDir(currentPath)">Refresh</button></h2>
    <p class="hint">{{ description }}</p>
    <div class="breadcrumbs">
      <span class="crumb" @click="loadDir('')">{{ root }}</span>
      <template v-for="c in crumbs" :key="c.path">
        <span class="sep">/</span>
        <span class="crumb" @click="loadDir(c.path)">{{ c.name }}</span>
      </template>
    </div>
    <div class="split-body">
      <div class="split-list">
        <div v-if="entries.length === 0" class="hint">(empty)</div>
        <div v-for="e in entries" :key="e.name" class="split-item"
             :class="{ active: selected && selected.path.endsWith('/' + e.name) }"
             @click="openEntry(e)">
          {{ e.is_dir ? '📁' : '📄' }} {{ e.name }}
          <span class="hint" v-if="!e.is_dir"> — {{ formatSize(e.size) }}</span>
        </div>
      </div>
      <div class="split-content">
        <div v-if="loadingFile" class="hint">loading…</div>
        <template v-else-if="selected">
          <div class="path">{{ selected.path }} ({{ formatSize(selected.totalBytes) }})</div>
          <p class="hint" v-if="selected.truncated">showing the last 100 lines only</p>
          <pre>{{ selected.content }}</pre>
        </template>
        <div class="hint" v-else>select a file on the left</div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.breadcrumbs {
  padding: 0.3rem 0.6rem;
  font-family: monospace;
}
.crumb {
  cursor: pointer;
  text-decoration: underline;
}
.sep {
  margin: 0 0.3rem;
  opacity: 0.5;
}
</style>
