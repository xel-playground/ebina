<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const status = ref(null)
async function refresh() { status.value = (await api('/status')).body }
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div>
    <h2>Status <button class="secondary" @click="refresh">Refresh</button></h2>
    <div class="card">
      <dl class="kv" v-if="status">
        <dt>budget date</dt><dd>{{ status.budget?.date ?? '—' }}</dd>
        <dt>tokens used today</dt><dd>{{ status.budget?.tokens_used ?? '—' }}</dd>
        <dt>last run</dt><dd>{{ status.last_run ? new Date(status.last_run.ts * 1000).toLocaleString() : '—' }}</dd>
        <dt>last summary</dt><dd>{{ status.last_run?.result?.summary ?? '—' }}</dd>
        <dt>next wake</dt><dd>{{ status.last_run?.sleep_until ? new Date(status.last_run.sleep_until * 1000).toLocaleString() : '—' }}</dd>
      </dl>
    </div>
  </div>
</template>
