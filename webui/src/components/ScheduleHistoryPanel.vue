<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { api } from '../api'

const status = ref(null)
const wakeResult = ref('')
const runs = ref([])
// one entry per scheduled occurrence (`<job>@<slot>`), newest slot first —
// the event-sourced pending/running/succeeded/failed/skipped state from
// kernel/src/schedule_log.rs
const occurrences = ref([])
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
  occurrences.value = (await api('/scheduler/occurrences')).body.occurrences || []
}
const STATE_COLOR = { pending: 'var(--muted)', running: 'var(--accent)', succeeded: 'var(--ok)', failed: 'var(--err)', skipped: 'var(--muted)' }
function fmt(ts) {
  return ts ? new Date(ts * 1000).toLocaleString() : '—'
}
// a pending occurrence with attempts behind it is waiting out a retry backoff
function retryNote(o) {
  return o.state === 'pending' && o.attempts > 0 ? `retry at ${fmt(o.not_before)}` : ''
}
async function retryOccurrence(o) {
  await api('/scheduler/occurrences/' + encodeURIComponent(o.id) + '/retry', { method: 'POST' })
  refresh()
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
// gone the moment the agent process is killed)
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
      <p class="hint">Background loop ticks every 30s and schedules each due slot exactly once (maintenance tiers,
        the agent's own `sleep_until` wake, each matching Scheduler task) — duplicates across ticks or restarts are
        no-ops. Failed attempts retry after 15 min, up to 3 times; a run cut off by a kernel restart counts as a failed
        attempt. "Wake now" is manual/dev use and isn't tracked as an occurrence.</p>
      <dl class="kv">
        <dt>next requested wake</dt>
        <dd>{{ status?.last_run?.sleep_until ? new Date(status.last_run.sleep_until * 1000).toLocaleString() : '—' }}</dd>
        <dt>last run</dt>
        <dd>{{ status?.last_run ? new Date(status.last_run.ts * 1000).toLocaleString() : '—' }}</dd>
      </dl>
      <div class="row"><button @click="wakeNow" :disabled="busy">{{ busy ? '⏳ scheduler running…' : 'Wake now' }}</button></div>
      <pre v-if="wakeResult">{{ wakeResult }}</pre>
    </div>

    <h3>Occurrences</h3>
    <div class="hint" v-if="occurrences.length === 0">nothing scheduled yet</div>
    <div class="card" v-else style="margin-bottom:1rem; overflow-x:auto">
      <table class="occurrences">
        <thead><tr><th>job</th><th>slot</th><th>state</th><th>attempts</th><th>detail</th><th></th></tr></thead>
        <tbody>
          <tr v-for="o in occurrences" :key="o.id">
            <td><code>{{ o.job }}</code></td>
            <td>{{ fmt(o.slot) }}</td>
            <td :style="{ color: STATE_COLOR[o.state] }">{{ o.state }}</td>
            <td>{{ o.attempts }}</td>
            <td class="hint" style="margin:0">{{ retryNote(o) || o.last_error || '' }}</td>
            <td><button v-if="o.state === 'failed'" class="secondary" @click="retryOccurrence(o)">Retry</button></td>
          </tr>
        </tbody>
      </table>
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

<style scoped>
.occurrences { width: 100%; border-collapse: collapse; font-size: 0.82rem; }
.occurrences th { text-align: left; color: var(--muted); font-weight: 500; padding: 0.3rem 0.5rem; }
.occurrences td { padding: 0.3rem 0.5rem; border-top: 1px solid var(--border); vertical-align: top; }
</style>
