<script setup>
defineProps({ section: String })
const emit = defineEmits(['go', 'logout'])

const items = [
  ['chat', '💬', 'Chat'],
  ['status', '📊', 'Status'],
  ['scheduler', '🗓️', 'Scheduler'],
  ['notes', '🗒️', 'Memory notes'],
  ['skills', '🧠', 'Skills'],
  ['grants', '✅', 'Grants'],
  ['report', '📄', 'Report'],
  ['config', '⚙️', 'Config'],
  ['secrets', '🔒', 'Secrets'],
  ['logs', '📡', 'Live log'],
]
</script>

<template>
  <div class="sidebar">
    <div class="brand"><span class="dot"></span> ebina</div>
    <div v-for="[key, icon, label] in items" :key="key"
         class="nav-item" :class="{ active: section === key }" @click="emit('go', key)">
      {{ icon }} {{ label }}
    </div>
    <div class="nav-spacer"></div>
    <div class="nav-item" @click="emit('logout')">🚪 Log out</div>
  </div>
</template>
