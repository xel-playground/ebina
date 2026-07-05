<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const status = ref(null)
const wakeResult = ref('')

async function refresh() { status.value = (await api('/status')).body }
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
    <h2>Scheduler <button class="secondary" @click="refresh">Refresh</button></h2>
    <div class="card">
      <p class="hint">No background scheduler loop yet (PROJECT.md Phase 4.5) — the agent only wakes when
        you send a chat message or hit "Wake now" below. This just shows the next wake time it last asked for.</p>
      <dl class="kv">
        <dt>next requested wake</dt>
        <dd>{{ status?.last_run?.sleep_until ? new Date(status.last_run.sleep_until * 1000).toLocaleString() : '—' }}</dd>
        <dt>last run</dt>
        <dd>{{ status?.last_run ? new Date(status.last_run.ts * 1000).toLocaleString() : '—' }}</dd>
      </dl>
      <div class="row"><button @click="wakeNow">Wake now</button></div>
      <pre v-if="wakeResult">{{ wakeResult }}</pre>
    </div>
  </div>
</template>
