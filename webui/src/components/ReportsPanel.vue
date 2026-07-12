<script setup>
import { ref, computed, onMounted } from 'vue'
import { api } from '../api'

const reports = ref([])
const selectedReport = ref(null)
const expandedFolders = ref({})

// same tree-flattening approach as NotesPanel — `date` is already
// "<kind>/<stem>"-shaped (e.g. "hourly/2026-07-12_0915"), so it slots into
// the same path-based grouping without any backend change
const reportRows = computed(() => {
  const root = { children: {} }
  for (const r of reports.value) {
    const parts = r.date.split('/')
    let cur = root
    let prefix = ''
    parts.forEach((part, i) => {
      prefix = prefix ? prefix + '/' + part : part
      const isLast = i === parts.length - 1
      if (isLast) {
        cur.children[part] = { isDir: false, name: part, fullPath: prefix, report: r }
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
  reports.value = (await api('/memory/reports')).body.reports || []
  const stillThere = reports.value.find(r => r.date === selectedReport.value?.date)
  selectedReport.value = stillThere || reports.value[0] || null
}
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>Maintenance reports <button class="secondary" @click="refresh">Refresh</button></h2>
    <div class="hint" v-if="reports.length === 0">no reports yet — one gets written per day by a daily_maintenance run</div>
    <div class="split-body" v-else>
      <div class="split-list">
        <div v-for="row in reportRows" :key="row.fullPath" class="split-item"
             :class="{ dir: row.isDir, active: !row.isDir && selectedReport && selectedReport.date === row.report.date }"
             :style="{ paddingLeft: (row.depth + 0.6) + 'rem' }"
             @click="row.isDir ? toggleFolder(row.fullPath) : (selectedReport = row.report)">
          <template v-if="row.isDir">{{ expandedFolders[row.fullPath] === false ? '▸' : '▾' }} 📁 {{ row.name }}</template>
          <template v-else>📄 {{ row.name }}</template>
        </div>
      </div>
      <div class="split-content">
        <div class="path" v-if="selectedReport">{{ selectedReport.date }}</div>
        <pre v-if="selectedReport">{{ selectedReport.content }}</pre>
        <div class="hint" v-else>select a report on the left</div>
      </div>
    </div>
  </div>
</template>
