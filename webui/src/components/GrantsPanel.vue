<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const grants = ref([])
async function refresh() { grants.value = (await api('/grants')).body.grants || [] }
async function act(id, action) {
  await api('/grants/' + encodeURIComponent(id) + '/' + action, { method: 'POST' })
  await refresh()
}
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div>
    <h2>Grants <button class="secondary" @click="refresh">Refresh</button></h2>
    <div class="hint" v-if="grants.length === 0">nothing pending</div>
    <div class="card" v-for="g in grants" :key="g.id">
      <dl class="kv">
        <dt>kind</dt><dd>{{ g.kind }}</dd>
        <dt>method</dt><dd>{{ g.method }}</dd>
        <dt>url</dt><dd>{{ g.url }}</dd>
        <dt>domain</dt><dd>{{ g.domain }}</dd>
        <dt>status</dt><dd>{{ g.status }}</dd>
        <dt>requested</dt><dd>{{ new Date(g.created_at * 1000).toLocaleString() }}</dd>
      </dl>
      <div class="row" v-if="g.status === 'pending'">
        <button @click="act(g.id, 'approve')">Approve</button>
        <button class="secondary" @click="act(g.id, 'deny')">Deny</button>
      </div>
    </div>
  </div>
</template>
