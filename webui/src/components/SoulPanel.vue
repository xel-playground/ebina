<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const soulText = ref('')
const soulResult = ref('')

async function load() { soulText.value = (await api('/soul')).body }
async function save() {
  soulResult.value = (await api('/soul', {
    method: 'POST', headers: { 'Content-Type': 'text/plain' }, body: soulText.value,
  })).body
}
onMounted(load)
defineExpose({ load })
</script>

<template>
  <div>
    <h2>Soul <button class="secondary" @click="load">Load</button></h2>
    <p class="hint">Persona/identity, shown in full to the agent every turn — it can read/write this
      itself via read_file/write_file on /SOUL.md, same as you can here.</p>
    <div class="card">
      <textarea v-model="soulText" style="min-height:20rem" placeholder="# Who I am&#10;..."></textarea>
      <div class="row"><button @click="save">Save</button></div>
      <pre v-if="soulResult">{{ soulResult }}</pre>
    </div>
  </div>
</template>
