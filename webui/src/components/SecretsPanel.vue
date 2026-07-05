<script setup>
import { ref } from 'vue'
import { api } from '../api'

const secretName = ref('')
const secretValue = ref('')
const secretResult = ref('')

async function save() {
  const { body } = await api('/secrets', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ name: secretName.value, value: secretValue.value }),
  })
  secretValue.value = ''
  secretResult.value = JSON.stringify(body, null, 2)
}
</script>

<template>
  <div>
    <h2>Secrets <small>write-only — no endpoint ever returns a value</small></h2>
    <div class="card">
      <div class="row">
        <input type="text" v-model="secretName" placeholder="name, e.g. ollama">
        <input type="password" v-model="secretValue" placeholder="value">
      </div>
      <button @click="save">Set secret</button>
      <pre v-if="secretResult">{{ secretResult }}</pre>
    </div>
  </div>
</template>
