<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const configText = ref('')
const configResult = ref('')

async function load() { configText.value = (await api('/config')).body }
async function save() {
  configResult.value = (await api('/config', {
    method: 'POST', headers: { 'Content-Type': 'text/plain' }, body: configText.value,
  })).body
}
onMounted(load)
defineExpose({ load })
</script>

<template>
  <div>
    <h2>Config <button class="secondary" @click="load">Load</button></h2>
    <div class="card">
      <textarea v-model="configText" style="min-height:14rem"></textarea>
      <div class="row"><button @click="save">Save</button></div>
      <pre v-if="configResult">{{ configResult }}</pre>
    </div>
  </div>
</template>
