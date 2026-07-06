<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const logs = ref([])
const expanded = ref({})

// request/response shapes differ per provider (openai/anthropic/ollama) —
// best-effort extraction across all three, not a strict schema
function replyText(entry) {
  const r = entry.response || {}
  return (
    r.choices?.[0]?.message?.content ||          // openai (Kimi/Moonshot)
    r.content?.[0]?.text ||                       // anthropic
    r.message?.content ||                         // ollama
    ''
  )
}
function tokens(entry) {
  const u = entry.response?.usage
  if (u) return `${u.prompt_tokens ?? u.input_tokens ?? '?'} in / ${u.completion_tokens ?? u.output_tokens ?? '?'} out`
  const r = entry.response || {}
  if (r.prompt_eval_count != null) return `${r.prompt_eval_count} in / ${r.eval_count ?? '?'} out`
  return '—'
}
// `source.session_key` is only set for a "message" trigger (webui or a
// specific Discord channel/DM — see agent_loop.rs `source_meta`); a
// cron/daily_maintenance/scheduled_task run just shows its trigger type.
// `source` itself is missing entirely on transcripts logged before this existed.
function sourceLabel(entry) {
  const s = entry.source
  if (!s) return '(unknown)'
  if (s.session_key) return `${s.channel} · ${s.session_key}`
  return s.trigger_type || '(unknown)'
}

async function refresh() { logs.value = (await api('/llm/logs')).body.logs || [] }
function toggle(i) { expanded.value = { ...expanded.value, [i]: !expanded.value[i] } }
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div>
    <h2>LLM logs <button class="secondary" @click="refresh">Refresh</button></h2>
    <p class="hint">Every `llm_call` request/response, most recent 100 — `logs/transcripts/` on disk.</p>
    <div class="hint" v-if="logs.length === 0">no llm_call logs yet</div>
    <div class="card" v-for="(entry, i) in logs" :key="entry.ts" style="margin-bottom:0.5rem">
      <div class="row" style="justify-content:space-between; cursor:pointer" @click="toggle(i)">
        <strong>{{ entry.request?.model || '(unknown model)' }} <span class="hint">— {{ sourceLabel(entry) }}</span></strong>
        <span class="hint">{{ tokens(entry) }} · {{ new Date(entry.ts * 1000).toLocaleString() }}</span>
      </div>
      <div v-if="!expanded[i]" style="white-space:pre-wrap">{{ replyText(entry).slice(0, 200) }}</div>
      <pre v-else>{{ JSON.stringify(entry, null, 2) }}</pre>
      <div class="row"><button class="secondary" @click="toggle(i)">{{ expanded[i] ? 'collapse' : 'view raw' }}</button></div>
    </div>
  </div>
</template>
