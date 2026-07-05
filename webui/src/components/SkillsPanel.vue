<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const skills = ref([])
const selectedSkill = ref(null)
const isNewSkill = ref(false)
const skillResult = ref('')

async function refresh() {
  skills.value = (await api('/skills')).body.skills || []
  const stillThere = skills.value.find(s => s.name === selectedSkill.value?.name)
  if (stillThere) selectedSkill.value = stillThere
}
function selectSkill(s) {
  isNewSkill.value = false
  skillResult.value = ''
  selectedSkill.value = { ...s }
}
function newSkill() {
  isNewSkill.value = true
  skillResult.value = ''
  selectedSkill.value = { name: '', description: '', body: '' }
}
async function save() {
  const { body } = await api('/skills', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(selectedSkill.value),
  })
  skillResult.value = typeof body === 'string' ? body : JSON.stringify(body)
  isNewSkill.value = false
  await refresh()
}
async function remove() {
  if (!selectedSkill.value) return
  await api('/skills/' + encodeURIComponent(selectedSkill.value.name), { method: 'DELETE' })
  selectedSkill.value = null
  await refresh()
}
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>
      Skills
      <span class="row" style="margin:0; gap:0.4rem;">
        <button class="secondary" @click="newSkill">New</button>
        <button class="secondary" @click="refresh">Refresh</button>
      </span>
    </h2>
    <div class="hint" v-if="skills.length === 0 && !selectedSkill">no skills saved yet — the agent saves these itself via `save_skill`, or add one here</div>
    <div class="split-body" v-else>
      <div class="split-list">
        <div class="hint" v-if="skills.length === 0">no skills saved yet</div>
        <div v-for="s in skills" :key="s.name" class="split-item"
             :class="{ active: selectedSkill && selectedSkill.name === s.name }"
             @click="selectSkill(s)">
          🧠 {{ s.name }}
        </div>
      </div>
      <div class="split-content">
        <div v-if="selectedSkill">
          <div class="row">
            <input type="text" v-model="selectedSkill.name" placeholder="name" :disabled="!isNewSkill">
            <input type="text" v-model="selectedSkill.description" placeholder="one-line description">
          </div>
          <textarea v-model="selectedSkill.body" style="min-height:16rem" placeholder="full step-by-step procedure"></textarea>
          <div class="row">
            <button @click="save">Save</button>
            <button class="secondary" @click="remove" v-if="!isNewSkill">Delete</button>
          </div>
          <pre v-if="skillResult">{{ skillResult }}</pre>
        </div>
        <div class="hint" v-else>select a skill on the left, or click New</div>
      </div>
    </div>
  </div>
</template>
