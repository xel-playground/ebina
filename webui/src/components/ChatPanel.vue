<script setup>
import { ref, nextTick, onMounted, onUnmounted } from 'vue'
import { api, eventSource, token } from '../api'
import { marked } from 'marked'
import DOMPurify from 'dompurify'

// `<img>`/`<a>` tags can't set an Authorization header, same reasoning as
// `eventSource()` — the gateway's `auth` middleware accepts the token as a
// `?token=` query param on every route specifically for this
function attachmentUrl(path) {
  return '/api/attachment?path=' + encodeURIComponent(path) + '&token=' + encodeURIComponent(token())
}

function isImagePath(path) {
  return /\.(png|jpe?g|gif|webp)$/i.test(path)
}

// agent replies can themselves contain content the agent read off the open
// internet (http_get/search_web results it summarized) — sanitize before
// v-html, same "untrusted content" treatment agent_loop.rs's system prompt
// already tells the agent to apply when reading fetched pages
function renderMarkdown(text) {
  return DOMPurify.sanitize(marked.parse(text, { breaks: true }))
}

const chatMessages = ref([])
const msg = ref('')
const sending = ref(false)
const thinkingText = ref('')
const lastTrace = ref('')
const showTrace = ref(false)
const contextTokens = ref(null)
const chatLog = ref(null)
let thinkingSource = null
let busyPoll = null
// Bumped every time a new run starts (either `sendMessage` or
// `reconcileWithServer` claiming one) — every async completion path checks
// its own captured token against this before writing `lastTrace`/
// `chatMessages`. Without this, two overlapping "wait for completion"
// chains (e.g. a stale `reconcileWithServer` poll left over from an earlier
// dropped fetch, still ticking in the background, plus a brand new
// `sendMessage` the user just sent) can both eventually resolve and
// clobber each other — whichever happens to finish *last* wins, with no
// relation to which run it actually belongs to. That's exactly how a
// finished run's trace ended up attached to an unrelated, already-displayed
// reply. A stale token means "a newer run has since started or finished
// instead — don't touch shared state, just stop."
let runToken = 0

async function scrollToBottom() {
  await nextTick()
  if (chatLog.value) chatLog.value.scrollTop = chatLog.value.scrollHeight
}

async function refreshSession() {
  const { body } = await api('/session')
  chatMessages.value = (body.turns || []).map(t => ({
    role: t.role === 'user' ? 'user' : (t.role === 'system' ? 'system' : 'agent'),
    text: t.content,
    attachments: t.attachments || [],
    time: new Date(t.ts * 1000).toLocaleTimeString(),
    raw: false,
  }))
  contextTokens.value = body.context_tokens ?? null
  scrollToBottom()
}

async function resetSession() {
  // same reasoning as `sendMessage`'s own guard — the button already hides
  // behind `:disabled="sending"`, this is the belt-and-suspenders backstop
  // for any other path that might call this directly
  if (sending.value) return
  await api('/session/reset', { method: 'POST' })
  await refreshSession()
}

async function compactSession() {
  if (sending.value) return
  await api('/session/compact', { method: 'POST' })
  await refreshSession()
}

async function abortRun() {
  await api('/abort', { method: 'POST' })
}

// { file: File, name, previewUrl } — previewUrl is a local blob: URL for
// images only, revoked once the file's actually uploaded or removed
const pendingFiles = ref([])
const fileInput = ref(null)
const uploadError = ref('')

function pickFiles() {
  fileInput.value?.click()
}

function onFilesSelected(e) {
  for (const file of e.target.files) {
    pendingFiles.value.push({
      file,
      name: file.name,
      previewUrl: file.type.startsWith('image/') ? URL.createObjectURL(file) : null,
    })
  }
  e.target.value = '' // so picking the same file again still fires @change
}

function removePendingFile(i) {
  const f = pendingFiles.value[i]
  if (f.previewUrl) URL.revokeObjectURL(f.previewUrl)
  pendingFiles.value.splice(i, 1)
}

function readAsBase64(file) {
  return new Promise((resolve, reject) => {
    const reader = new FileReader()
    reader.onload = () => resolve(reader.result.split(',')[1] || '')
    reader.onerror = () => reject(reader.error)
    reader.readAsDataURL(file)
  })
}

// uploads whatever's pending, returns the agent_home-relative paths that
// made it — a failed upload (e.g. disk quota) is skipped rather than
// blocking the whole send, with `uploadError` surfacing what happened
async function uploadPendingFiles() {
  const paths = []
  const errors = []
  for (const f of pendingFiles.value) {
    try {
      const data_base64 = await readAsBase64(f.file)
      const { body } = await api('/upload', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ filename: f.name, data_base64 }),
      })
      if (body.ok) paths.push(body.path)
      else errors.push(`${f.name}: ${body.error}`)
    } catch (e) {
      errors.push(`${f.name}: ${e}`)
    }
    if (f.previewUrl) URL.revokeObjectURL(f.previewUrl)
  }
  uploadError.value = errors.join('; ')
  pendingFiles.value = []
  return paths
}

async function sendMessage() {
  // the Send button already hides itself behind `v-if="!sending"` (Stop
  // shows instead), but the textarea's `@keydown.enter` handler calls this
  // directly with no such guard — hitting Enter mid-run would otherwise
  // fire a second overlapping `/api/message` while the first is still
  // in-flight, racing over the same session
  if (sending.value) return
  const text = msg.value.trim()
  if (!text && !pendingFiles.value.length) return
  const myToken = ++runToken
  if (busyPoll) { clearInterval(busyPoll); busyPoll = null }
  const attachments = await uploadPendingFiles()
  if (myToken !== runToken) return // superseded while uploads were in flight
  chatMessages.value.push({ role: 'user', text, attachments, time: new Date().toLocaleTimeString() })
  msg.value = ''
  sending.value = true
  thinkingText.value = ''
  lastTrace.value = ''
  showTrace.value = false
  scrollToBottom()
  try {
    await api('/message', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ text, attachments }),
    })
    // grab the trace directly rather than trusting the live SSE stream
    // caught up in time — it only emits on a 200ms poll tick, and a fast
    // run (a plain reply, no actions) can finish before that tick ever
    // fires, so the live view could otherwise show nothing at all
    const { body } = await api('/thinking/snapshot')
    if (myToken !== runToken) return // superseded — see `runToken` docs above
    lastTrace.value = body.text || ''
  } catch {
    // this fetch can be open for the agent's *entire* run (a multi-turn
    // ssh_exec chain easily takes minutes) — if it drops client-side for
    // any reason (proxy hiccup, tab throttling, network blip) partway
    // through, the backend doesn't know or care and keeps working
    // regardless. Falling back to the same busy-poll `reconcileWithServer`
    // uses means this never gets permanently stuck showing "thinking…"
    // forever just because *this one fetch* died — it'll pick the actual
    // completion back up once the backend really is done.
    await reconcileWithServer(myToken)
    return
  }
  if (myToken !== runToken) return
  sending.value = false
  await refreshSession()
}

async function finishRun(token) {
  const snap = await api('/thinking/snapshot')
  if (token !== runToken) return // superseded — see `runToken` docs above
  lastTrace.value = snap.body.text || ''
  sending.value = false
  await refreshSession()
}

// The only other way `sending` ever becomes true is `sendMessage` itself
// awaiting its own `/api/message` call — which only covers a run *this*
// page load started, over a connection that stays open the whole time. A
// run kicked off before this page loaded, still going after a reload/HMR
// mid-run, or one `sendMessage`'s own fetch lost track of because *that*
// connection specifically dropped (see the `catch` above) is otherwise
// invisible: `run_lock` is held server-side for the run's entire duration
// regardless of whether any particular browser tab is still watching.
// `GET /api/status`'s `busy` field is exactly that check — poll it until
// it clears, then wrap up the same way a normal `sendMessage` would.
//
// `token` is passed in when called from `sendMessage`'s own `catch` (that
// run already claimed a token); called with none (e.g. from `onMounted`) it
// claims a fresh one itself, since it's the one initiating this particular
// watch.
async function reconcileWithServer(token) {
  const myToken = token ?? ++runToken
  const { body } = await api('/status')
  if (myToken !== runToken) return
  if (!body.busy) {
    await finishRun(myToken)
    return
  }
  sending.value = true
  if (busyPoll) clearInterval(busyPoll)
  busyPoll = setInterval(async () => {
    if (myToken !== runToken) {
      clearInterval(busyPoll)
      busyPoll = null
      return
    }
    const { body } = await api('/status')
    if (body.busy) return
    clearInterval(busyPoll)
    busyPoll = null
    await finishRun(myToken)
  }, 1000)
}

onMounted(() => {
  refreshSession()
  reconcileWithServer()
  thinkingSource = eventSource('/thinking')
  // keep the live trace pinned to the bottom as it streams in, same as a
  // new chat message arriving — otherwise a long trace scrolls the visible
  // area past whatever the human was reading and just keeps growing offscreen
  thinkingSource.onmessage = (e) => { thinkingText.value = e.data; scrollToBottom() }
})
onUnmounted(() => {
  thinkingSource?.close()
  if (busyPoll) clearInterval(busyPoll)
})

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
        <button class="secondary" @click="compactSession" v-if="chatMessages.length" :disabled="sending">Compact</button>
        <button class="secondary" @click="resetSession" v-if="chatMessages.length" :disabled="sending">Reset</button>
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
        <div v-if="m.attachments && m.attachments.length" class="row" style="margin-top:0.35rem; flex-wrap:wrap; gap:0.4rem;">
          <img
            v-for="path in m.attachments.filter(isImagePath)" :key="path"
            :src="attachmentUrl(path)" class="attachment-thumb"
          />
          <a
            v-for="path in m.attachments.filter(p => !isImagePath(p))" :key="path"
            :href="attachmentUrl(path)" target="_blank" class="attachment-chip"
          >📎 {{ path.split('/').pop() }}</a>
        </div>
        <div v-if="m.role === 'agent' && i === chatMessages.length - 1 && lastTrace && !sending" class="row" style="margin-top:0.25rem">
          <button class="link-btn" @click="showTrace = !showTrace">{{ showTrace ? 'hide trace' : 'view trace' }}</button>
        </div>
        <pre
          v-if="m.role === 'agent' && i === chatMessages.length - 1 && showTrace && lastTrace && !sending"
          class="raw-text thinking-text"
        >{{ lastTrace }}</pre>
      </div>
      <div class="bubble agent" v-if="sending">
        <div class="meta">agent · thinking…</div>
        <div v-if="thinkingText" class="thinking-text">{{ thinkingText }}</div>
        <div v-else>…</div>
      </div>
    </div>
    <div v-if="pendingFiles.length" class="row" style="flex-wrap:wrap; gap:0.4rem; margin-bottom:0.35rem;">
      <div v-for="(f, i) in pendingFiles" :key="i" class="attachment-chip">
        <img v-if="f.previewUrl" :src="f.previewUrl" class="attachment-thumb-sm" />
        <span v-else>📎</span>
        {{ f.name }}
        <button class="link-btn" @click="removePendingFile(i)">✕</button>
      </div>
    </div>
    <div v-if="uploadError" class="hint" style="color:var(--danger, #c0392b);">{{ uploadError }}</div>
    <div class="chat-input">
      <input ref="fileInput" type="file" multiple style="display:none" @change="onFilesSelected" />
      <button class="secondary" @click="pickFiles" title="attach file" style="align-self:flex-end">📎</button>
      <textarea v-model="msg" placeholder="say something to the agent" @keydown.enter.exact.prevent="sendMessage"></textarea>
      <button v-if="!sending" @click="sendMessage" style="align-self:flex-end">Send</button>
      <button v-else @click="abortRun" class="secondary" style="align-self:flex-end">Stop</button>
    </div>
  </div>
</template>
