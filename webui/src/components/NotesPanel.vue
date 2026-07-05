<script setup>
import { ref, computed, onMounted } from 'vue'
import { api } from '../api'

const notes = ref([])
const selectedNote = ref(null)
const expandedFolders = ref({})

// flattens notes[] (flat list of "a/b/c.md"-style relative paths) into a
// tree, then back into a flat, indented row list respecting which folders
// are currently collapsed — a hierarchical, expandable view of
// memory/notes/ without needing a recursive component
const noteRows = computed(() => {
  const root = { children: {} }
  for (const n of notes.value) {
    const parts = n.path.split('/')
    let cur = root
    let prefix = ''
    parts.forEach((part, i) => {
      prefix = prefix ? prefix + '/' + part : part
      const isLast = i === parts.length - 1
      if (isLast) {
        cur.children[part] = { isDir: false, name: part, fullPath: prefix, note: n }
      } else {
        if (!cur.children[part]) cur.children[part] = { isDir: true, name: part, fullPath: prefix, children: {} }
        cur = cur.children[part]
      }
    })
  }
  const rows = []
  const walk = (node, depth) => {
    for (const key of Object.keys(node.children).sort()) {
      const child = node.children[key]
      rows.push({ ...child, depth })
      if (child.isDir && expandedFolders.value[child.fullPath] !== false) walk(child, depth + 1)
    }
  }
  walk(root, 0)
  return rows
})

function toggleFolder(path) {
  expandedFolders.value = { ...expandedFolders.value, [path]: expandedFolders.value[path] === false }
}

async function refresh() {
  notes.value = (await api('/memory/notes')).body.notes || []
  const stillThere = notes.value.find(n => n.path === selectedNote.value?.path)
  selectedNote.value = stillThere || notes.value[0] || null
}
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>Memory notes <button class="secondary" @click="refresh">Refresh</button></h2>
    <div class="hint" v-if="notes.length === 0">no notes yet</div>
    <div class="split-body" v-else>
      <div class="split-list">
        <div v-for="row in noteRows" :key="row.fullPath" class="split-item"
             :class="{ dir: row.isDir, active: !row.isDir && selectedNote && selectedNote.path === row.note.path }"
             :style="{ paddingLeft: (row.depth + 0.6) + 'rem' }"
             @click="row.isDir ? toggleFolder(row.fullPath) : (selectedNote = row.note)">
          <template v-if="row.isDir">{{ expandedFolders[row.fullPath] === false ? '▸' : '▾' }} 📁 {{ row.name }}</template>
          <template v-else>📄 {{ row.name }}</template>
        </div>
      </div>
      <div class="split-content">
        <div class="path" v-if="selectedNote">{{ selectedNote.path }}</div>
        <pre v-if="selectedNote">{{ selectedNote.content }}</pre>
        <div class="hint" v-else>select a note on the left</div>
      </div>
    </div>
  </div>
</template>
