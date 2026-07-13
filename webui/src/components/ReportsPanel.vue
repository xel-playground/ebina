<script setup>
import { ref, computed, onMounted } from 'vue'
import { api } from '../api'

const reports = ref([])
const selectedReport = ref(null)
// 'summary' default per adr/001-memory-v2.md §9 TODO — the tier most worth
// checking first day-to-day (hourly is noisy, core changes once a day)
const activeTab = ref('summary')

// `date` is "<kind>/<stem>" (e.g. "hourly/2026-07-12_0915") — filter by the
// active tab's kind and sort newest first. `stem`'s "YYYY-MM-DD_HHMM" shape
// sorts correctly as a plain string, no date parsing needed.
const tabReports = computed(() =>
  reports.value
    .filter(r => r.kind === activeTab.value)
    .slice()
    .sort((a, b) => b.date.localeCompare(a.date))
)

function selectTab(kind) {
  activeTab.value = kind
  const stillThere = tabReports.value.find(r => r.date === selectedReport.value?.date)
  if (!stillThere) selectedReport.value = reports.value.filter(r => r.kind === kind).sort((a, b) => b.date.localeCompare(a.date))[0] || null
}

async function refresh() {
  reports.value = (await api('/reports')).body.reports || []
  const stillThere = reports.value.find(r => r.date === selectedReport.value?.date)
  selectedReport.value = stillThere || tabReports.value[0] || null
}
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>Maintenance reports <button class="secondary" @click="refresh">Refresh</button></h2>
    <div class="tab-row">
      <button class="secondary" :class="{ active: activeTab === 'hourly' }" @click="selectTab('hourly')">hourly</button>
      <button class="secondary" :class="{ active: activeTab === 'summary' }" @click="selectTab('summary')">summary</button>
      <button class="secondary" :class="{ active: activeTab === 'core' }" @click="selectTab('core')">core</button>
    </div>
    <div class="hint" v-if="reports.length === 0">no reports yet — one gets written per day by a daily_maintenance run</div>
    <div class="split-body" v-else>
      <div class="split-list">
        <div v-if="tabReports.length === 0" class="hint">no {{ activeTab }} reports yet</div>
        <div v-for="r in tabReports" :key="r.date" class="split-item"
             :class="{ active: selectedReport && selectedReport.date === r.date }"
             @click="selectedReport = r">
          📄 {{ r.date.split('/')[1] }}
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

<style scoped>
.tab-row {
  display: flex;
  gap: 0.4rem;
  padding: 0.4rem 0;
  margin-bottom: 0.4rem;
}
.tab-row button.active {
  font-weight: bold;
  text-decoration: underline;
}
</style>
