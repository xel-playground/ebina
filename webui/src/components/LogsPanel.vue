<script setup>
import { ref, onUnmounted } from 'vue'
import { eventSource } from '../api'

const logLines = ref([])
const logging = ref(false)
let source = null

function connect() {
  if (logging.value) return
  logging.value = true
  source = eventSource('/logs')
  source.onmessage = (e) => { logLines.value.push(e.data) }
  source.onerror = () => { logLines.value.push('[stream error]') }
}
onUnmounted(() => source?.close())
</script>

<template>
  <div>
    <h2>Live log <button class="secondary" @click="connect" :disabled="logging">{{ logging ? 'connected' : 'Connect' }}</button></h2>
    <pre>{{ logLines.join('\n') }}</pre>
  </div>
</template>
