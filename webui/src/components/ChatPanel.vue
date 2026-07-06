<script setup>
import { ref, nextTick, onMounted, onUnmounted } from 'vue'
import { api, eventSource } from '../api'
import { marked } from 'marked'
import DOMPurify from 'dompurify'

// agent replies can themselves contain content the agent read off the open
// internet (http_fetch/search_web results it summarized) — sanitize before
// v-html, same "untrusted content" treatment agent_loop.rs's system prompt
// already tells the agent to apply when reading fetched pages
function renderMarkdown(text) {
  return DOMPurify.sanitize(marked.parse(text, { breaks: true }))
}

const chatMessages = ref([])
const msg = ref('')
const sending = ref(false)
const thinkingText = ref('')
const contextTokens = ref(null)
const chatLog = ref(null)
let thinkingSource = null

async function scrollToBottom() {
  await nextTick()
  if (chatLog.value) chatLog.value.scrollTop = chatLog.value.scrollHeight
}

async function refreshSession() {
  const { body } = await api('/session')
  chatMessages.value = (body.turns || []).map(t => ({
    role: t.role === 'user' ? 'user' : (t.role === 'system' ? 'system' : 'agent'),
    text: t.content,
    time: new Date(t.ts * 1000).toLocaleTimeString(),
    raw: false,
  }))
  contextTokens.value = body.context_tokens ?? null
  scrollToBottom()
}

async function resetSession() {
  await api('/session/reset', { method: 'POST' })
  await refreshSession()
}

async function compactSession() {
  await api('/session/compact', { method: 'POST' })
  await refreshSession()
}

async function abortRun() {
  await api('/abort', { method: 'POST' })
}

async function sendMessage() {
  const text = msg.value.trim()
  if (!text) return
  chatMessages.value.push({ role: 'user', text, time: new Date().toLocaleTimeString() })
  msg.value = ''
  sending.value = true
  thinkingText.value = ''
  scrollToBottom()
  await api('/message', {
    method: 'POST', headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ text }),
  })
  sending.value = false
  await refreshSession()
}

onMounted(() => {
  refreshSession()
  thinkingSource = eventSource('/thinking')
  thinkingSource.onmessage = (e) => { thinkingText.value = e.data }
})
onUnmounted(() => { thinkingSource?.close() })

defineExpose({ refreshSession })
</script>

<template>
  <div class="chat-section">
    <h2>
      Chat
      <span class="hint" v-if="contextTokens != null" style="font-size:0.75rem; margin-left:0.5rem;">
        context: ~{{ contextTokens.toLocaleString() }} tokens
      </span>
      <span class="row" style="margin:0; gap:0.4rem;">
        <button class="secondary" @click="compactSession" v-if="chatMessages.length">Compact</button>
        <button class="secondary" @click="resetSession" v-if="chatMessages.length">Reset</button>
      </span>
    </h2>
    <div class="chat-log" ref="chatLog">
      <div v-for="(m, i) in chatMessages" :key="i" class="bubble" :class="m.role">
        <div class="meta" v-if="m.role !== 'system'">
          {{ m.role === 'user' ? 'you' : 'agent' }} · {{ m.time }}
          <button v-if="m.role === 'agent'" class="link-btn" @click="m.raw = !m.raw">{{ m.raw ? 'rendered' : 'raw' }}</button>
        </div>
        <div v-if="m.role === 'agent' && !m.raw" class="md" v-html="renderMarkdown(m.text)"></div>
        <pre v-else-if="m.role === 'agent'" class="raw-text">{{ m.text }}</pre>
        <div v-else>{{ m.text }}</div>
      </div>
      <div class="bubble agent" v-if="sending">
        <div class="meta">agent · thinking…</div>
        <div v-if="thinkingText" class="thinking-text">{{ thinkingText }}</div>
        <div v-else>…</div>
      </div>
    </div>
    <div class="chat-input">
      <textarea v-model="msg" placeholder="say something to the agent" @keydown.enter.exact.prevent="sendMessage"></textarea>
      <button v-if="!sending" @click="sendMessage" style="align-self:flex-end">Send</button>
      <button v-else @click="abortRun" class="secondary" style="align-self:flex-end">Stop</button>
    </div>
  </div>
</template>
