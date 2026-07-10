<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { api } from '../api'

const status = ref(null)
const wakeResult = ref('')
const runs = ref([])
// `/api/scheduler/runs` already sorts newest first server-side
// (kernel/src/gateway.rs `get_scheduled_runs`) — reversing here undid that
const rows = computed(() => runs.value)
// the kernel only ever runs one trigger at a time (`run_lock` in
// gateway.rs) — `status.busy` is true for *any* in-flight run, not just a
// scheduler-driven one, so "Wake now" would otherwise queue behind
// whatever's already running (a long ssh_exec chain, another wake, ...)
// with no feedback until it suddenly unblocks
const busy = computed(() => status.value?.busy ?? false)
let interval = null

async function refresh() {
  status.value = (await api('/status')).body
  runs.value = (await api('/scheduler/runs')).body.runs || []
}
async function wakeNow() {
  if (busy.value) return
  const { body } = await api('/wake', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ type: 'manual', text: 'manual wake-up check' }),
  })
  wakeResult.value = JSON.stringify(body, null, 2)
  refresh()
}
// a failed run's `trigger` is stored right alongside its `outcome` in the
// same history entry — re-firing it is just re-POSTing that same trigger
// to `/api/wake`, no separate "resume" mechanism needed (there's no
// execution state left to resume anyway, see kernel/src/lib.rs's
// `RunOutcome.trapped` doc comment: a trapped run's in-memory progress is
// gone the moment the wasmtime Store drops)
function isFailed(r) {
  const summary = r.outcome?.result?.summary || ''
  return r.outcome?.ok === false || summary.startsWith('run aborted') || summary.startsWith('(run timed out')
}
async function rerun(entry) {
  if (busy.value) return
  await api('/wake', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(entry.trigger),
  })
  refresh()
}
onMounted(() => {
  refresh()
  // poll for `busy` clearing (or a scheduler-driven run landing) — same
  // 5s cadence as AppsPanel's pairing-code poll
  interval = setInterval(refresh, 5000)
})
onUnmounted(() => clearInterval(interval))
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
      <div class="row"><button @click="wakeNow" :disabled="busy">{{ busy ? '⏳ scheduler running…' : 'Wake now' }}</button></div>
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
      <div class="row" v-if="isFailed(r)" style="margin-top:0.5rem">
        <button class="secondary" @click="rerun(r)" :disabled="busy">{{ busy ? '⏳ running…' : 'Re-run' }}</button>
      </div>
    </div>
  </div>
</template>
