<script setup>
import { ref, onMounted } from 'vue'
import { api } from '../api'

const tasks = ref([])
const selectedTask = ref(null)
const isNewTask = ref(false)
const taskResult = ref('')
// the task's `data_path` file content — separate from selectedTask since it
// loads async (a second request) and isn't part of the task JSON itself
const taskFile = ref('')
const taskFileResult = ref('')

async function refresh() {
  tasks.value = (await api('/scheduler/tasks')).body.tasks || []
  const stillThere = tasks.value.find(t => t.id === selectedTask.value?.id)
  if (stillThere) selectedTask.value = stillThere
}

async function loadTaskFile(path) {
  taskFileResult.value = ''
  taskFile.value = path ? (await api('/scheduler/task_file?path=' + encodeURIComponent(path))).body : ''
}

function selectTask(t) {
  isNewTask.value = false
  taskResult.value = ''
  selectedTask.value = { ...t }
  loadTaskFile(t.data_path)
}
function newTask() {
  isNewTask.value = true
  taskResult.value = ''
  taskFile.value = ''
  taskFileResult.value = ''
  selectedTask.value = { cron: '0 9 * * *', data_path: '/workspace/tasks/', description: '', enabled: true }
}
async function saveTaskFile() {
  const { body } = await api('/scheduler/task_file?path=' + encodeURIComponent(selectedTask.value.data_path), {
    method: 'PUT', headers: { 'Content-Type': 'text/plain' }, body: taskFile.value,
  })
  taskFileResult.value = typeof body === 'string' ? body : JSON.stringify(body)
}
async function saveTask() {
  const t = selectedTask.value
  const { body } = isNewTask.value
    ? await api('/scheduler/tasks', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ cron: t.cron, data_path: t.data_path, description: t.description }),
      })
    : await api('/scheduler/tasks/' + encodeURIComponent(t.id), {
        method: 'PUT', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ cron: t.cron, data_path: t.data_path, description: t.description, enabled: t.enabled }),
      })
  taskResult.value = typeof body === 'string' ? body : JSON.stringify(body)
  isNewTask.value = false
  await refresh()
}
async function deleteTask() {
  if (!selectedTask.value?.id) return
  await api('/scheduler/tasks/' + encodeURIComponent(selectedTask.value.id), { method: 'DELETE' })
  selectedTask.value = null
  await refresh()
}
onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>
      Scheduler
      <span class="row" style="margin:0; gap:0.4rem;">
        <button class="secondary" @click="newTask">New</button>
        <button class="secondary" @click="refresh">Refresh</button>
      </span>
    </h2>
    <p class="hint">One file per task under `scheduler/&lt;id&gt;.json` — the agent can set these up itself
      via chat (`schedule_task`/`update_task`/`delete_task`), or add/edit them here. `cron` is 5-field
      (minute hour day month weekday, UTC): `*`, a number, a comma list, or `*/step`.
      `data_path` is a guest-absolute path the agent read_files for its instructions when woken.</p>
    <div class="hint" v-if="tasks.length === 0 && !selectedTask">no scheduled tasks yet — click New to add one</div>
    <div class="split-body" v-else>
      <div class="split-list">
        <div class="hint" v-if="tasks.length === 0">no scheduled tasks yet</div>
        <div v-for="t in tasks" :key="t.id" class="split-item"
             :class="{ active: selectedTask && selectedTask.id === t.id }"
             @click="selectTask(t)">
          {{ t.enabled ? '⏰' : '⏸️' }} {{ t.cron }} — {{ t.description || t.data_path }}
        </div>
      </div>
      <div class="split-content">
        <div v-if="selectedTask">
          <div class="row">
            <input type="text" v-model="selectedTask.cron" placeholder="0 9 * * *">
            <label v-if="!isNewTask" class="row" style="margin:0"><input type="checkbox" v-model="selectedTask.enabled"> enabled</label>
          </div>
          <div class="row">
            <input type="text" v-model="selectedTask.data_path" placeholder="/workspace/tasks/x.md">
            <button class="secondary" @click="loadTaskFile(selectedTask.data_path)">Load file</button>
          </div>
          <input type="text" v-model="selectedTask.description" placeholder="one-line description">
          <div class="hint" v-if="!isNewTask">
            id: {{ selectedTask.id }} — last run: {{ selectedTask.last_run ? new Date(selectedTask.last_run * 1000).toLocaleString() : 'never' }}
          </div>
          <div class="row">
            <button @click="saveTask">Save</button>
            <button class="secondary" @click="deleteTask" v-if="!isNewTask">Delete</button>
          </div>
          <pre v-if="taskResult">{{ taskResult }}</pre>

          <div class="hint" style="margin-top:1rem">data_path 內容 — 直接編輯後按 Save file</div>
          <textarea v-model="taskFile" style="min-height:12rem"></textarea>
          <div class="row"><button @click="saveTaskFile" :disabled="!selectedTask.data_path">Save file</button></div>
          <pre v-if="taskFileResult">{{ taskFileResult }}</pre>
        </div>
        <div class="hint" v-else>select a task on the left, or click New</div>
      </div>
    </div>
  </div>
</template>
