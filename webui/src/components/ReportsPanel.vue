<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const reports = ref([])
const selectedReport = ref(null)

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
        <div v-for="r in reports" :key="r.date" class="split-item"
             :class="{ active: selectedReport && selectedReport.date === r.date }"
             @click="selectedReport = r">
          📄 {{ r.date }}
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
