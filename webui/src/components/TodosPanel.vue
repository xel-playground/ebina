<script setup>
import { ref, computed, onMounted } from 'vue'
import { api } from '../api'

// one file per item under /workspace/memory/ — see kernel/src/workspace_memory.rs
const todos = ref([])
const today = ref('')
const selected = ref(null) // { name, expires, body, state?, isNew? }
const result = ref('')

const STATE = {
  active: { icon: '⏳', label: 'active', color: 'var(--ok)' },
  expired: { icon: '⚠️', label: 'expired', color: 'var(--err)' },
  one_shot: { icon: '1️⃣', label: 'one-shot', color: 'var(--accent)' },
}

const isNew = computed(() => selected.value?.isNew)

async function refresh() {
  const { body } = await api('/todos')
  todos.value = body.todos || []
  today.value = body.today || ''
  if (selected.value && !selected.value.isNew) {
    const fresh = todos.value.find(t => t.name === selected.value.name)
    selected.value = fresh ? { ...fresh } : null
  }
}

function select(t) {
  selected.value = { ...t }
  result.value = ''
}

function newTodo() {
  selected.value = { name: '', expires: '', body: '', isNew: true }
  result.value = ''
}

function fileName(name) {
  const n = name.trim()
  return n.endsWith('.md') ? n : n + '.md'
}

async function save() {
  const name = fileName(selected.value.name)
  const { body } = await api('/todos/' + encodeURIComponent(name), {
    method: 'PUT',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ body: selected.value.body, expires: selected.value.expires || null }),
  })
  if (!body.ok) {
    result.value = body.error
    return
  }
  selected.value = { name }
  result.value = 'saved'
  await refresh()
}

async function remove() {
  if (!confirm(`Delete ${selected.value.name}?`)) return
  const { body } = await api('/todos/' + encodeURIComponent(selected.value.name), { method: 'DELETE' })
  if (!body.ok) {
    result.value = body.error
    return
  }
  selected.value = null
  await refresh()
}

function deadline(t) {
  return t.expires ? `until ${t.expires}` : 'no deadline'
}

onMounted(refresh)
defineExpose({ refresh })
</script>

<template>
  <div class="split-section">
    <h2>
      Todos
      <span class="row" style="margin:0; gap:0.4rem;">
        <button class="secondary" @click="newTodo">New</button>
        <button class="secondary" @click="refresh">Refresh</button>
      </span>
    </h2>
    <p class="hint">Short-term items in `/workspace/memory/`, one per file — the agent writes these from chat, or add them
      here. With an expiry date an item stays until that day passes; without one it's one-shot. The hourly maintenance
      pass handles expired and one-shot items (reports the outcome to you), then they're deleted.</p>
    <div class="hint" v-if="todos.length === 0 && !selected">no todos — click New to add one</div>
    <div class="split-body" v-else>
      <div class="split-list">
        <div class="hint" v-if="todos.length === 0">no todos</div>
        <div v-for="t in todos" :key="t.name" class="split-item"
             :class="{ active: selected && !selected.isNew && selected.name === t.name }"
             @click="select(t)">
          {{ STATE[t.state].icon }} {{ t.name.replace(/\.md$/, '') }}
          <div class="todo-meta">{{ deadline(t) }}</div>
        </div>
      </div>
      <div class="split-content">
        <div v-if="selected">
          <div class="row" v-if="isNew">
            <input type="text" v-model="selected.name" placeholder="file name, e.g. cat-vaccine">
          </div>
          <div class="todo-head" v-else>
            <code>{{ selected.name }}</code>
            <span :style="{ color: STATE[selected.state].color }">{{ STATE[selected.state].label }}</span>
          </div>
          <div class="row todo-expires">
            <label>expires</label>
            <input type="date" v-model="selected.expires">
            <button class="secondary" @click="selected.expires = ''" :disabled="!selected.expires">No deadline</button>
          </div>
          <div class="hint" v-if="!selected.expires">one-shot — handled and deleted by the next hourly maintenance pass</div>
          <div class="hint" v-else-if="selected.expires < today">already expired — the next hourly pass wraps it up</div>
          <textarea v-model="selected.body" style="min-height:14rem" placeholder="what to remember / watch / do"></textarea>
          <div class="row">
            <button @click="save" :disabled="isNew && !selected.name.trim()">Save</button>
            <button class="secondary" @click="remove" v-if="!isNew">Delete</button>
          </div>
          <div class="hint" v-if="result">{{ result }}</div>
        </div>
        <div class="hint" v-else>select a todo on the left, or click New</div>
      </div>
    </div>
  </div>
</template>

<style scoped>
.todo-meta { font-size: 0.72rem; opacity: 0.75; margin-top: 0.15rem; }
.todo-head { display: flex; gap: 0.8rem; align-items: center; margin: 0.6rem 0; font-size: 0.85rem; }
.todo-expires { align-items: center; }
.todo-expires label { flex: 0 0 auto; color: var(--muted); font-size: 0.85rem; }
.todo-expires input[type=date] {
  flex: 0 0 auto; font-family: ui-monospace, monospace; font-size: 0.85rem; color-scheme: dark;
  border: 1px solid var(--border); border-radius: 8px; padding: 0.5rem 0.6rem; background: #10131a; color: var(--text);
}
.todo-expires button { flex: 0 0 auto; }
</style>
