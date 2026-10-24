<script setup>
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { api } from '../api'

const status = ref(null)
const wakeResult = ref('')
// one entry per scheduled occurrence (`<job>@<slot>`), newest slot first —
// the event-sourced state from kernel/src/schedule_log.rs
const occurrences = ref([])
const taskNames = ref({})
const filter = ref('active')
const selectedId = ref(null)
const detail = ref(null)
// `status.busy` is true for *any* in-flight run, not just a scheduler-driven
// one, so "Wake now" would otherwise queue behind whatever's already
// running with no feedback until it suddenly unblocks
const busy = computed(() => status.value?.busy ?? false)
let interval = null

const STATE = {
  pending: { icon: '🕒', color: 'var(--muted)' },
  running: { icon: '▶️', color: 'var(--accent)' },
  succeeded: { icon: '✅', color: 'var(--ok)' },
  failed: { icon: '❌', color: 'var(--err)' },
  skipped: { icon: '⏭️', color: 'var(--muted)' },
}
const FILTERS = [
  ['active', 'Ran'],
  ['problems', 'Failed / pending'],
  ['all', 'All'],
]
// "skipped" is the hourly tier's no-op check and housekeeping is host-only
// bookkeeping — both are noise by default
const shown = computed(() => occurrences.value.filter(o => {
  if (filter.value === 'all') return true
  if (filter.value === 'problems') return ['failed', 'pending', 'running'].includes(o.state)
  return o.state !== 'skipped' && o.job !== 'housekeeping'
}))

function fmt(ts) {
  return ts ? new Date(ts * 1000).toLocaleString() : '—'
}
function jobLabel(job) {
  if (job.startsWith('task:')) {
    const id = job.slice(5)
    return taskNames.value[id] || `task ${id}`
  }
  return job
}

async function refresh() {
  const [s, occ, tasks] = await Promise.all([api('/status'), api('/scheduler/occurrences'), api('/scheduler/tasks')])
  status.value = s.body
  occurrences.value = occ.body.occurrences || []
  taskNames.value = Object.fromEntries((tasks.body.tasks || []).map(t => [t.id, t.description || t.data_path]))
  if (selectedId.value) {
    const current = occurrences.value.find(o => o.id === selectedId.value)
    // re-fetch the detail only while it can still change
    if (!detail.value || ['pending', 'running'].includes(current?.state) || current?.state !== detail.value.occurrence.state) {
      await loadDetail(selectedId.value)
    }
  }
}

async function loadDetail(id) {
  const { body } = await api('/scheduler/occurrences/' + encodeURIComponent(id))
  detail.value = body.ok ? body : null
}

async function select(o) {
  selectedId.value = o.id
  detail.value = null
  await loadDetail(o.id)
}

async function retry() {
  await api('/scheduler/occurrences/' + encodeURIComponent(selectedId.value) + '/retry', { method: 'POST' })
  await refresh()
  await loadDetail(selectedId.value)
}

async function wakeNow() {
  if (busy.value) return
  const { body } = await api('/wake', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ type: 'manual', text: 'manual wake-up check' }),
  })
  wakeResult.value = body?.result?.summary || JSON.stringify(body)
  refresh()
}

function eventText(e) {
  switch (e.event) {
    case 'scheduled': return 'scheduled'
    case 'started': return `attempt ${e.attempt} started`
    case 'succeeded': return `attempt ${e.attempt} succeeded`
    case 'failed': return `attempt ${e.attempt} failed${e.retry_at ? ` — retry at ${fmt(e.retry_at)}` : ' — giving up'}`
    case 'skipped': return `skipped — ${e.reason}`
    case 'retry_requested': return 'retry requested'
    default: return e.event
  }
}
function runSummary(r) {
  const o = r.outcome || {}
  return o.result?.summary || (o.ok === false ? o.error : '(no summary)')
}

onMounted(() => {
  refresh()
  interval = setInterval(refresh, 5000)
})
onUnmounted(() => clearInterval(interval))
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>
      Schedule history
      <span class="row" style="margin:0; gap:0.4rem;">
        <button @click="wakeNow" :disabled="busy">{{ busy ? '⏳ running…' : 'Wake now' }}</button>
        <button class="secondary" @click="refresh">Refresh</button>
      </span>
    </h2>
    <div class="sched-bar hint">
      <span>next requested wake: <b>{{ fmt(status?.last_run?.sleep_until) }}</b></span>
      <span>last run: <b>{{ fmt(status?.last_run?.ts) }}</b></span>
      <span v-if="wakeResult">wake: {{ wakeResult }}</span>
    </div>
    <div class="tab-row">
      <button v-for="[key, label] in FILTERS" :key="key" class="secondary"
              :class="{ active: filter === key }" @click="filter = key">{{ label }}</button>
    </div>
    <div class="split-body">
      <div class="split-list sched-list">
        <div class="hint" v-if="shown.length === 0">nothing here</div>
        <div v-for="o in shown" :key="o.id" class="split-item"
             :class="{ active: selectedId === o.id }" @click="select(o)">
          {{ STATE[o.state].icon }} {{ jobLabel(o.job) }}
          <div class="sched-meta">{{ fmt(o.slot) }}<span v-if="o.attempts > 1"> · {{ o.attempts }} attempts</span></div>
        </div>
      </div>
      <div class="split-content">
        <div v-if="detail">
          <div class="sched-head">
            <strong>{{ jobLabel(detail.occurrence.job) }}</strong>
            <span :style="{ color: STATE[detail.occurrence.state].color }">{{ detail.occurrence.state }}</span>
            <button v-if="detail.occurrence.state === 'failed'" class="secondary" @click="retry">Retry</button>
          </div>
          <dl class="kv card">
            <dt>occurrence</dt><dd>{{ detail.occurrence.id }}</dd>
            <dt>slot</dt><dd>{{ fmt(detail.occurrence.slot) }}</dd>
            <dt>attempts</dt><dd>{{ detail.occurrence.attempts }}</dd>
            <template v-if="detail.occurrence.state === 'pending' && detail.occurrence.attempts > 0">
              <dt>next retry</dt><dd>{{ fmt(detail.occurrence.not_before) }}</dd>
            </template>
            <template v-if="detail.occurrence.last_error">
              <dt>{{ detail.occurrence.state === 'skipped' ? 'reason' : 'last error' }}</dt>
              <dd>{{ detail.occurrence.last_error }}</dd>
            </template>
          </dl>

          <h3>Timeline</h3>
          <ul class="timeline">
            <li v-for="(e, i) in detail.events" :key="i">
              <span class="hint">{{ fmt(e.ts) }}</span>
              <span :class="{ 'err-text': e.event === 'failed' }">{{ eventText(e) }}</span>
              <div v-if="e.event === 'failed'" class="hint">{{ e.error }}</div>
            </li>
          </ul>

          <h3 v-if="detail.runs.length">Runs</h3>
          <div v-for="r in detail.runs" :key="r.file" class="card">
            <div class="sched-head">
              <strong>attempt {{ r.attempt ?? '?' }}</strong>
              <span class="hint">{{ fmt(r.ts) }}</span>
            </div>
            <div class="raw-text">{{ runSummary(r) }}</div>
            <details>
              <summary class="hint">trigger</summary>
              <pre>{{ JSON.stringify(r.trigger, null, 2) }}</pre>
            </details>
            <details v-if="r.outcome?.stdout">
              <summary class="hint">agent stdout (tail)</summary>
              <pre>{{ r.outcome.stdout }}</pre>
            </details>
          </div>
        </div>
        <div class="hint" v-else-if="selectedId">loading…</div>
        <div class="hint" v-else>Select an occurrence. Every due slot (maintenance tiers, the agent's own
          `sleep_until` wake, each Scheduler task) is scheduled exactly once; failed attempts retry after 15 min,
          up to 3 times, and a run cut off by a kernel restart counts as a failed attempt. "Wake now" is manual/dev
          use and isn't tracked here.</div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.sched-bar { display: flex; gap: 1.5rem; flex-wrap: wrap; }
.sched-bar b { color: var(--text); font-weight: 500; }
.sched-list { width: 280px; }
.sched-meta { font-size: 0.72rem; opacity: 0.75; margin-top: 0.15rem; }
.sched-head { display: flex; gap: 0.8rem; align-items: center; margin-bottom: 0.6rem; }
.sched-head button { margin-left: auto; }
.timeline { list-style: none; padding: 0; margin: 0 0 1rem; font-size: 0.85rem; }
.timeline li { padding: 0.35rem 0; border-bottom: 1px solid var(--border); display: grid; grid-template-columns: 11rem 1fr; gap: 0.2rem 0.8rem; }
.timeline li .hint { margin: 0; }
.timeline li > div { grid-column: 2; }
.err-text { color: var(--err); }
details { margin-top: 0.5rem; }
summary { cursor: pointer; }
h3 { font-size: 0.95rem; margin: 1rem 0 0.5rem; }
</style>
