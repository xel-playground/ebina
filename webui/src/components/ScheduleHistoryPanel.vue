<script setup>
import { ref, computed, onMounted } from 'vue'
import { api } from '../api'

const status = ref(null)
const wakeResult = ref('')
const runs = ref([])
const rows = computed(() => [...runs.value].reverse()) // newest first

async function refresh() {
  status.value = (await api('/status')).body
  runs.value = (await api('/scheduler/runs')).body.runs || []
}
async function wakeNow() {
  const { body } = await api('/wake', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ type: 'manual', text: 'manual wake-up check' }),
  })
  wakeResult.value = JSON.stringify(body, null, 2)
  refresh()
}
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div>
    <h2>Schedule history <button class="secondary" @click="refresh">Refresh</button></h2>
    <div class="card">
      <p class="hint">Background loop ticks every 30s: fires a fresh (no chat history) `daily_maintenance`
        session once today's report is missing, a fresh `cron` session once the agent's own last
        `sleep_until` has passed, and one fresh session per matching Scheduler task. "Wake now" is manual/dev use.</p>
      <dl class="kv">
        <dt>next requested wake</dt>
        <dd>{{ status?.last_run?.sleep_until ? new Date(status.last_run.sleep_until * 1000).toLocaleString() : '—' }}</dd>
        <dt>last run</dt>
        <dd>{{ status?.last_run ? new Date(status.last_run.ts * 1000).toLocaleString() : '—' }}</dd>
      </dl>
      <div class="row"><button @click="wakeNow">Wake now</button></div>
      <pre v-if="wakeResult">{{ wakeResult }}</pre>
    </div>

    <h3>Scheduled run history</h3>
    <div class="hint" v-if="rows.length === 0">no scheduler-driven runs yet</div>
    <div class="card" v-for="r in rows" :key="r.ts" style="margin-bottom:0.5rem">
      <div class="row" style="justify-content:space-between">
        <strong>{{ r.trigger?.type }}</strong>
        <span class="hint">{{ new Date(r.ts * 1000).toLocaleString() }}</span>
      </div>
      <div>{{ r.outcome?.result?.summary || (r.outcome?.ok === false ? r.outcome.error : '(no summary)') }}</div>
    </div>
  </div>
</template>
