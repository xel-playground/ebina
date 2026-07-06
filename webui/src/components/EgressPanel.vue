<script setup>
import { ref, computed, onMounted } from 'vue'
import { api } from '../api'

const entries = ref([])
const rows = computed(() => [...entries.value].reverse()) // newest first

// same shape as LlmLogsPanel's source — see agent_loop.rs `source_meta`
function sourceLabel(e) {
  const s = e.source
  if (!s) return '(unknown)'
  if (s.session_key) return `${s.channel} · ${s.session_key}`
  return s.trigger_type || '(unknown)'
}

async function refresh() { entries.value = (await api('/egress')).body.entries || [] }
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div>
    <h2>Egress log <button class="secondary" @click="refresh">Refresh</button></h2>
    <p class="hint">Every outbound `http_fetch` attempt, allowed or denied — full URL + byte count,
      nothing redacted here except vault secret values.</p>
    <div class="hint" v-if="rows.length === 0">no requests logged yet</div>
    <div class="card" v-for="(e, i) in rows" :key="i" :class="{ denied: e.error }" style="margin-bottom:0.5rem">
      <div class="row" style="justify-content:space-between">
        <strong>{{ e.method }} {{ e.domain }} <span class="hint">— {{ sourceLabel(e) }}</span></strong>
        <span class="hint">{{ new Date(e.ts * 1000).toLocaleString() }}</span>
      </div>
      <div style="word-break:break-all">{{ e.url }}</div>
      <div v-if="e.error" class="hint" style="color:var(--danger, #e55)">denied: {{ e.error }}</div>
      <div v-else class="hint">{{ e.bytes }} bytes</div>
    </div>
  </div>
</template>
